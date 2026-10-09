//! Claude Code stream-json output reduced to provider-neutral events.

use serde_json::Value;

use seatline_core::protocol::ErrorCode;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::turn::Usage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Init(String),
    MessageStart,
    TextDelta(String),
    /// A tool call starts in the message being streamed.
    ToolUseStart,
    /// The message being streamed ended.
    MessageStop,
    ToolEvents(Vec<ToolEvent>),
    ResultSuccess {
        session_id: Option<String>,
        text: String,
        usage: Usage,
    },
    ResultFailed(ErrorBody),
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEvent {
    WebSearchUse(String),
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

const MAX_SESSION_ID_LENGTH: usize = 128;

pub fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_LENGTH
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub fn parse(line: &str) -> Result<Line, Malformed> {
    if line.trim().is_empty() {
        return Ok(Line::Ignored);
    }
    let event: Value = serde_json::from_str(line).map_err(|_| Malformed)?;
    let kind = event.get("type").and_then(Value::as_str).ok_or(Malformed)?;
    Ok(match kind {
        "system" if event.get("subtype").and_then(Value::as_str) == Some("init") => {
            let id = event
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or(Malformed)?;
            if !is_session_id(id) {
                return Err(Malformed);
            }
            Line::Init(id.to_owned())
        }
        "stream_event" => {
            let nested = event
                .get("event")
                .filter(|value| value.is_object())
                .ok_or(Malformed)?;
            match nested
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "message_start" => Line::MessageStart,
                "content_block_delta" => {
                    let delta = nested
                        .get("delta")
                        .filter(|value| value.is_object())
                        .ok_or(Malformed)?;
                    if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                        Line::TextDelta(
                            delta
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or(Malformed)?
                                .to_owned(),
                        )
                    } else {
                        Line::Ignored
                    }
                }
                "content_block_start"
                    if nested
                        .pointer("/content_block/type")
                        .and_then(Value::as_str)
                        == Some("tool_use") =>
                {
                    Line::ToolUseStart
                }
                "message_stop" => Line::MessageStop,
                _ => Line::Ignored,
            }
        }
        "assistant" | "user" => event
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|blocks| blocks.iter().filter_map(tool_event).collect::<Vec<_>>())
            .filter(|events| !events.is_empty())
            .map(Line::ToolEvents)
            .unwrap_or(Line::Ignored),
        "result" => {
            let failed = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || event.get("subtype").and_then(Value::as_str) != Some("success");
            if failed {
                let message = event
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Line::ResultFailed(result_failure(message))
            } else {
                let id = event
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|id| is_session_id(id))
                    .map(str::to_owned);
                Line::ResultSuccess {
                    session_id: id,
                    text: event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    usage: Usage {
                        input_tokens: total_input_tokens(event.get("usage")),
                        output_tokens: event
                            .pointer("/usage/output_tokens")
                            .and_then(Value::as_u64),
                        cached_input_tokens: event
                            .pointer("/usage/cache_read_input_tokens")
                            .and_then(Value::as_u64),
                        cache_write_input_tokens: event
                            .pointer("/usage/cache_creation_input_tokens")
                            .and_then(Value::as_u64),
                        reasoning_output_tokens: None,
                    },
                }
            }
        }
        _ => Line::Ignored,
    })
}

fn total_input_tokens(usage: Option<&Value>) -> Option<u64> {
    let usage = usage?;
    let input = usage.get("input_tokens").and_then(Value::as_u64);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let cache_read = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
    if input.is_none() && cache_creation.is_none() && cache_read.is_none() {
        None
    } else {
        Some(
            input
                .unwrap_or(0)
                .saturating_add(cache_creation.unwrap_or(0))
                .saturating_add(cache_read.unwrap_or(0)),
        )
    }
}

