//! Grok Build headless Messages output reduced to provider-neutral events.

use serde_json::Value;

use seatline_core::protocol::ErrorCode;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::turn::Usage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Init {
        api_key_source: String,
        model: String,
        cwd: String,
        tools: Vec<String>,
        skills: Vec<String>,
        all_mcp_disabled: bool,
    },
    MessageStart,
    TextDelta(String),
    /// A streaming content block started a forbidden client or server tool.
    ForbiddenTool,
    Assistant {
        text: String,
        activity: bool,
        forbidden_tool: bool,
    },
    ResultSuccess {
        text: String,
        usage: Usage,
    },
    ResultFailed(ErrorBody),
    Activity,
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

pub fn parse(line: &str) -> Result<Line, Malformed> {
    if line.trim().is_empty() {
        return Ok(Line::Ignored);
    }
    let event: Value = serde_json::from_str(line).map_err(|_| Malformed)?;
    let kind = event.get("type").and_then(Value::as_str).ok_or(Malformed)?;
    Ok(match kind {
        "system" if event.get("subtype").and_then(Value::as_str) == Some("init") => {
            let api_key_source = required_string(&event, "apiKeySource")?;
            let model = required_string(&event, "model")?;
            let cwd = required_string(&event, "cwd")?;
            let tools = required_strings(&event, "tools")?;
            let skills = required_strings(&event, "skills")?;
            let servers = event
                .get("mcp_servers")
                .and_then(Value::as_array)
                .ok_or(Malformed)?;
            let all_mcp_disabled = servers
                .iter()
                .all(|server| server.get("status").and_then(Value::as_str) == Some("disabled"));
            Line::Init {
                api_key_source,
                model,
                cwd,
                tools,
                skills,
                all_mcp_disabled,
            }
        }
        "system" => Line::Activity,
        "assistant" => parse_assistant(&event)?,
        "user" => parse_user(&event)?,
        "result" => {
            let subtype = event
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let is_error = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if subtype == "success" && !is_error {
                Line::ResultSuccess {
                    text: event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    usage: parse_usage(event.get("usage")),
                }
            } else {
                let message = event
                    .get("errors")
                    .and_then(Value::as_array)
                    .and_then(|errors| errors.first())
                    .and_then(|error| {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .or_else(|| error.as_str())
                    })
                    .or_else(|| event.get("result").and_then(Value::as_str))
                    .unwrap_or_default();
                Line::ResultFailed(provider_failure(message))
            }
        }
        "stream_event" => parse_stream_event(&event)?,
        _ => Line::Ignored,
    })
}

fn parse_stream_event(event: &Value) -> Result<Line, Malformed> {
    let nested = event
        .get("event")
        .filter(|value| value.is_object())
        .ok_or(Malformed)?;
    Ok(
        match nested
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => Line::MessageStart,
            "content_block_start" => match nested
                .pointer("/content_block/type")
                .and_then(Value::as_str)
            {
                Some("tool_use" | "tool_result" | "server_tool_use" | "web_search_tool_result") => {
                    Line::ForbiddenTool
                }
                Some("text" | "thinking" | "redacted_thinking") | None => Line::Activity,
                Some(_) => return Err(Malformed),
            },
            "content_block_delta" => match nested.get("delta").and_then(Value::as_object) {
                Some(delta) if delta.get("type").and_then(Value::as_str) == Some("text_delta") => {
                    Line::TextDelta(
                        delta
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or(Malformed)?
                            .to_owned(),
                    )
                }
                _ => Line::Activity,
            },
            _ => Line::Activity,
        },
    )
}

/// Grok's `streaming-messages-json` reports the three Messages API prompt-side
/// buckets disjoint (`input_tokens` uncached, `cache_read_input_tokens`,
/// `cache_creation_input_tokens`), and falls back to all zeros when the turn's
/// usage ledger is absent or incomplete: read an all-zero object as unknown.
fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage.filter(|value| value.is_object()) else {
        return Usage::default();
    };
    let input = usage.get("input_tokens").and_then(Value::as_u64);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let cache_read = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
    let output = usage.get("output_tokens").and_then(Value::as_u64);
    let reasoning = usage.get("reasoning_tokens").and_then(Value::as_u64);
    let total_input = input
        .unwrap_or(0)
        .saturating_add(cache_creation.unwrap_or(0))
        .saturating_add(cache_read.unwrap_or(0));
    if total_input == 0 && output.unwrap_or(0) == 0 {
        return Usage::default();
    }
    Usage {
        input_tokens: (input.is_some() || cache_creation.is_some() || cache_read.is_some())
            .then_some(total_input),
        output_tokens: output,
        cached_input_tokens: cache_read,
        cache_write_input_tokens: cache_creation,
        reasoning_output_tokens: reasoning,
    }
}

