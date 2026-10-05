use seatline_core::exchange::{SessionLoss, Update};
use seatline_core::protocol::{ErrorCode, Failure};
use serde_json::{Value, json};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 1024 * 1024;

/// Every failure reason the broker or a provider adapter can put on the wire.
/// The decoder accepts only these, so provider output can never be reflected
/// back as a reason. Defining the constants and the list together keeps the two
/// from drifting: a reason the hub emits is always one the client recognises.
macro_rules! reasons {
    ($($name:ident),* $(,)?) => {
        pub mod reason {
            pub use seatline_core::protocol::CLEANUP_FAILED;
            $(pub const $name: &str = stringify!($name);)*
        }
        pub const KNOWN_REASONS: &[&str] = &[reason::CLEANUP_FAILED, $(reason::$name),*];
    };
}

reasons!(
    UNKNOWN_SESSION,
    LOGIN_REQUIRED,
    EXECUTABLE_NOT_FOUND,
    APP_NOT_AUTHORIZED,
    QUEUE_FULL,
    QUEUE_TIMEOUT,
    PERSISTENT_SESSION_UNSUPPORTED,
    AUTH_REJECTED,
    PROVIDER_RATE_LIMITED,
    PROVIDER_UNAVAILABLE,
    WORKSPACE_UNAVAILABLE,
    PROCESS_EXITED,
    MALFORMED_PROVIDER_OUTPUT,
    PROVIDER_BOUNDARY_VIOLATION,
    PROVIDER_AGENT_NOT_USED,
    PROVIDER_PERMISSIONS_TOO_OPEN,
    MODEL_NOT_SUPPORTED,
    TOOL_ISOLATION_UNAVAILABLE,
    NATIVE_SEARCH_CONFIGURATION_UNSAFE,
    SEARCH_UNSUPPORTED,
    MODEL_MISMATCH,
    WORKSPACE_MISMATCH,
    TOOLSET_MISMATCH,
    SKILLS_MISMATCH,
    MCP_MISMATCH,
    INVALID_TURN,
    READINESS_TIMEOUT,
    READINESS_CHANGED,
    READINESS_EXPIRED,
    READINESS_UNVERIFIED,
    READINESS_UNSUPPORTED,
    SIGN_IN_POLICY_DENIED,
    PREPARATION_UNSUPPORTED,
    INVALID_REQUEST,
    TURN_DEADLINE_EXCEEDED,
    ADAPTER_PANICKED,
    SESSION_STORE_UNAVAILABLE,
    PROVIDER_DEFAULT_TOOLS_DENIED,
    INVALID_CLEANUP_GROUP,
    // Raised by the broker itself.
    PROVIDER_TIMEOUT,
    PROVIDER_FAILED,
    SESSION_STORE_FAILED,
    SESSION_LIMIT_REACHED,
    CLEANUP_BACKLOG_FULL,
    // Raised by clients of the broker.
    COMPANION_DISCONNECTED,
    REMOTE_PROVIDER_FAILED,
    CONSUMER_TOO_SLOW,
);

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Value> {
    let length = reader.read_u32_le().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::other("invalid frame length"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

pub fn failure(code: ErrorCode, reason: &'static str, retryable: bool) -> Failure {
    Failure {
        code,
        reason,
        retryable,
    }
}

pub fn encode_update(update: &Update) -> Value {
    match update {
        Update::Queued { ahead, timeout_ms } => {
            json!({"type":"queued","ahead":ahead,"timeout_ms":timeout_ms})
        }
        Update::Admitted => json!({"type":"admitted"}),
        Update::Launched => json!({"type":"launched"}),
        Update::Started => json!({"type":"started"}),
        Update::Activity => json!({"type":"activity"}),
        Update::Delta(text) => json!({"type":"delta","text":text}),
        Update::Session(handle) => json!({"type":"session","handle":handle}),
        Update::SessionLost(loss) => {
            json!({"type":"session_lost","confirmed":*loss == SessionLoss::Confirmed})
        }
        Update::Usage(usage) => json!({"type":"usage","usage":usage}),
        Update::Source(source) => json!({"type":"source","source":source}),
        Update::Status {
            provider_id,
            status,
        } => json!({"type":"status","provider":provider_id,"status":status}),
        Update::Completed => json!({"type":"completed"}),
        Update::Stopped => json!({"type":"stopped"}),
        Update::Failed(error) => {
            json!({"type":"failed","code":format!("{:?}",error.code),"reason":error.reason,"retryable":error.retryable})
        }
    }
}

pub fn decode_update(value: Value) -> io::Result<Update> {
    let invalid = || io::Error::other("invalid companion event");
    Ok(match value["type"].as_str().ok_or_else(invalid)? {
        "queued" => Update::Queued {
            ahead: u32::try_from(value["ahead"].as_u64().ok_or_else(invalid)?)
                .map_err(|_| invalid())?,
            timeout_ms: value["timeout_ms"]
                .as_u64()
                .filter(|n| (1..=900_000).contains(n))
                .ok_or_else(invalid)?,
        },
        "admitted" => Update::Admitted,
        "launched" => Update::Launched,
        "started" => Update::Started,
        "activity" => Update::Activity,
        "delta" => Update::Delta(value["text"].as_str().ok_or_else(invalid)?.to_owned()),
        "session" => Update::Session(value["handle"].as_str().ok_or_else(invalid)?.to_owned()),
        "session_lost" => Update::SessionLost(if value["confirmed"] == true {
            SessionLoss::Confirmed
        } else {
            SessionLoss::Suspected
        }),
        "usage" => Update::Usage(serde_json::from_value(value["usage"].clone())?),
        "source" => Update::Source(serde_json::from_value(value["source"].clone())?),
        "status" => Update::Status {
            provider_id: value["provider"].as_str().ok_or_else(invalid)?.to_owned(),
            status: serde_json::from_value(value["status"].clone())?,
        },
        "completed" => Update::Completed,
        "stopped" => Update::Stopped,
        "failed" => {
            let code = match value["code"].as_str() {
                Some("ProviderNotFound") => ErrorCode::ProviderNotFound,
                Some("ProviderNotAuthenticated") => ErrorCode::ProviderNotAuthenticated,
                Some("InvalidRequest") => ErrorCode::InvalidRequest,
                Some("SearchFailed") => ErrorCode::SearchFailed,
                Some("InternalError") => ErrorCode::InternalError,
                _ => ErrorCode::ProviderFailed,
            };
            // Failure reasons remain static and cannot carry provider output.
            let reason = value["reason"]
                .as_str()
                .and_then(|reason| KNOWN_REASONS.iter().copied().find(|known| *known == reason))
                .unwrap_or(reason::REMOTE_PROVIDER_FAILED);
            Update::Failed(failure(code, reason, value["retryable"] == true))
        }
        _ => return Err(invalid()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_failures_preserve_rate_limits_and_isolation_without_reflecting_output() {
        for (code, reason, retryable) in [
            (ErrorCode::ProviderFailed, "PROVIDER_RATE_LIMITED", true),
            (
                ErrorCode::InvalidRequest,
                "TOOL_ISOLATION_UNAVAILABLE",
                false,
            ),
        ] {
            let original = failure(code, reason, retryable);
            let decoded = decode_update(encode_update(&Update::Failed(original))).unwrap();
            assert!(matches!(decoded, Update::Failed(actual) if actual == original));
        }
        let value = json!({"type":"failed","code":"ProviderFailed","reason":"private provider output","retryable":false});
        let decoded = decode_update(value).unwrap();
        assert!(
            matches!(decoded, Update::Failed(error) if error.reason == "REMOTE_PROVIDER_FAILED")
        );
    }

    #[test]
    fn every_known_reason_survives_the_wire_unchanged() {
        for reason in KNOWN_REASONS {
            let original = failure(ErrorCode::ProviderFailed, reason, true);
            let decoded = decode_update(encode_update(&Update::Failed(original))).unwrap();
            assert!(
                matches!(decoded, Update::Failed(actual) if actual.reason == *reason),
                "{reason} was rewritten by the decoder"
            );
        }
    }

    #[test]
    fn the_slow_consumer_reason_is_the_one_the_service_and_the_client_end_a_turn_with() {
        assert_eq!(
            reason::CONSUMER_TOO_SLOW,
            seatline_core::backlog::CONSUMER_TOO_SLOW
        );
        assert_eq!(
            seatline_core::backlog::too_slow().reason,
            reason::CONSUMER_TOO_SLOW
        );
    }

    #[test]
    fn reasons_are_unique_screaming_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for reason in KNOWN_REASONS {
            assert!(
                reason
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit()),
                "{reason}"
            );
            assert!(seen.insert(*reason), "{reason} is listed twice");
        }
    }
}