fn tool_event(block: &Value) -> Option<ToolEvent> {
    match block.get("type").and_then(Value::as_str)? {
        "tool_use" if block.get("name").and_then(Value::as_str) == Some("WebSearch") => {
            let id = block.get("id").and_then(Value::as_str)?;
            (!id.is_empty()).then(|| ToolEvent::WebSearchUse(id.to_owned()))
        }
        "tool_result" => {
            let tool_use_id = block.get("tool_use_id").and_then(Value::as_str)?;
            let content = block.get("content").and_then(Value::as_str)?;
            Some(ToolEvent::ToolResult {
                tool_use_id: tool_use_id.to_owned(),
                content: content.to_owned(),
            })
        }
        _ => None,
    }
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

const UNKNOWN_SESSION: ErrorBody = ErrorBody {
    code: ErrorCode::InvalidRequest,
    reason: "UNKNOWN_SESSION",
    retryable: false,
};

const UNAVAILABLE: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_UNAVAILABLE",
    retryable: true,
};

/// Whether Claude's message says the session it was asked to resume doesn't
/// exist. Claude reports this in a `result`, or on stderr before `init` as
/// "No conversation found with session ID: …".
pub fn names_unknown_session(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "session no longer exists",
        "session not found",
        "no conversation found",
        "unknown session",
        "invalid session id",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// Whether Claude's message says its command line does not know `option` (given
/// in lower case). Claude's CLI reports this on stderr and exits before it
/// starts anything: `error: unknown option '--effort=low'`. The option has to
/// be named on the line that says so, as a whole name: a CLI that knows it but
/// refuses something else, and prints its usage after the error, lists it too.
pub fn names_unknown_option(message: &str, option: &str) -> bool {
    message.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("unknown option") && names_whole_option(&line, option)
    })
}

/// Whether `text` has `option` in it and not just as the start of a longer name.
fn names_whole_option(text: &str, option: &str) -> bool {
    text.match_indices(option).any(|(at, _)| {
        !text[at + option.len()..]
            .starts_with(|next: char| next.is_ascii_alphanumeric() || next == '-' || next == '_')
    })
}

