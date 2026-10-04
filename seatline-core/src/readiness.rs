//! Explicit readiness freshness. A successful check is evidence of a local
//! sign-in, never a guarantee that a later model request will succeed.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::turn::SignInClassification;

/// Sign-in modes an application permits for a checked generation. Enforced
/// before provider send, including after readiness refresh. Missing sign-in
/// classification is never accepted; Unknown must be explicitly listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<SignInClassification>",
    into = "Vec<SignInClassification>"
)]
pub struct SignInPolicy(Vec<SignInClassification>);

impl SignInPolicy {
    pub fn allows(&self, sign_in: Option<SignInClassification>) -> bool {
        sign_in.is_some_and(|mode| self.0.contains(&mode))
    }
}

impl TryFrom<Vec<SignInClassification>> for SignInPolicy {
    type Error = &'static str;

    fn try_from(modes: Vec<SignInClassification>) -> Result<Self, Self::Error> {
        if modes.is_empty() || modes.len() > 4 {
            return Err("expected one to four allowed sign-in modes");
        }
        let mut unique = Vec::with_capacity(modes.len());
        for mode in modes {
            if !unique.contains(&mode) {
                unique.push(mode);
            }
        }
        Ok(Self(unique))
    }
}

impl From<SignInPolicy> for Vec<SignInClassification> {
    fn from(policy: SignInPolicy) -> Self {
        policy.0
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_deduplicates_modes_without_relaxing_wire_bounds() {
        let policy: SignInPolicy =
            serde_json::from_str(r#"["subscription","api_key","subscription","api_key"]"#).unwrap();
        assert_eq!(
            serde_json::to_value(policy).unwrap(),
            serde_json::json!(["subscription", "api_key"])
        );
        for invalid in [
            r#"[]"#,
            r#"["subscription","subscription","subscription","subscription","subscription"]"#,
            r#"["invalid"]"#,
        ] {
            assert!(serde_json::from_str::<SignInPolicy>(invalid).is_err());
        }
    }
}
