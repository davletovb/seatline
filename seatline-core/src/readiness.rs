//! Explicit readiness freshness. A successful check is evidence of a local
//! sign-in, never a guarantee that a later model request will succeed.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Maximum lifetime of verified readiness, including credentials held in an
/// OS keyring whose changes cannot be observed through file metadata.
pub const MAX_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Freshness {
    /// Always starts a new check, even if another check is already running.
    Fresh,
    /// Reuse verified readiness no older than this, capped at [`MAX_AGE`].
    /// Concurrent cache misses can share a check. Zero means fresh.
    Cached { max_age_ms: u64 },
}

impl Freshness {
    pub fn max_age(self) -> Duration {
        match self {
            Self::Fresh => Duration::ZERO,
            Self::Cached { max_age_ms } => Duration::from_millis(max_age_ms).min(MAX_AGE),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Fresh,
    Cached,
    /// Another caller owns the check. This request launched no probe.
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    pub source: Source,
    /// Age of the verified result when returned, on the monotonic clock.
    pub age_ms: u64,
}
