//! Local owner configuration and bounded per-request scheduling hints.
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// The most requests the broker runs at once, of every class together. The
/// owner can lower it, not raise it.
pub const MAX_RUNNING_CEILING: usize = 8;

/// The most requests one application, or one provider, may have running at once
/// within a class, however the owner sets it. The defaults stay at
/// [`DEFAULT_PER_APP_PROVIDER_RUNNING`]; an owner whose application splits one
/// long job into parallel requests can raise them, up to the global ceiling,
/// because each running request is a provider process and a call against the
/// account's rate limits and quota.
pub const MAX_APP_PROVIDER_RUNNING_CEILING: usize = MAX_RUNNING_CEILING;

/// What `max_app_running` and `max_provider_running` are unless the owner's
/// file says otherwise.
pub const DEFAULT_PER_APP_PROVIDER_RUNNING: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Generation,
    Readiness,
    Cleanup,
}

impl Class {
    pub(crate) fn of(method: &str) -> Self {
        match method {
            "status" | "readiness" | "prepare" => Self::Readiness,
            "forget" | "cleanup" => Self::Cleanup,
            _ => Self::Generation,
        }
    }
}

/// Optional top-level `scheduling` on a wire request. Queue events are opt-in
/// so existing protocol-v1 clients see exactly the events they understand.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hints {
    pub interactive: bool,
    pub queue_timeout_ms: Option<u64>,
    pub events: bool,
}

/// Read once from `scheduling.json` beside the broker grants. Hard ceilings
/// remain fixed; a client cannot change the owner's concurrency policy.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub max_running: usize,
    /// Requests of one class one application may have running at once: 2 unless
    /// raised, up to [`MAX_APP_PROVIDER_RUNNING_CEILING`]. The global and class
    /// limits still apply, so a higher value does not take more than they give.
    pub max_app_running: usize,
    /// Requests of one class one provider may have running at once, for all
    /// applications together: 2 unless raised, up to
    /// [`MAX_APP_PROVIDER_RUNNING_CEILING`].
    pub max_provider_running: usize,
    pub max_readiness_running: usize,
    pub max_cleanup_running: usize,
    pub interactive_burst: usize,
    pub queue_timeout_ms: u64,
    /// Start the turns that give Claude no tools without the user's own hooks,
    /// plugins, skills and `CLAUDE.md` (`claude --safe-mode`), which Claude
    /// would otherwise load at every start. Off by default. A turn that leaves
    /// the provider's own configuration in charge keeps them. Not a scheduling
    /// limit: it lives here because this is the owner's one broker-wide file.
    pub claude_isolation: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_running: MAX_RUNNING_CEILING,
            max_app_running: DEFAULT_PER_APP_PROVIDER_RUNNING,
            max_provider_running: DEFAULT_PER_APP_PROVIDER_RUNNING,
            max_readiness_running: 2,
            max_cleanup_running: 2,
            interactive_burst: 3,
            queue_timeout_ms: 30_000,
            claude_isolation: false,
        }
    }
}

