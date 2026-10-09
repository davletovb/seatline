//! Antigravity CLI stream-json output reduced to provider-neutral events.

use serde_json::Value;

use seatline_core::protocol::ErrorCode;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::turn::Usage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Init {
        conversation_id: String,
        permission_mode: String,
        agent: String,
    },
    /// Text of the answer being written: fragments while the step is
    /// ACTIVE, and the last one when it is DONE.
    AgentDelta {
        index: Option<u64>,
        text: String,
        done: bool,
    },
    Tool(String),
    Subagent,
    /// A step Antigravity documents that is neither answer nor action: the
    /// prompt echoed back (`user_input`), a `system_message`, or a step it
    /// doesn't classify (`unknown`). Its text is never shown.
    OtherStep {
        index: Option<u64>,
    },
    /// An update that doesn't name its step type, such as a later update of
    /// a step already seen; `index` says which.
    Untyped {
        index: Option<u64>,
        text: String,
        done: bool,
    },
    /// A step type Antigravity doesn't document. It fails closed.
    UnexpectedStep,
    ResultSuccess {
        conversation_id: Option<String>,
        response: String,
        usage: Usage,
    },
    ResultFailed(ErrorBody),
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

const MAX_CONVERSATION_ID_LENGTH: usize = 128;

pub fn is_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CONVERSATION_ID_LENGTH
        && !id.starts_with('-')
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub fn parse(line: &str) -> Result<Line, Malformed> {
    if line.trim().is_empty() {
        return Ok(Line::Ignored);
    }
    let event: Value = serde_json::from_str(line).map_err(|_| Malformed)?;
    let kind = event
        .get("event")
        .and_then(Value::as_str)
        .ok_or(Malformed)?;
    Ok(match kind {
        "init" => {
            let conversation_id = event
                .get("conversation_id")
                .and_then(Value::as_str)
                .ok_or(Malformed)?;
            if !is_conversation_id(conversation_id) {
                return Err(Malformed);
            }
            let init = event.get("init").ok_or(Malformed)?;
            let permission_mode = init
                .get("permission_mode")
                .and_then(Value::as_str)
                .ok_or(Malformed)?;
            let agent = init.get("agent").and_then(Value::as_str).ok_or(Malformed)?;
            Line::Init {
                conversation_id: conversation_id.to_owned(),
                permission_mode: permission_mode.to_owned(),
                agent: agent.to_owned(),
            }
        }
        "step_update" => {
            let update = event.get("step_update").ok_or(Malformed)?;
            if update
                .get("subagent_info")
                .is_some_and(|value| !value.is_null())
                || update.get("step_type").and_then(Value::as_str) == Some("subagent")
            {
                Line::Subagent
            } else if update.get("step_type").and_then(Value::as_str) == Some("tool")
                || update
                    .get("tool_info")
                    .is_some_and(|value| !value.is_null())
            {
                Line::Tool(
                    update
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                )
            } else {
                let index = update.get("step_index").and_then(Value::as_u64);
                let text = update
                    .get("text_delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let done = update.get("state").and_then(Value::as_str) == Some("DONE");
                match update.get("step_type") {
                    None | Some(Value::Null) => Line::Untyped { index, text, done },
                    Some(kind) => match kind.as_str() {
                        Some("agent_response") => Line::AgentDelta { index, text, done },
                        Some("user_input" | "system_message" | "unknown") => {
                            Line::OtherStep { index }
                        }
                        // Antigravity is an execution-capable runtime. A step
                        // type it doesn't document fails closed at the
                        // adapter boundary rather than counting as progress.
                        _ => Line::UnexpectedStep,
                    },
                }
            }
        }
        "result" => {
            let result = event.get("result").ok_or(Malformed)?;
            let status = result
                .get("status")
                .and_then(Value::as_str)
                .ok_or(Malformed)?;
            if status == "SUCCESS" {
                let conversation_id = result
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if conversation_id
                    .as_deref()
                    .is_some_and(|id| !is_conversation_id(id))
                {
                    return Err(Malformed);
                }
                Line::ResultSuccess {
                    conversation_id,
                    response: result
                        .get("response")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    usage: Usage {
                        input_tokens: result
                            .pointer("/usage/input_tokens")
                            .and_then(Value::as_u64),
                        output_tokens: result
                            .pointer("/usage/output_tokens")
                            .and_then(Value::as_u64),
                        ..Usage::default()
                    },
                }
            } else {
                let message = result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or(status);
                Line::ResultFailed(provider_failure(message))
            }
        }
        _ => Line::Ignored,
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
        "authentication required",
        "not authenticated",
        "not signed in",
        "sign-in required",
        "signin required",
        "login required",
        "credentials missing",
        "credentials not found",
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
        "api error: 429",
        "usage limit",
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
    fn parses_antigravity_stream_json_events() {
        assert_eq!(
            parse(
                r#"{"event":"init","conversation_id":"agy-123","init":{"permission_mode":"request-review","agent":"my-app-text","tools":[]}}"#
            ),
            Ok(Line::Init {
                conversation_id: "agy-123".to_owned(),
                permission_mode: "request-review".to_owned(),
                agent: "my-app-text".to_owned(),
            })
        );
        assert_eq!(
            parse(
                r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"ACTIVE","text_delta":"hi"}}"#
            ),
            Ok(Line::AgentDelta {
                index: None,
                text: "hi".to_owned(),
                done: false,
            })
        );
        assert_eq!(
            parse(
                r#"{"event":"step_update","step_update":{"step_type":"tool","tool_name":"search_web","tool_info":{}}}"#
            ),
            Ok(Line::Tool("search_web".to_owned()))
        );
        assert_eq!(
            parse(
                r#"{"event":"result","result":{"conversation_id":"agy-123","status":"SUCCESS","response":"done"}}"#
            ),
            Ok(Line::ResultSuccess {
                conversation_id: Some("agy-123".to_owned()),
                response: "done".to_owned(),
                usage: Usage::default(),
            })
        );
    }

    #[test]
    fn invalid_conversation_ids_and_malformed_events_are_refused() {
        for id in ["", "--help", "../../x", "has space"] {
            let line = serde_json::json!({
                "event":"init",
                "conversation_id":id,
                "init":{"permission_mode":"request-review","agent":"my-app-text"}
            });
            assert_eq!(parse(&line.to_string()), Err(Malformed));
        }
        assert_eq!(parse("{not json"), Err(Malformed));
    }

    #[test]
    fn failures_are_normalized_without_forwarding_provider_text() {
        for (message, code, reason) in [
            (
                "authentication required",
                ErrorCode::ProviderNotAuthenticated,
                "AUTH_REJECTED",
            ),
            (
                "http 429 RESOURCE_EXHAUSTED quota exceeded",
                ErrorCode::ProviderFailed,
                "PROVIDER_RATE_LIMITED",
            ),
            (
                "secret provider detail",
                ErrorCode::ProviderFailed,
                "PROVIDER_UNAVAILABLE",
            ),
        ] {
            let failure = provider_failure(message);
            assert_eq!(failure.code, code);
            assert_eq!(failure.reason, reason);
        }
    }

    #[test]
    fn unrelated_numbers_are_not_rate_limits() {
        for message in [
            "server.go:4291 crashed",
            "pid 14290 exited",
            "quota note only",
        ] {
            assert_eq!(provider_failure(message).reason, "PROVIDER_UNAVAILABLE");
        }
    }

    /// Every real run starts with the prompt echoed back as a `user_input`
    /// step (seen in `agy` 1.2.x output); `system_message` and `unknown` steps
    /// are documented too. None of them is answer text or an action.
    #[test]
    fn documented_non_answer_steps_are_not_answers_or_actions() {
        for (kind, index) in [("user_input", 0), ("system_message", 1), ("unknown", 2)] {
            let line = serde_json::json!({
                "event":"step_update",
                "step_update":{"conversation_id":"agy-1","step_index":index,"state":"DONE","step_type":kind,"text_delta":"not answer"}
            });
            assert_eq!(
                parse(&line.to_string()),
                Ok(Line::OtherStep { index: Some(index) }),
                "{kind}"
            );
        }
        assert_eq!(
            parse(
                r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","text_delta":"\n"}}"#
            ),
            Ok(Line::Untyped {
                index: Some(3),
                text: "\n".to_owned(),
                done: true
            })
        );
        assert_eq!(
            parse(
                r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"agent_response","text_delta":"PONG"}}"#
            ),
            Ok(Line::AgentDelta {
                index: Some(3),
                text: "PONG".to_owned(),
                done: false
            })
        );
    }

    #[test]
    fn unknown_step_types_fail_closed() {
        assert_eq!(
            parse(
                r#"{"event":"step_update","step_update":{"step_type":"command","state":"ACTIVE"}}"#
            ),
            Ok(Line::UnexpectedStep)
        );
    }
}