/// Classifies a failed `result` by specific phrases, never bare words such as
/// "rate" or "auth". Besides Claude's own wording, this covers the raw API
/// errors it passes on, such as `API Error: 429 {"type":"error","error":{"type":
/// "rate_limit_error",…}}`.
pub fn result_failure(message: &str) -> ErrorBody {
    let lower = message.to_ascii_lowercase();
    if names_unknown_session(message) {
        UNKNOWN_SESSION
    } else if [
        "authentication failed",
        "authentication_error",
        "not authenticated",
        "login required",
        "not logged in",
        "oauth token",
        "401 unauthorized",
        "api error: 401",
        "invalid api key",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
    {
        AUTH_REJECTED
    } else if [
        "rate limit",
        "rate_limit_error",
        "usage limit",
        "too many requests",
        "api error: 429",
        "billing limit",
        "credit balance",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
        // "5-hour limit reached", "Weekly limit reached"; a context limit is
        // not something waiting fixes.
        || (lower.contains("limit reached") && !lower.contains("context"))
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
    fn parses_init_message_start_delta_and_result() {
        assert_eq!(
            parse(r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#),
            Ok(Line::Init("abc-123".to_owned()))
        );
        assert_eq!(
            parse(
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}}}"#
            ),
            Ok(Line::MessageStart)
        );
        assert_eq!(
            parse(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}}"#
            ),
            Ok(Line::TextDelta("hi".to_owned()))
        );
        assert_eq!(
            parse(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"abc-123"}"#
            ),
            Ok(Line::ResultSuccess {
                session_id: Some("abc-123".to_owned()),
                text: "done".to_owned(),
                usage: Usage::default()
            })
        );
    }
    #[test]
    fn parses_real_claude_websearch_tool_use_and_result_shapes() {
        let use_line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"WebSearch","input":{"query":"rust"}}]}}"#;
        assert_eq!(
            parse(use_line),
            Ok(Line::ToolEvents(vec![ToolEvent::WebSearchUse(
                "toolu_1".to_owned()
            )]))
        );

        let result_line = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"Web search results for query: \"rust\"\n\nLinks: [{\"title\":\"Rust\",\"url\":\"https://www.rust-lang.org/\"}]"}]}}"#;
        assert!(matches!(
            parse(result_line),
            Ok(Line::ToolEvents(events))
                if matches!(
                    events.as_slice(),
                    [ToolEvent::ToolResult { tool_use_id, content }]
                        if tool_use_id == "toolu_1" && content.contains("Links:")
                )
        ));
    }

    #[test]
    fn a_tool_call_starting_and_a_message_ending_are_told_apart() {
        assert_eq!(
            parse(
                r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"WebSearch","input":{}}}}"#
            ),
            Ok(Line::ToolUseStart)
        );
        assert_eq!(
            parse(
                r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#
            ),
            Ok(Line::Ignored)
        );
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"message_stop"}}"#),
            Ok(Line::MessageStop)
        );
    }

    #[test]
    fn unknown_stream_events_do_not_count_as_progress() {
        assert_eq!(
            parse(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"x"}}}"#
            ),
            Ok(Line::Ignored)
        );
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"future_event"}}"#),
            Ok(Line::Ignored)
        );
    }

    #[test]
    fn invalid_session_ids_are_refused() {
        for id in ["", "--help", "../../x", "has space"] {
            assert!(!is_session_id(id), "{id}");
        }
    }

    #[test]
    fn error_classification_uses_specific_phrases() {
        assert_eq!(
            result_failure("authentication failed").reason,
            "AUTH_REJECTED"
        );
        assert_eq!(
            result_failure("rate limit exceeded (429 Too Many Requests)").reason,
            "PROVIDER_RATE_LIMITED"
        );
        assert_eq!(
            result_failure("session no longer exists").reason,
            "UNKNOWN_SESSION"
        );
        assert_eq!(
            result_failure("No conversation found with session ID: abc").reason,
            "UNKNOWN_SESSION"
        );
        assert_eq!(
            result_failure(
                r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            )
            .reason,
            "AUTH_REJECTED"
        );
        for message in [
            r#"API Error: 429 {"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            "5-hour limit reached \u{2219} resets 3pm",
            "Claude AI usage limit reached|1760000000",
        ] {
            assert_eq!(
                result_failure(message).reason,
                "PROVIDER_RATE_LIMITED",
                "{message}"
            );
        }

        for message in [
            "Failed to generate a response",
            "The author is unavailable",
            "Prompt exceeds context limit",
            "Context limit reached",
        ] {
            assert_eq!(
                result_failure(message).reason,
                "PROVIDER_UNAVAILABLE",
                "{message}"
            );
        }
    }
    #[test]
    fn an_unknown_option_is_told_from_other_complaints() {
        for message in [
            "error: unknown option '--effort=low'\n",
            "Error: Unknown option '--effort' (Did you mean --model?)",
        ] {
            assert!(names_unknown_option(message, "--effort"), "{message}");
        }
        for message in [
            // Another option, a value Claude merely warns about, and silence.
            "error: unknown option '--safe-mode'",
            "Warning: Unknown --effort value 'bogus' - ignoring it",
            "error: option '--effort <level>' argument missing",
            "",
        ] {
            assert!(!names_unknown_option(message, "--effort"), "{message}");
        }
    }

    #[test]
    fn an_option_that_only_the_usage_text_names_is_not_the_unknown_one() {
        // A CLI that knows both options refuses a third and prints its usage.
        let usage = "error: unknown option '--tols'\n(Did you mean --tools?)\n\n\
            Usage: claude [options] [prompt]\n\
              --effort <level>   Effort level for the current session\n\
              --safe-mode        Skip hooks, plugins and skills\n";
        for option in ["--effort", "--safe-mode"] {
            assert!(!names_unknown_option(usage, option), "{option}");
        }
        assert!(names_unknown_option(usage, "--tols"));
        // Nor is a longer name that merely starts with it.
        assert!(!names_unknown_option(
            "error: unknown option '--effort-level'",
            "--effort"
        ));
        assert!(!names_unknown_option(
            "error: unknown option '--safe-mode-x'",
            "--safe-mode"
        ));
        // The option on a line of its own, apart from the complaint, is not named by it.
        assert!(!names_unknown_option(
            "error: unknown option\n--effort\n",
            "--effort"
        ));
    }

    #[test]
    fn usage_includes_cache_creation_and_cache_reads() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"abc-123","usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":6,"output_tokens":3}}"#;
        assert!(matches!(
            parse(line),
            Ok(Line::ResultSuccess {
                usage: Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(3),
                    // What the total is made of, as Claude reports it.
                    cached_input_tokens: Some(6),
                    cache_write_input_tokens: Some(4),
                    reasoning_output_tokens: None,
                },
                ..
            })
        ));
    }
}