impl Policy {
    pub(crate) fn load(root: &Path) -> io::Result<Self> {
        let policy = match std::fs::read(root.join("scheduling.json")) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e),
        };
        policy.validate()?;
        Ok(policy)
    }

    pub(crate) fn generation_limit(&self) -> usize {
        self.max_running
            .saturating_sub(self.max_readiness_running)
            .max(1)
    }

    pub(crate) fn limits(&self) -> std::collections::BTreeMap<&'static str, u64> {
        std::collections::BTreeMap::from([
            ("max_running", self.max_running as u64),
            ("max_app_running", self.max_app_running as u64),
            ("max_provider_running", self.max_provider_running as u64),
            ("running_limits_per_class", 1),
            ("max_generation_running", self.generation_limit() as u64),
            ("max_readiness_running", self.max_readiness_running as u64),
            ("max_cleanup_running", self.max_cleanup_running as u64),
            ("interactive_burst", self.interactive_burst as u64),
            ("queue_timeout_ms", self.queue_timeout_ms),
            ("claude_isolation", u64::from(self.claude_isolation)),
            ("ledger_capacity", crate::ledger::CAPACITY as u64),
            ("cleanup_workers", 2),
            ("cleanup_capacity", 32),
            ("active_poll_min_ms", 1),
            ("active_poll_max_ms", 5),
        ])
    }

    pub fn validate(&self) -> io::Result<()> {
        if !(1..=MAX_RUNNING_CEILING).contains(&self.max_running)
            || !(1..=MAX_APP_PROVIDER_RUNNING_CEILING).contains(&self.max_app_running)
            || !(1..=MAX_APP_PROVIDER_RUNNING_CEILING).contains(&self.max_provider_running)
            || !(1..=2).contains(&self.max_readiness_running)
            || !(1..=2).contains(&self.max_cleanup_running)
            || !(1..=8).contains(&self.interactive_burst)
            || !(1..=900_000).contains(&self.queue_timeout_ms)
        {
            return Err(io::Error::other("invalid scheduling policy"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_policy_and_client_hints_refuse_unknown_fields_and_unbounded_limits() {
        assert!(serde_json::from_str::<Policy>(r#"{"unknown":1}"#).is_err());
        assert!(serde_json::from_str::<Hints>(r#"{"priority":99}"#).is_err());
        for field in [
            "max_running",
            "max_app_running",
            "max_provider_running",
            "max_readiness_running",
            "max_cleanup_running",
            "interactive_burst",
            "queue_timeout_ms",
        ] {
            let policy: Policy = serde_json::from_value(serde_json::json!({field:0})).unwrap();
            assert!(policy.validate().is_err(), "{field}");
            let policy: Policy =
                serde_json::from_value(serde_json::json!({field:1_000_000})).unwrap();
            assert!(policy.validate().is_err(), "{field}");
        }
        assert!(Policy::default().validate().is_ok());
    }

    #[test]
    fn the_app_and_provider_ceilings_default_to_two_and_the_owner_can_raise_them_to_the_global_one()
    {
        let defaults = Policy::default();
        assert_eq!(
            (defaults.max_app_running, defaults.max_provider_running),
            (2, 2)
        );
        // A file that says nothing about them, or about other limits, leaves them at two.
        for silent in ["{}", r#"{"max_running":4}"#, r#"{"claude_isolation":true}"#] {
            let policy: Policy = serde_json::from_str(silent).unwrap();
            assert_eq!(
                (policy.max_app_running, policy.max_provider_running),
                (2, 2)
            );
        }
        for field in ["max_app_running", "max_provider_running"] {
            for allowed in [1, 2, 3, 4, 8] {
                let policy: Policy =
                    serde_json::from_value(serde_json::json!({field: allowed})).unwrap();
                assert!(policy.validate().is_ok(), "{field} = {allowed}");
                assert_eq!(
                    policy.limits()[field],
                    allowed as u64,
                    "{field} is reported"
                );
            }
            // Zero runs nothing; nine is more than the broker runs at all.
            for refused in [0, 9, 1_000_000] {
                let policy: Policy =
                    serde_json::from_value(serde_json::json!({field: refused})).unwrap();
                assert!(policy.validate().is_err(), "{field} = {refused}");
            }
        }
        // The other class limits keep their ceilings: raising these is not raising them.
        for field in ["max_readiness_running", "max_cleanup_running"] {
            let policy: Policy = serde_json::from_value(serde_json::json!({field: 3})).unwrap();
            assert!(policy.validate().is_err(), "{field}");
        }
    }

    #[test]
    fn claude_isolation_is_off_unless_the_owner_turns_it_on_and_is_reported() {
        assert!(!Policy::default().claude_isolation);
        assert_eq!(Policy::default().limits()["claude_isolation"], 0);
        // An owner's file that says nothing about it leaves it off.
        let silent: Policy = serde_json::from_str(r#"{"max_running":4}"#).unwrap();
        assert!(!silent.claude_isolation);
        let on: Policy = serde_json::from_str(r#"{"claude_isolation":true}"#).unwrap();
        assert!(on.validate().is_ok() && on.claude_isolation);
        assert_eq!(on.limits()["claude_isolation"], 1);
        // It is a switch, not a number or a word.
        for wrong in [r#"{"claude_isolation":1}"#, r#"{"claude_isolation":"yes"}"#] {
            assert!(serde_json::from_str::<Policy>(wrong).is_err(), "{wrong}");
        }
    }
}
