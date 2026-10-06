//! Neutral turn contract shared by applications and provider adapters.

use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_MODEL_ID_BYTES: usize = 128;
pub const MAX_MODEL_LABEL_BYTES: usize = 64;
pub const MAX_MODEL_OPTIONS: usize = 32;
pub const MAX_CONTINUATION_BYTES: usize = 256;
pub const MAX_CLEANUP_GROUP_BYTES: usize = 64;

pub fn is_model_id(model: &str) -> bool {
    let bytes = model.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_MODEL_ID_BYTES
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || b"._-:/@".contains(&byte))
}

/// Whether `value` can be a native-session handle: opaque to the runtime, and
/// safe to keep in a file name and to pass as one command-line argument. It
/// starts with a letter or digit, so it can be neither an option nor a
/// relative path such as `..`.
pub fn is_session_handle(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_CONTINUATION_BYTES
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// What the provider may do besides answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// No tools at all. For turns whose text the application doesn't control:
    /// it can only inform the answer, never make the provider act. An adapter
    /// that can't guarantee this (see `Capabilities::tool_isolation`) refuses
    /// the turn rather than running it with tools.
    None,
    /// No tools except the provider's own web search.
    NativeWebSearch,
    /// The provider's own configuration decides. For turns whose text the
    /// application wrote itself, so the user's usual provider setup applies.
    ProviderDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPolicy {
    Ephemeral,
    Persistent,
}

/// A requested reasoning budget. Omission leaves the provider's own default.
/// Adapters that cannot honor this choice refuse it before launching a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// A requested processing tier, independent of the model's reasoning budget.
/// Omission leaves the provider's configured tier in charge. An explicit
/// Standard choice overrides a provider configuration that prefers Fast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Standard,
    Fast,
}

impl ServiceTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fast => "fast",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// The application's own instructions for the conversation, as opposed to
    /// what its user said. It never goes on a command line, where anyone on the
    /// machine could read it. Antigravity (Gemini) reads its system prompt from
    /// an agent file the adapter writes, so it goes there; the other adapters
    /// send it as the first part of the prompt, under an introduction (Codex and
    /// Claude: [`crate::prompt::SYSTEM_INTRO`]; Grok has its own). It goes with every turn that carries it,
    /// including one that resumes a native session, where it repeats what the
    /// session already holds: an application that resumes sessions sends it on
    /// the first turn only. A system prompt that forbids searching starves a
    /// [`ToolPolicy::NativeWebSearch`] turn, whose own instructions ask for
    /// it: an application that sets both means them to agree.
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    pub tools: ToolPolicy,
    pub session: SessionPolicy,
    pub continuation: Option<String>,
    /// Groups this turn's per-turn cleanup records with the others of the same
    /// group, so the application can retry a group's failed deletions
    /// together (one conversation, say). Opaque to the runtime.
    pub cleanup_group: Option<String>,
    /// Legacy send-time probe: Codex/Claude check freshly without emitting a
    /// Status update; Gemini/Grok do not implement an inline check. On the
    /// explicit `send_with_readiness` API, true requires Fresh for every
    /// provider, emits Status before launch, and overrides cached freshness.
    pub check_sign_in: bool,
}

