//! What an application is asked to do, and what it measured doing it. The
//! harness hands each simulated application a [`Spec`] and reads [`Sample`]s
//! back, one JSON line each.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One simulated application's whole workload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// The broker's data directory.
    pub root: PathBuf,
    pub app: String,
    pub provider: String,
    pub requests: Vec<Req>,
    /// When this file exists the application makes no further request and
    /// finishes: how a background load is told the measurement is over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Send,
    Status,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Status => "status",
        }
    }
}

/// How the application talks to the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// The wire protocol directly, on a connection of its own for each
    /// request. Connecting, the handshake and the request are timed apart.
    Wire,
    /// The shipped `RemoteProvider`, as an application's adapter uses it: it
    /// starts a thread, a runtime and a connection for every exchange. Only
    /// what the application can see from outside is timed, from the call that
    /// starts the exchange.
    Adapter,
}

/// One request, which the application makes on a connection of its own, as the
/// shipped client does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Req {
    /// Unique across the whole run: it is how the broker's record of the
    /// request is matched with the application's.
    pub id: String,
    pub method: Method,
    /// The question. A fake provider's behavior can be chosen by its first
    /// word; a live provider just answers it.
    pub prompt: String,
    /// Whether the turn keeps a native session the next request can resume.
    pub persistent: bool,
    /// Continue the session the last persistent turn reported.
    pub resume: bool,
    /// Ask the adapter to check the sign-in before the turn.
    pub check_sign_in: bool,
    /// How long to wait before making the request.
    pub gap_ms: u64,
    /// Whether the request is part of the measurement or only warms things up.
    pub measured: bool,
    pub via: Via,
}

/// What the application saw of one request. Durations are microseconds from
/// the application's own monotonic clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub id: String,
    pub app: String,
    pub method: Method,
    pub measured: bool,
    pub via: Via,
    /// `completed`, `failed`, `stopped`, or `error` when the exchange broke.
    pub outcome: String,
    /// A failure reason or an I/O error kind: never provider output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Connecting to the broker. On a cold start this includes starting it.
    /// Only over the wire: an adapter does not show it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_us: Option<u64>,
    /// Sending the credential until `ready` arrives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_us: Option<u64>,
    /// Everything before the request was sent: connecting and the handshake.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepare_us: Option<u64>,
    /// From sending the request (over the wire) or starting the exchange (by
    /// the adapter) to the first event of each kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_to_launched_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_to_started_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_to_first_text_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_to_complete_us: Option<u64>,
    /// The whole request as the application lived it: preparation included.
    pub total_us: u64,
    /// The broker's phases for this request, joined in afterwards from its
    /// telemetry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broker: Option<Broker>,
}

/// The broker's own account of a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Broker {
    /// The broker's handshake with this request's connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_us: Option<u64>,
    pub outcome: String,
    pub probes: u64,
    pub launches: u64,
    pub total_us: u64,
    pub phases_us: std::collections::BTreeMap<String, u64>,
}