fn required_string(event: &Value, key: &str) -> Result<String, Malformed> {
    event
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(Malformed)
}

fn required_strings(event: &Value, key: &str) -> Result<Vec<String>, Malformed> {
    event
        .get(key)
        .and_then(Value::as_array)
        .ok_or(Malformed)?
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or(Malformed))
        .collect()
}

fn parse_user(event: &Value) -> Result<Line, Malformed> {
    let Some(content) = event.pointer("/message/content") else {
        return Err(Malformed);
    };
    match content {
        Value::String(_) => Ok(Line::Activity),
        Value::Array(blocks) => {
            let forbidden_tool = blocks.iter().any(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("tool_result")
                )
            });
            if forbidden_tool {
                Ok(Line::Assistant {
                    text: String::new(),
                    activity: true,
                    forbidden_tool: true,
                })
            } else {
                Ok(Line::Activity)
            }
        }
        _ => Err(Malformed),
    }
}

fn parse_assistant(event: &Value) -> Result<Line, Malformed> {
    let blocks = event
        .pointer("/message/content")
        .and_then(Value::as_array)
        .ok_or(Malformed)?;
    let mut text = String::new();
    let mut activity = false;
    let mut forbidden_tool = false;

    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).ok_or(Malformed)?;
        match kind {
            "text" => {
                activity = true;
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            "thinking" | "redacted_thinking" => activity = true,
            // The shipped Grok integration is deliberately text-only.
            // Any tool-bearing block, including hosted/server tools, is a
            // boundary violation regardless of its particular tool name.
            "tool_use" | "tool_result" | "server_tool_use" | "web_search_tool_result" => {
                activity = true;
                forbidden_tool = true;
            }
            // Unknown blocks fail closed: Grok adds new execution-bearing
            // block types over time, and the adapter must not silently bless one.
            _ => return Err(Malformed),
        }
    }

    Ok(Line::Assistant {
        text,
        activity,
        forbidden_tool,
    })
}

const AUTH_REJECTED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderNotAuthenticated,
    reason: "AUTH_REJECTED",
    retryable: false,
};

const RATE_LIMITED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_RATE_LIMITED",
    retryable: true,
};

const UNAVAILABLE: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_UNAVAILABLE",
    retryable: true,
};

pub fn authentication_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "not authenticated",
        "authentication failed",
        "run grok login",
        "not signed in",
        "please sign in",
        "login required",
        "cached credential",
        "token expired",
        "401 unauthorized",
        "http 401",
        "status 401",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

