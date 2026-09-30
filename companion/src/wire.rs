use seatline_core::exchange::{SessionLoss, Update};
use seatline_core::protocol::{ErrorCode, Failure};
use serde_json::{Value, json};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 1024 * 1024;

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
            let reason = match value["reason"].as_str() {
                Some("UNKNOWN_SESSION") => "UNKNOWN_SESSION",
                Some("LOGIN_REQUIRED") => "LOGIN_REQUIRED",
                Some("EXECUTABLE_NOT_FOUND") => "EXECUTABLE_NOT_FOUND",
                Some("APP_NOT_AUTHORIZED") => "APP_NOT_AUTHORIZED",
                Some("QUEUE_FULL") => "QUEUE_FULL",
                Some("PERSISTENT_SESSION_UNSUPPORTED") => "PERSISTENT_SESSION_UNSUPPORTED",
                _ => "REMOTE_PROVIDER_FAILED",
            };
            Update::Failed(failure(code, reason, value["retryable"] == true))
        }
        _ => return Err(invalid()),
    })
}
