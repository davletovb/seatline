//! The provider adapters of the runtime: one per execution mode of a provider
//! CLI, behind one neutral [`Provider`] trait.
//!
//! A [`Provider`] reports its status and runs a [`Turn`] as an [`Exchange`]: a
//! state machine its caller drives, which never blocks past the deadline it is
//! given, and never keeps working past it for longer than [`BUSY_LIMIT`],
//! however fast the provider writes. That lets one caller serve several turns,
//! and read cancellations, while providers work.
//!
//! Everything provider-specific stays inside the adapter: command lines,
//! output formats, and what a native-session handle means. An adapter knows no
//! conversations and nothing of any application's protocol: it reports
//! [`Update`]s, and an application maps them to its own.
//!
//! | Module | Execution mode |
//! |---|---|
//! | [`codex`] | `codex exec --json` |
//! | [`claude`] | `claude -p` in stream-json mode |
//! | [`gemini`] | Antigravity's one-shot mode (`agy`) |
//! | [`grok`] | Grok's one-shot headless mode |

use std::io;

pub use seatline_core::exchange::{Exchange, Scripted, Timeouts, Update};
use seatline_core::protocol::{Capabilities, ErrorCode, Failure};
pub use seatline_core::stream::BUSY_LIMIT;
use seatline_core::turn::Turn;

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod grok;
pub mod readiness;

pub(crate) fn keep_bounded_output(output: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    let remaining = limit.saturating_sub(output.len());
    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

/// File-system work that removes what a provider saved. It owns everything it
/// needs, so it runs on a thread of its own and never holds up the caller.
pub struct Cleanup {
    /// The removal itself, on that thread.
    pub work: Box<dyn FnOnce() -> io::Result<()> + Send>,
    /// Runs on the caller's thread once `work` succeeded, and never
    /// otherwise: what it drops, such as an in-memory record, stays for a
    /// retry after a failure.
    pub completed: Box<dyn FnOnce()>,
}

impl Cleanup {
    /// Nothing to remove.
    pub fn nothing() -> Self {
        Self::new(|| Ok(()), || {})
    }

    pub fn new(
        work: impl FnOnce() -> io::Result<()> + Send + 'static,
        completed: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            work: Box::new(work),
            completed: Box::new(completed),
        }
    }

    /// This cleanup, then `next`: the second runs only if the first
    /// succeeded, and both `completed` steps run once both worked.
    #[must_use]
    pub fn then(self, next: Self) -> Self {
        let (first, second) = (self.work, next.work);
        let (first_done, second_done) = (self.completed, next.completed);
        Self::new(
            move || {
                first()?;
                second()
            },
            move || {
                first_done();
                second_done();
            },
        )
    }
}

/// A turn a caller built wrongly: an adapter refuses it instead of passing
/// what it holds to a command line.
pub const INVALID_TURN: Failure = Failure {
    code: ErrorCode::InvalidRequest,
    reason: "INVALID_TURN",
    retryable: false,
};

/// Explicit choices are never silently replaced by a provider default.
pub const REASONING_EFFORT_UNSUPPORTED: Failure = Failure {
    code: ErrorCode::InvalidRequest,
    reason: "REASONING_EFFORT_UNSUPPORTED",
    retryable: false,
};

/// An explicit tier is refused rather than silently using another tier.
pub const SERVICE_TIER_UNSUPPORTED: Failure = Failure {
    code: ErrorCode::InvalidRequest,
    reason: "SERVICE_TIER_UNSUPPORTED",
    retryable: false,
};

/// One execution mode of a provider CLI, in terms that belong to no
/// application: it runs a [`Turn`], and knows nothing of conversations, of
/// browsers, or of which sessions an application keeps.
pub trait Provider {
    /// The provider ID requests name, such as `codex`.
    fn id(&self) -> &str;

    fn timeouts(&self) -> Timeouts;

    /// What this mode can do. It can change while the runtime runs, for
    /// example when the user's own provider configuration changes.
    fn capabilities(&self) -> Capabilities;

    /// Whether this mode can keep a native session and resume it by an opaque
    /// handle. Only such modes accept a persistent [`Turn`].
    fn supports_persistent_session(&self) -> bool {
        false
    }

    /// Starts checking availability, authentication, and capabilities. The
    /// exchange reports one `Status` and then `Completed`.
    fn status(&self) -> Box<dyn Exchange>;

    /// Explicit freshness. Wrap a local adapter in [`readiness::Ready`] to
    /// enable verified caching and concurrent check deduplication.
    fn readiness(&self, _freshness: seatline_core::readiness::Freshness) -> Box<dyn Exchange> {
        self.status()
    }

    /// Whether preparation can resolve the executable, check the workspace,
    /// and check sign-in without a model prompt. No process reuse is implied.
    fn supports_preparation(&self) -> bool {
        false
    }

    fn prepare(&self, freshness: seatline_core::readiness::Freshness) -> Box<dyn Exchange> {
        if self.supports_preparation() {
            self.readiness(freshness)
        } else {
            Box::new(Scripted::failed(Failure {
                code: ErrorCode::InvalidRequest,
                reason: "PREPARATION_UNSUPPORTED",
                retryable: false,
            }))
        }
    }

    /// Opt-in readiness before sending, with a Status update before launch.
    /// `turn.check_sign_in` always overrides cached freshness with Fresh.
    fn send_with_readiness(
        &self,
        turn: Turn,
        _freshness: seatline_core::readiness::Freshness,
    ) -> Box<dyn Exchange> {
        let _ = turn;
        Box::new(Scripted::failed(Failure {
            code: ErrorCode::InvalidRequest,
            reason: "READINESS_UNSUPPORTED",
            retryable: false,
        }))
    }

    /// Enforce the application's accepted sign-in modes before launch.
    /// Adapters without this gate refuse; they must never fall back to send.
    fn send_with_readiness_policy(
        &self,
        turn: Turn,
        freshness: seatline_core::readiness::Freshness,
        policy: seatline_core::readiness::SignInPolicy,
    ) -> Box<dyn Exchange> {
        let _ = (turn, freshness, policy);
        Box::new(Scripted::failed(Failure {
            code: ErrorCode::InvalidRequest,
            reason: "READINESS_UNSUPPORTED",
            retryable: false,
        }))
    }

    /// Opaque configuration/file fingerprint. None disables caching. The
    /// wrapper itself scopes immutable environment and workspace settings.
    fn readiness_key(&self) -> Option<readiness::Key> {
        None
    }

    /// Drop cached evidence after account/config changes or revocation.
    fn invalidate_readiness(&self) {}

    /// Starts serving `turn`. A persistent turn reports the native session it
    /// runs in, before `Started`, as an opaque handle.
    fn send(&self, turn: Turn) -> Box<dyn Exchange>;

    /// Removes what the provider saved for these native sessions, where the
    /// adapter can prove the provider wrote it for this application.
    fn cleanup_sessions(&self, sessions: &[String]) -> Cleanup {
        let _ = sessions;
        Cleanup::nothing()
    }

    /// Retries the per-turn cleanups that failed earlier for the turns of
    /// `group` (`Turn::cleanup_group`).
    fn cleanup_group(&self, group: &str) -> Cleanup {
        let _ = group;
        Cleanup::nothing()
    }
}
