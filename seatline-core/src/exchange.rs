//! Bounded-deadline provider exchange events, shared by every adapter.
//!
//! The events are the runtime's own: an application maps them to whatever it
//! sends its users, and it owns the conversations the runtime knows nothing
//! about.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::protocol::{Failure, ProviderState, Source};
use crate::turn::Usage;

/// How sure an adapter is that a session it was asked to resume is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLoss {
    /// The provider said the session doesn't exist. The application can drop
    /// what it kept for it and rebuild the dialogue in a new session.
    Confirmed,
    /// The resumed run ended before the turn started, and the adapter can't
    /// tell why: a lost session, or a crash. The application decides whether
    /// to start over from its own history.
    Suspected,
}

/// What an exchange reports, in protocol order. After a terminal update
/// (`Completed`, `Failed`, or `Stopped`), the exchange is finished.
///
/// The scheduling slice adds `Queued` and `Admitted`. Downstream exhaustive
/// matches must add arms for these progress events when updating their pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Opt-in companion scheduling event; not provider activity.
    Queued { ahead: u32, timeout_ms: u64 },
    /// The companion admitted this request to its execution lane.
    Admitted,
    /// The provider process was successfully spawned.
    Launched,
    /// A resumable provider's opaque native session handle. Persistent turns
    /// emit this before `Started`, and again if the provider changes it.
    Session(String),
    /// A cumulative usage snapshot for this turn.
    Usage(Usage),
    /// The session the turn was asked to resume can't be used. Sent at most
    /// once, only before any answer text, and followed by the turn's own
    /// terminal `Failed`: an application that can start over discards that
    /// failure, and one that can't reports it.
    SessionLost(SessionLoss),
    /// The provider accepted the turn: its own start-of-turn event arrived and,
    /// where the adapter checks one, its `init` boundary passed. No answer text
    /// comes before it, and the start limit runs until it arrives.
    Started,
    /// The next piece of the answer (`response.delta`).
    Delta(String),
    /// A normalized source attached to the answer (`response.source`).
    Source(Source),
    /// One provider's status (`provider.status`).
    Status {
        provider_id: String,
        status: ProviderState,
    },
    /// The provider is working without anything to show, for example while
    /// a tool runs. Resets the idle timeout.
    Activity,
    /// Terminal: the request succeeded.
    Completed,
    /// Terminal: the request failed.
    Failed(Failure),
    /// Terminal: the exchange stopped after [`Exchange::cancel`].
    Stopped,
}

impl Update {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_) | Self::Stopped)
    }
}

/// One request being served.
pub trait Exchange {
    /// Returns the next update, waiting until `deadline` at most, or `None` if
    /// the deadline passes first. Output that keeps arriving without an update
    /// may hold the call at most [`crate::stream::BUSY_LIMIT`] past the deadline.
    fn next(&mut self, deadline: Instant) -> Option<Update>;

    /// Stops the work. Updates already produced may still arrive, then
    /// `Stopped`, or another terminal update if the work ended first. A
    /// process that outlives `grace` after being asked to stop is killed.
    fn cancel(&mut self, grace: Duration);

    /// The sign-in probe this exchange ran before its turn, as it measured
    /// it, for [`crate::telemetry`]. Its updates do not show a probe, so an
    /// adapter that runs one reports it here; the default says it ran none.
    fn probe_span(&self) -> Option<crate::telemetry::Span> {
        None
    }

    /// When the provider reported its final result, as the adapter saw it, for
    /// [`crate::telemetry`]: the last line of a turn, such as Claude's `result`
    /// or Codex's `turn.completed`, whether the turn succeeded or failed. The
    /// terminal update comes later, once the provider's process is gone and the
    /// adapter has finished its own wrap-up, and the difference is the tail an
    /// application waits through after its answer is complete. An adapter that
    /// leaves a finished process to exit on its own ([`crate::process::Reaper`])
    /// ends the turn at the result, and its tail is next to nothing. The default
    /// says the adapter reports none.
    fn result_at(&self) -> Option<Instant> {
        None
    }
}

/// How long the host lets a provider's requests take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// From the request to `response.started`.
    pub start: Duration,
    /// Between recognized work updates once the response has started.
    pub idle: Duration,
    /// Absolute wall-clock bound for the whole turn, including provider start.
    pub max_turn: Duration,
    /// How long a cancelled or timed-out request may take to stop before its
    /// process is killed.
    pub stop_grace: Duration,
}

/// An exchange whose updates are all known when it starts.
pub struct Scripted(VecDeque<Update>);

impl Scripted {
    pub fn new(updates: impl IntoIterator<Item = Update>) -> Self {
        Self(updates.into_iter().collect())
    }

    /// An exchange that fails at once with `error`.
    pub fn failed(error: Failure) -> Self {
        Self::new([Update::Failed(error)])
    }
}

impl Exchange for Scripted {
    fn next(&mut self, _deadline: Instant) -> Option<Update> {
        self.0.pop_front()
    }

    fn cancel(&mut self, _grace: Duration) {
        if !self.0.is_empty() {
            self.0 = VecDeque::from([Update::Stopped]);
        }
    }
}
