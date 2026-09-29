//! Grok Build headless Messages output reduced to provider-neutral events.

use serde_json::Value;

use seatline_core::protocol::ErrorCode;
use seatline_core::protocol::Failure as ErrorBody;

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
    Assistant {
        text: String,
        activity: bool,
        forbidden_tool: bool,
    },
    ResultSuccess {
        text: String,
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
        // The adapter does not expose token-level Grok streaming yet, but partial
        // frames prove that the provider is still making progress and must
        // refresh the idle timer.
        "stream_event" => Line::Activity,
        _ => Line::Ignored,
    })
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
    fn partial_frames_are_activity() {
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#).unwrap(),
            Line::Activity
        );
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