pub fn provider_failure(message: &str) -> ErrorBody {
    let lower = message.to_ascii_lowercase();
    if authentication_failure(message) {
        AUTH_REJECTED
    } else if [
        "rate limit",
        "quota exceeded",
        "resource_exhausted",
        "too many requests",
        "http 429",
        "status 429",
        "code 429",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
    {
        RATE_LIMITED
    } else {
        UNAVAILABLE
    }
}

/// Whether Grok's pre-init stderr says the installed CLI rejected
/// `--reasoning-effort` / `--effort` as an unknown flag or invalid effort value.
pub fn names_unsupported_effort_option(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let names_flag = lower.contains("--reasoning-effort") || lower.contains("--effort");
        let unknown_flag = lower.contains("unexpected argument")
            || lower.contains("unknown option")
            || lower.contains("unrecognized option")
            || lower.contains("invalid value");
        names_flag && unknown_flag
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shipped_messages_shapes_strictly() {
        let init = parse(r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"oauth","model":"grok-4.6","cwd":"/tmp/x","tools":[],"skills":[],"mcp_servers":[]}"#).unwrap();
        assert!(matches!(
            init,
            Line::Init {
                api_key_source,
                model,
                tools,
                all_mcp_disabled: true,
                ..
            } if api_key_source == "oauth" && model == "grok-4.6" && tools.is_empty()
        ));

        assert!(parse(r#"{"type":"system","subtype":"init","apiKeySource":"oauth","model":"grok-4.6","cwd":"/tmp/x","skills":[],"mcp_servers":[]}"#).is_err());
        assert!(parse(r#"{"type":"system","subtype":"init","apiKeySource":"oauth","model":"grok-4.6","cwd":"/tmp/x","tools":[],"skills":[]}"#).is_err());

        let active = parse(r#"{"type":"system","subtype":"init","apiKeySource":"oauth","model":"grok-4.6","cwd":"/tmp/x","tools":[],"skills":[],"mcp_servers":[{"name":"x","status":"connected"}]}"#).unwrap();
        assert!(matches!(
            active,
            Line::Init {
                all_mcp_disabled: false,
                ..
            }
        ));
    }

    #[test]
    fn user_strings_are_activity_but_tool_results_fail_closed() {
        assert_eq!(
            parse(r#"{"type":"user","message":{"content":"hello"}}"#).unwrap(),
            Line::Activity
        );
        let tool = parse(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"done"}]}}"#).unwrap();
        assert!(matches!(
            tool,
            Line::Assistant {
                forbidden_tool: true,
                ..
            }
        ));
    }

    #[test]
    fn reasoning_is_activity_and_client_tools_fail_closed() {
        let thinking = parse(r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking","data":"x"}]}}"#).unwrap();
        assert!(matches!(
            thinking,
            Line::Assistant {
                activity: true,
                forbidden_tool: false,
                ..
            }
        ));

        let line = parse(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"run_terminal_cmd","input":{}}]}}"#).unwrap();
        assert!(matches!(
            line,
            Line::Assistant {
                forbidden_tool: true,
                ..
            }
        ));
    }

    #[test]
    fn partial_frames_stream_text_and_reject_forbidden_tools() {
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"message_start"}}"#).unwrap(),
            Line::MessageStart
        );
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi "}}}"#).unwrap(),
            Line::TextDelta("hi ".to_owned())
        );
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"..."}}}"#).unwrap(),
            Line::Activity
        );
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#).unwrap(),
            Line::Activity
        );
        for block in [
            "tool_use",
            "tool_result",
            "server_tool_use",
            "web_search_tool_result",
        ] {
            let line = format!(
                r#"{{"type":"stream_event","event":{{"type":"content_block_start","content_block":{{"type":"{block}"}}}}}}"#
            );
            assert_eq!(parse(&line).unwrap(), Line::ForbiddenTool, "{block}");
        }
        assert!(
            parse(r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"future_exec_block"}}}"#).is_err()
        );
        assert!(parse(r#"{"type":"stream_event"}"#).is_err());
        assert!(
            parse(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":7}}}"#).is_err()
        );
    }

    #[test]
    fn result_usage_sums_disjoint_input_buckets_and_treats_all_zeros_as_unknown() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":7210,"cache_read_input_tokens":41000,"cache_creation_input_tokens":200,"output_tokens":1893,"reasoning_tokens":412}}"#;
        assert_eq!(
            parse(line),
            Ok(Line::ResultSuccess {
                text: "done".to_owned(),
                usage: Usage {
                    input_tokens: Some(48_410),
                    output_tokens: Some(1893),
                    cached_input_tokens: Some(41_000),
                    cache_write_input_tokens: Some(200),
                    reasoning_output_tokens: Some(412),
                },
            })
        );

        let all_zero = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0}}"#;
        assert_eq!(
            parse(all_zero),
            Ok(Line::ResultSuccess {
                text: "done".to_owned(),
                usage: Usage::default(),
            })
        );
    }

    #[test]
    fn unsupported_effort_option_errors_are_recognized() {
        assert!(names_unsupported_effort_option(
            "error: unexpected argument '--reasoning-effort=high' found\n"
        ));
        assert!(names_unsupported_effort_option(
            "error: unknown option '--effort'\n"
        ));
        assert!(!names_unsupported_effort_option(
            "Usage: grok [--reasoning-effort <EFFORT>]\n"
        ));
    }

    #[test]
    fn failed_result_falls_back_to_top_level_result() {
        let line = parse(r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Not signed in. Run grok login."}"#).unwrap();
        assert!(matches!(
            line,
            Line::ResultFailed(error) if error.reason == "AUTH_REJECTED"
        ));
    }

    #[test]
    fn provider_errors_are_normalized_without_bare_sign_in_matches() {
        assert_eq!(
            provider_failure("Authentication failed").reason,
            "AUTH_REJECTED"
        );
        assert_eq!(
            provider_failure("Not signed in. Run grok login.").reason,
            "AUTH_REJECTED"
        );
        assert_eq!(
            provider_failure("failed to assign input buffer").reason,
            "PROVIDER_UNAVAILABLE"
        );
        assert_eq!(
            provider_failure("HTTP 429 rate limit").reason,
            "PROVIDER_RATE_LIMITED"
        );
    }
}