/// Whether `value` can name a cleanup group: a file-name-safe token.
pub fn is_cleanup_group(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_CLEANUP_GROUP_BYTES
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl Turn {
    pub fn validate(&self) -> Result<(), TurnError> {
        if self.messages.is_empty() {
            return Err(TurnError::NoMessages);
        }
        if self
            .system
            .as_ref()
            .is_some_and(|text| text.as_bytes().contains(&0))
            || self
                .messages
                .iter()
                .any(|message| message.text.as_bytes().contains(&0))
        {
            return Err(TurnError::InvalidText);
        }
        if self
            .model
            .as_deref()
            .is_some_and(|model| !is_model_id(model))
        {
            return Err(TurnError::InvalidModel);
        }
        if self
            .continuation
            .as_deref()
            .is_some_and(|continuation| !is_session_handle(continuation))
        {
            return Err(TurnError::InvalidContinuation);
        }
        if self.continuation.is_some() && self.session != SessionPolicy::Persistent {
            return Err(TurnError::ContinuationRequiresPersistentSession);
        }
        if self
            .cleanup_group
            .as_deref()
            .is_some_and(|group| !is_cleanup_group(group))
        {
            return Err(TurnError::InvalidCleanupGroup);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnError {
    NoMessages,
    InvalidText,
    InvalidModel,
    InvalidContinuation,
    ContinuationRequiresPersistentSession,
    InvalidCleanupGroup,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    pub fn is_monotonic_after(self, previous: Self) -> bool {
        non_decreasing(previous.input_tokens, self.input_tokens)
            && non_decreasing(previous.output_tokens, self.output_tokens)
    }
}

fn non_decreasing(previous: Option<u64>, next: Option<u64>) -> bool {
    match (previous, next) {
        (Some(previous), Some(next)) => next >= previous,
        (Some(_), None) => false,
        _ => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInClassification {
    Subscription,
    ApiKey,
    Cloud,
    Unknown,
}

/// The name an application gives the runtime, which becomes a component of
/// every directory the runtime chooses (cache, data, workspaces).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace(String);

impl Namespace {
    /// A namespace of 1 to 64 lowercase ASCII letters, digits, `-` or `_`, that
    /// starts with a letter or a digit (so that a name built from it can never
    /// be read as a command-line option), and is not the name of a Windows
    /// device (`con`, `prn`, `aux`, `nul`, `com0`
    /// to `com9`, `lpt0` to `lpt9`): as a directory name, a device name cannot
    /// be created on Windows, whatever its case. It is refused on every
    /// platform, so that a namespace that works on one works on all.
    pub fn fixed(value: impl Into<String>) -> Result<Self, NamespaceError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || is_windows_device_name(&value)
        {
            return Err(NamespaceError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether Windows reserves `name`, in lowercase, as the name of a device.
fn is_windows_device_name(name: &str) -> bool {
    matches!(name, "con" | "prn" | "aux" | "nul")
        || name
            .strip_prefix("com")
            .or_else(|| name.strip_prefix("lpt"))
            .is_some_and(|number| matches!(number.as_bytes(), [b'0'..=b'9']))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceError;

impl fmt::Display for NamespaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "namespace must be 1-64 lowercase ASCII letters, digits, '-' or '_', \
             starting with a letter or digit, and not a Windows device name such as 'con' or 'com1'",
        )
    }
}

impl std::error::Error for NamespaceError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_tier_is_optional_and_rejects_unrecognized_or_injected_values() {
        let old = serde_json::json!({"system": null, "messages": [{"role": "user", "text": "hello"}],
            "model": null, "tools": "none", "session": "ephemeral", "continuation": null,
            "cleanup_group": null, "check_sign_in": false});
        let turn: Turn = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(turn.service_tier, None);
        assert_eq!(serde_json::to_value(turn).unwrap(), old);
        for tier in [ServiceTier::Standard, ServiceTier::Fast] {
            let mut value = old.clone();
            value["service_tier"] = serde_json::json!(tier.as_str());
            let turn: Turn = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(turn.service_tier, Some(tier));
            assert_eq!(serde_json::to_value(turn).unwrap(), value);
        }
        for invalid in [
            serde_json::json!("fast\" --dangerously-bypass-approvals-and-sandbox"),
            serde_json::json!(true),
            serde_json::json!("unknown"),
            serde_json::json!("priority"),
        ] {
            let mut value = old.clone();
            value["service_tier"] = invalid;
            assert!(serde_json::from_value::<Turn>(value).is_err());
        }
    }

    #[test]
    fn reasoning_effort_is_optional_and_only_accepts_named_budgets() {
        let old = serde_json::json!({"system": null, "messages": [{"role": "user", "text": "hello"}],
            "model": null, "tools": "none", "session": "ephemeral", "continuation": null,
            "cleanup_group": null, "check_sign_in": false});
        let turn: Turn = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(turn.reasoning_effort, None);
        assert_eq!(serde_json::to_value(turn).unwrap(), old);
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ] {
            let mut value = old.clone();
            value["reasoning_effort"] = serde_json::json!(effort.as_str());
            let turn: Turn = serde_json::from_value(value).unwrap();
            assert_eq!(turn.reasoning_effort, Some(effort));
        }
        for invalid in [
            serde_json::json!("low\" --dangerously-bypass-approvals-and-sandbox"),
            serde_json::json!(true),
            serde_json::json!("unknown"),
        ] {
            let mut value = old.clone();
            value["reasoning_effort"] = invalid;
            assert!(serde_json::from_value::<Turn>(value).is_err());
        }
    }

    #[test]
    fn a_continuation_requires_persistence() {
        let turn = Turn {
            system: None,
            messages: vec![Message {
                role: Role::User,
                text: "hello".to_owned(),
            }],
            model: None,
            reasoning_effort: None,
            service_tier: None,
            tools: ToolPolicy::None,
            session: SessionPolicy::Ephemeral,
            continuation: Some("opaque".to_owned()),
            cleanup_group: None,
            check_sign_in: true,
        };
        assert_eq!(
            turn.validate(),
            Err(TurnError::ContinuationRequiresPersistentSession)
        );
    }

    #[test]
    fn usage_snapshots_never_go_backwards() {
        let first = Usage {
            input_tokens: Some(10),
            output_tokens: Some(3),
        };
        assert!(
            Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
            }
            .is_monotonic_after(first)
        );
        assert!(
            !Usage {
                input_tokens: Some(9),
                output_tokens: Some(4),
            }
            .is_monotonic_after(first)
        );
    }

    #[test]
    fn namespaces_are_fixed_safe_path_components() {
        assert_eq!(Namespace::fixed("my-app").unwrap().as_str(), "my-app");
        for value in ["", "My-app", "../my-app", "my app", "-my-app", "_my_app"] {
            assert!(Namespace::fixed(value).is_err(), "{value}");
        }
    }

    #[test]
    fn a_namespace_is_never_a_windows_device_name() {
        // As a directory, `Con` or `con` cannot be created on Windows, and the
        // paths the runtime derives from a namespace would all fail.
        for value in [
            "con", "prn", "aux", "nul", "com0", "com1", "com9", "lpt0", "lpt1", "lpt9",
        ] {
            assert!(Namespace::fixed(value).is_err(), "{value}");
        }
        // Names that only look like them are ordinary.
        for value in [
            "com",
            "lpt",
            "com10",
            "lpt10",
            "console",
            "con-app",
            "my-con",
            "nulls",
            "auxiliary",
        ] {
            assert_eq!(Namespace::fixed(value).unwrap().as_str(), value);
        }
    }

    #[test]
    fn argv_bound_fields_are_validated() {
        let base = Turn {
            system: None,
            messages: vec![Message {
                role: Role::User,
                text: "hello".to_owned(),
            }],
            model: None,
            reasoning_effort: None,
            service_tier: None,
            tools: ToolPolicy::None,
            session: SessionPolicy::Persistent,
            continuation: None,
            cleanup_group: None,
            check_sign_in: false,
        };
        assert!(
            Turn {
                model: Some("--help".to_owned()),
                ..base.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Turn {
                continuation: Some("--resume".to_owned()),
                ..base.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Turn {
                system: Some("bad\0system".to_owned()),
                ..base.clone()
            }
            .validate()
            .is_err()
        );
        for handle in ["..", ".hidden", "-c", "", "a/b", "a b"] {
            assert!(!is_session_handle(handle), "{handle:?}");
        }
        for group in ["", "../up", "has space", &"x".repeat(65)] {
            assert_eq!(
                Turn {
                    cleanup_group: Some(group.to_owned()),
                    ..base.clone()
                }
                .validate(),
                Err(TurnError::InvalidCleanupGroup),
                "{group:?}"
            );
        }
        assert!(
            Turn {
                cleanup_group: Some("conv_0123456789abcdef".to_owned()),
                ..base
            }
            .validate()
            .is_ok()
        );
    }
}
