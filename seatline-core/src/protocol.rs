//! Failures, capabilities and provider status shared by adapters.
//!
//! These are runtime concepts only. An application maps them to its own wire
//! vocabulary and wording: a browser application's protocol can add error
//! categories for its transport, and capabilities such as page context and
//! attachments, on top of these.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::turn::SignInClassification;

/// Failed deletion must remain visible even after cancellation or timeout.
pub const CLEANUP_FAILED: &str = "CLEANUP_FAILED";

/// What kind of failure an adapter reports. Applications match on this, so the
/// set can grow: match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// The provider's executable couldn't be found.
    ProviderNotFound,
    /// The provider is signed out, or rejected the credentials it has.
    ProviderNotAuthenticated,
    /// The provider failed, or produced output the adapter couldn't use.
    ProviderFailed,
    /// A turn that required native web search ended without a usable source.
    SearchFailed,
    /// The request can't be served as it is, for example an unknown session.
    InvalidRequest,
    /// The runtime itself failed, for example while storing a file.
    InternalError,
}

/// A normalized source attached to an answer. Provider adapters fill this
/// provider-neutral shape from their native search results, and the host emits
/// it through `response.source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub backend_id: String,
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<String>,
}

/// Provider-runtime failure. Applications own user-facing wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub code: ErrorCode,
    pub reason: &'static str,
    pub retryable: bool,
}

/// Provider availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Unavailable,
    NotFound,
    Unknown,
}

/// Provider authentication state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authentication {
    Authenticated,
    Unauthenticated,
    Unknown,
}

/// A capability value, serialized as `true`, `false`, or `"unknown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Supported,
    Unsupported,
    Unknown,
}

impl Serialize for Capability {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Supported => serializer.serialize_bool(true),
            Self::Unsupported => serializer.serialize_bool(false),
            Self::Unknown => serializer.serialize_str("unknown"),
        }
    }
}

/// What an execution mode can do. Whether a product offers page context or
/// attachments on top of that is the application's policy, not the runtime's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub streaming: Capability,
    pub continuation: Capability,
    pub web_search: Capability,
    pub model_selection: Capability,
    pub cancellation: Capability,
    /// The adapter can run a turn that gives the provider no tools, so text
    /// the application doesn't control can only inform the answer, never make
    /// the provider act. It can change while the runtime runs: an adapter
    /// reports `Unsupported` while the user's own provider configuration
    /// exposes tools it can't switch off.
    pub tool_isolation: Capability,
}

/// A model an adapter suggests (`status.models`). Suggestions, not the
/// complete set: a provider may accept other valid model IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// Provider-native model ID. Live discovery can own this value.
    pub id: Cow<'static, str>,
    /// Human-readable model name.
    pub label: Cow<'static, str>,
}

/// One provider's availability, sign-in and capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderState {
    pub availability: Availability,
    pub authentication: Authentication,
    pub capabilities: Capabilities,
    /// Suggested models, when `model_selection` is supported. Live provider
    /// catalogs use the owned form.
    pub models: Cow<'static, [ModelOption]>,
    /// How the provider is signed in, when the adapter can tell. Applications
    /// decide whether to accept it, and whether to show account or billing mode.
    pub sign_in: Option<SignInClassification>,
    /// Present on the explicit readiness API; legacy status remains compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<crate::readiness::Readiness>,
}

impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Bool(true) => Ok(Self::Supported),
            serde_json::Value::Bool(false) => Ok(Self::Unsupported),
            serde_json::Value::String(value) if value == "unknown" => Ok(Self::Unknown),
            _ => Err(serde::de::Error::custom("invalid capability")),
        }
    }
}
