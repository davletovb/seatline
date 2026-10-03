//! Opt-in, instance-scoped readiness caching and prompt-free preparation.
//! [`Ready`] owns one immutable provider/account/workspace/environment
//! configuration. There is no global cache or cross-app result sharing.

use crate::{Cleanup, Exchange, Provider, Scripted, Timeouts, Update};
use seatline_core::discovery::FileStamp;
use seatline_core::protocol::{
    Authentication, Availability, Capabilities, ErrorCode, Failure, ProviderState,
};
use seatline_core::readiness::{Freshness, MAX_AGE, Readiness, Source};
use seatline_core::telemetry::Span;
use seatline_core::turn::Turn;
use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

const CHECK_LIMIT: Duration = Duration::from_secs(30);
const WATCH_BYTES: u64 = 1024 * 1024;

fn age_ms(age: Duration) -> u64 {
    // Round up so a consumer that reconstructs the observation time cannot
    // make evidence younger than it is at the millisecond wire precision.
    u64::try_from(age.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(age.subsec_nanos() % 1_000_000 != 0))
}

/// Deliberately neither serialized nor Debug: no account paths or credential
/// material can escape through status, errors, telemetry, or debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct Key {
    files: Vec<(PathBuf, Option<FileStamp>, Option<u64>)>,
    capabilities: Capabilities,
}

impl Key {
    /// Hash bounded credential/config files in memory and fingerprint the
    /// executable. Missing files are tracked; unreadable/oversized files
    /// disable caching rather than creating a reusable unknown fingerprint.
    pub fn watch(
        executable: &Path,
        files: impl IntoIterator<Item = PathBuf>,
        capabilities: Capabilities,
    ) -> Option<Self> {
        let mut watched = vec![(
            executable.to_owned(),
            FileStamp::read(executable).ok()?,
            None,
        )];
        watched[0].1.as_ref()?;
        for path in files {
            let stamp = if path.is_dir() {
                FileStamp::read_directory(&path).ok()?
            } else {
                FileStamp::read(&path).ok()?
            };
            let digest = if stamp.is_some() && path.is_file() {
                let file = std::fs::File::open(&path).ok()?;
                let mut bytes = Vec::new();
                file.take(WATCH_BYTES + 1).read_to_end(&mut bytes).ok()?;
                if bytes.len() as u64 > WATCH_BYTES {
                    return None;
                }
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hasher);
                Some(hasher.finish())
            } else {
                None
            };
            watched.push((path, stamp, digest));
        }
        Some(Self {
            files: watched,
            capabilities,
        })
    }

    /// A mixed state file may be atomically rewritten for unrelated history
    /// or counters. Its account projection, rather than inode/timestamps or
    /// the entire file, defines readiness. None records a missing file.
    pub(crate) fn with_projection(mut self, path: PathBuf, digest: Option<u64>) -> Self {
        self.files.push((path, None, digest));
        self
    }
}

struct Verified {
    key: Key,
    status: ProviderState,
    at: Instant,
}

#[derive(Default)]
struct State {
    epoch: u64,
    // Only explicit invalidation/authentication failures revoke evidence.
    // Starting another fresh check merely changes publication ownership.
    invalidations: Rc<Cell<u64>>,
    cached: Option<Verified>,
    flight: Option<Weak<RefCell<Flight>>>,
}

/// Reuse this wrapper for one app and one effective adapter configuration.
/// Rebuild it when changing environment or workspace configuration. Local
/// account/config files and the executable are checked before cache reuse.
#[derive(Clone)]
pub struct Ready {
    provider: Rc<dyn Provider>,
    state: Rc<RefCell<State>>,
}

impl Ready {
    pub fn new(provider: impl Provider + 'static) -> Self {
        Self::boxed(Box::new(provider))
    }

    pub fn boxed(provider: Box<dyn Provider>) -> Self {
        Self {
            provider: Rc::from(provider),
            state: Rc::new(RefCell::new(State::default())),
        }
    }

    fn check(&self, freshness: Freshness) -> Box<dyn Exchange> {
        let now = Instant::now();
        let max_age = freshness.max_age();
        if max_age == Duration::ZERO {
            self.provider.invalidate_readiness();
        }
        if max_age != Duration::ZERO {
            let state = self.state.borrow();
            if let Some(cached) = &state.cached {
                let age = now.saturating_duration_since(cached.at);
                // Validate lazily when Status is consumed, then again at
                // Completed. Computing here would duplicate the first read
                // and would not protect a caller that holds the exchange.
                if age < max_age && age < MAX_AGE {
                    return Box::new(CachedHit {
                        ready: self.clone(),
                        status: cached.status.clone(),
                        at: cached.at,
                        key: cached.key.clone(),
                        freshness,
                        inner: None,
                        cursor: 0,
                        epoch: state.epoch,
                    });
                }
            }
        }
        let key = self.provider.readiness_key();
        let mut state = self.state.borrow_mut();
        if max_age != Duration::ZERO && key.is_some() {
            if let Some(flight) = state.flight.as_ref().and_then(Weak::upgrade) {
                let shared = flight.borrow();
                if shared.key == key && shared.epoch == state.epoch && !shared.done {
                    drop(shared);
                    return Box::new(Subscriber {
                        flight: Some(flight),
                        cursor: 0,
                        source: Source::Shared,
                        stopped: false,
                        cancelled_span: None,
                        max_age,
                    });
                }
            }
        }
        // New checks and invalidations prevent an older probe from publishing
        // over newer evidence or repopulating a revoked cache.
        state.epoch = state.epoch.wrapping_add(1);
        state.cached = None;
        let epoch = state.epoch;
        let invalidations = Rc::clone(&state.invalidations);
        let generation = invalidations.get();
        drop(state);
        let exchange = self.provider.status();
        let flight = Rc::new(RefCell::new(Flight {
            exchange: Some(exchange),
            provider: Rc::clone(&self.provider),
            state: Rc::downgrade(&self.state),
            key,
            epoch,
            invalidations,
            generation,
            updates: Vec::new(),
            candidate: None,
            observed: None,
            done: false,
            span: Span::begin(now),
            until: now + CHECK_LIMIT,
        }));
        self.state.borrow_mut().flight = Some(Rc::downgrade(&flight));
        Box::new(Subscriber {
            flight: Some(flight),
            cursor: 0,
            source: Source::Fresh,
            stopped: false,
            cancelled_span: None,
            max_age: if max_age == Duration::ZERO {
                MAX_AGE
            } else {
                max_age
            },
        })
    }

    fn track(&self, exchange: Box<dyn Exchange>) -> Box<dyn Exchange> {
        Box::new(Tracked {
            exchange,
            state: Rc::downgrade(&self.state),
        })
    }
}

impl Provider for Ready {
    fn id(&self) -> &str {
        self.provider.id()
    }
    fn timeouts(&self) -> Timeouts {
        self.provider.timeouts()
    }
    fn capabilities(&self) -> Capabilities {
        self.provider.capabilities()
    }
    fn supports_persistent_session(&self) -> bool {
        self.provider.supports_persistent_session()
    }
    fn supports_preparation(&self) -> bool {
        self.provider.supports_preparation()
    }
    fn status(&self) -> Box<dyn Exchange> {
        self.check(Freshness::Fresh)
    }
    fn readiness(&self, freshness: Freshness) -> Box<dyn Exchange> {
        self.check(freshness)
    }
    fn send(&self, turn: Turn) -> Box<dyn Exchange> {
        self.track(self.provider.send(turn))
    }
    fn send_with_readiness(&self, mut turn: Turn, freshness: Freshness) -> Box<dyn Exchange> {
        if turn.validate().is_err() {
            return Box::new(Scripted::failed(crate::INVALID_TURN));
        }
        let freshness = if turn.check_sign_in {
            Freshness::Fresh
        } else {
            freshness
        };
        // The explicit readiness exchange performs the check for all adapters.
        // Avoid a second inline Codex/Claude probe on the subsequent send.
        turn.check_sign_in = false;
        self.track(Box::new(PreparedSend {
            check: Some(self.check(freshness)),
            provider: Rc::clone(&self.provider),
            turn: Some(turn),
            running: None,
            status: None,
            span: None,
            stopped: false,
        }))
    }
    fn invalidate_readiness(&self) {
        invalidate(&self.state);
        self.provider.invalidate_readiness();
    }
    fn cleanup_sessions(&self, sessions: &[String]) -> Cleanup {
        self.provider.cleanup_sessions(sessions)
    }
    fn cleanup_group(&self, group: &str) -> Cleanup {
        self.provider.cleanup_group(group)
    }
}

fn invalidate(state: &Rc<RefCell<State>>) {
    let mut state = state.borrow_mut();
    state.epoch = state.epoch.wrapping_add(1);
    state
        .invalidations
        .set(state.invalidations.get().wrapping_add(1));
    state.cached = None;
    state.flight = None;
}

struct Flight {
    exchange: Option<Box<dyn Exchange>>,
    provider: Rc<dyn Provider>,
    state: Weak<RefCell<State>>,
    key: Option<Key>,
    epoch: u64,
    // Remains observable after the owning Ready wrapper is dropped, without
    // retaining the cache itself or introducing a reference cycle.
    invalidations: Rc<Cell<u64>>,
    generation: u64,
    updates: Vec<Update>,
    candidate: Option<ProviderState>,
    // Freshness begins when Status was observed, not when a slow subscriber
    // finally asks for Completed.
    observed: Option<Instant>,
    done: bool,
    span: Span,
    until: Instant,
}

impl Flight {
    fn poll(&mut self, deadline: Instant) {
        if self.done {
            return;
        }
        let update = if Instant::now() >= self.until {
            self.exchange.as_mut().unwrap().cancel(Duration::ZERO);
            Some(Update::Failed(Failure {
                code: ErrorCode::ProviderFailed,
                reason: "READINESS_TIMEOUT",
                retryable: true,
            }))
        } else {
            self.exchange
                .as_mut()
                .unwrap()
                .next(deadline.min(self.until))
        };
        let Some(mut update) = update else {
            return;
        };
        match &mut update {
            Update::Status {
                provider_id,
                status,
            } if provider_id == self.provider.id() && self.candidate.is_none() => {
                status.readiness = Some(Readiness {
                    source: Source::Fresh,
                    age_ms: 0,
                });
                self.candidate = Some(status.clone());
                self.observed = Some(Instant::now());
            }
            Update::Completed | Update::Failed(_) | Update::Stopped => {}
            _ => {
                update = Update::Failed(Failure {
                    code: ErrorCode::ProviderFailed,
                    reason: "PROVIDER_BOUNDARY_VIOLATION",
                    retryable: false,
                })
            }
        }
        if update.is_terminal() {
            self.done = true;
            self.span.end = Some(Instant::now());
            if update == Update::Completed
                && self.key.is_some()
                && self.provider.readiness_key() != self.key
            {
                update = Update::Failed(Failure {
                    code: ErrorCode::ProviderFailed,
                    reason: "READINESS_CHANGED",
                    retryable: true,
                });
            }
            if update == Update::Completed {
                if let (Some(key), Some(status), Some(state)) =
                    (&self.key, &self.candidate, self.state.upgrade())
                {
                    if status.availability == Availability::Available
                        && status.authentication == Authentication::Authenticated
                    {
                        let mut state = state.borrow_mut();
                        if state.epoch == self.epoch {
                            state.cached = Some(Verified {
                                key: key.clone(),
                                status: status.clone(),
                                at: self.observed.unwrap(),
                            });
                        }
                    }
                }
            }
            self.exchange.take();
        }
        // At most one Status and one terminal event are retained, regardless
        // of the number or speed of subscribers.
        self.updates.push(update);
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Some(exchange) = &mut self.exchange {
            exchange.cancel(Duration::ZERO);
        }
    }
}

struct Subscriber {
    flight: Option<Rc<RefCell<Flight>>>,
    cursor: usize,
    source: Source,
    stopped: bool,
    cancelled_span: Option<Span>,
    max_age: Duration,
}

impl Exchange for Subscriber {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if self.stopped {
            self.stopped = false;
            return Some(Update::Stopped);
        }
        if self.cursor == usize::MAX {
            return None;
        }
        let flight = self.flight.as_ref()?;
        let mut flight = flight.borrow_mut();
        let polled = self.cursor == flight.updates.len();
        if polled {
            flight.poll(deadline);
        }
        let mut update = flight.updates.get(self.cursor)?.clone();
        self.cursor += 1;
        if let Update::Status { status, .. } = &mut update {
            let age = flight.observed.unwrap().elapsed();
            if age >= self.max_age {
                self.cursor = usize::MAX;
                return Some(Update::Failed(Failure {
                    code: ErrorCode::ProviderFailed,
                    reason: "READINESS_EXPIRED",
                    retryable: true,
                }));
            }
            status.readiness = Some(Readiness {
                source: self.source,
                age_ms: age_ms(age),
            });
        }
        if update == Update::Completed {
            // A peer may have completed this flight while this subscriber
            // paused. Revalidate retained terminal evidence at consumption;
            // a newly polled completion already checked its fingerprint.
            let reason = if flight
                .observed
                .is_some_and(|at| at.elapsed() >= self.max_age)
            {
                Some("READINESS_EXPIRED")
            } else if flight.invalidations.get() != flight.generation
                || (!polled
                    && flight.key.is_some()
                    && flight.provider.readiness_key() != flight.key)
            {
                Some("READINESS_CHANGED")
            } else {
                None
            };
            if let Some(reason) = reason {
                update = Update::Failed(Failure {
                    code: ErrorCode::ProviderFailed,
                    reason,
                    retryable: true,
                });
            }
        }
        Some(update)
    }
    fn cancel(&mut self, _: Duration) {
        self.cancelled_span = self.probe_span().map(|mut span| {
            span.finish(Instant::now());
            span
        });
        self.flight.take();
        self.stopped = true;
    }
    fn probe_span(&self) -> Option<Span> {
        self.cancelled_span.or_else(|| {
            (self.source == Source::Fresh)
                .then(|| self.flight.as_ref().map(|f| f.borrow().span))
                .flatten()
        })
    }
}

struct CachedHit {
    ready: Ready,
    status: ProviderState,
    at: Instant,
    key: Key,
    freshness: Freshness,
    inner: Option<Box<dyn Exchange>>,
    cursor: u8,
    epoch: u64,
}
impl Exchange for CachedHit {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if let Some(inner) = &mut self.inner {
            return inner.next(deadline);
        }
        match self.cursor {
            0 => {
                let age = self.at.elapsed();
                if age >= self.freshness.max_age()
                    || self.ready.provider.readiness_key().as_ref() != Some(&self.key)
                    || self.ready.state.borrow().epoch != self.epoch
                {
                    // Discard only this stale cache. A newer epoch may have
                    // already published different, valid evidence.
                    if self.ready.state.borrow().epoch == self.epoch {
                        // Cache expiry/refresh is not a revocation of another
                        // caller's still-valid fresh check.
                        let mut state = self.ready.state.borrow_mut();
                        state.cached = None;
                        state.flight = None;
                    }
                    self.inner = Some(self.ready.check(self.freshness));
                    return self.inner.as_mut().unwrap().next(deadline);
                }
                self.cursor = 1;
                let mut status = self.status.clone();
                status.readiness = Some(Readiness {
                    source: Source::Cached,
                    age_ms: age_ms(age),
                });
                Some(Update::Status {
                    provider_id: self.ready.id().to_owned(),
                    status,
                })
            }
            1 => {
                self.cursor = 2;
                let reason = if self.at.elapsed() >= self.freshness.max_age() {
                    Some("READINESS_EXPIRED")
                } else if self.ready.state.borrow().epoch != self.epoch
                    || self.ready.provider.readiness_key().as_ref() != Some(&self.key)
                {
                    Some("READINESS_CHANGED")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    Some(Update::Failed(Failure {
                        code: ErrorCode::ProviderFailed,
                        reason,
                        retryable: true,
                    }))
                } else {
                    Some(Update::Completed)
                }
            }
            _ => None,
        }
    }
    fn cancel(&mut self, grace: Duration) {
        if let Some(inner) = &mut self.inner {
            inner.cancel(grace);
        } else {
            self.inner = Some(Box::new(Scripted::new([Update::Stopped])));
        }
    }
    fn probe_span(&self) -> Option<Span> {
        self.inner.as_ref().and_then(|e| e.probe_span())
    }
}

struct Tracked {
    exchange: Box<dyn Exchange>,
    state: Weak<RefCell<State>>,
}
impl Exchange for Tracked {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let update = self.exchange.next(deadline);
        // Changed/expired readiness belongs to this exchange's evidence.
        // Its consumption checks already reject it; treating that consequence
        // as a new revocation could cancel a peer's current fresh check.
        if matches!(&update, Some(Update::Failed(error)) if matches!(error.code, ErrorCode::ProviderNotAuthenticated | ErrorCode::ProviderNotFound) || matches!(error.reason, "AUTH_REJECTED" | "LOGIN_REQUIRED" | "PROVIDER_UNAVAILABLE"))
        {
            if let Some(state) = self.state.upgrade() {
                invalidate(&state);
            }
        }
        update
    }
    fn cancel(&mut self, grace: Duration) {
        self.exchange.cancel(grace);
    }
    fn probe_span(&self) -> Option<Span> {
        self.exchange.probe_span()
    }
}

struct PreparedSend {
    check: Option<Box<dyn Exchange>>,
    provider: Rc<dyn Provider>,
    turn: Option<Turn>,
    running: Option<Box<dyn Exchange>>,
    status: Option<ProviderState>,
    span: Option<Span>,
    stopped: bool,
}

impl Exchange for PreparedSend {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if self.stopped {
            self.stopped = false;
            return Some(Update::Stopped);
        }
        if let Some(running) = &mut self.running {
            return running.next(deadline);
        }
        let check = self.check.as_mut()?;
        let update = check.next(deadline)?;
        self.span = check.probe_span();
        match update {
            Update::Status { ref status, .. } => {
                self.status = Some(status.clone());
                Some(update)
            }
            Update::Completed => {
                // Both cached and shared checks validate age/configuration
                // at this boundary, immediately before the turn is launched.
                self.check.take();
                let status = self.status.take();
                let error = match status.as_ref().map(|s| (s.availability, s.authentication)) {
                    Some((Availability::Available, Authentication::Authenticated)) => None,
                    Some((Availability::NotFound, _)) => {
                        Some((ErrorCode::ProviderNotFound, "EXECUTABLE_NOT_FOUND"))
                    }
                    Some((_, Authentication::Unauthenticated)) => {
                        Some((ErrorCode::ProviderNotAuthenticated, "LOGIN_REQUIRED"))
                    }
                    _ => Some((ErrorCode::ProviderFailed, "READINESS_UNVERIFIED")),
                };
                if let Some((code, reason)) = error {
                    self.turn.take();
                    return Some(Update::Failed(Failure {
                        code,
                        reason,
                        retryable: code == ErrorCode::ProviderFailed,
                    }));
                }
                self.running = Some(self.provider.send(self.turn.take().unwrap()));
                self.running.as_mut().unwrap().next(deadline)
            }
            terminal => {
                self.check.take();
                self.turn.take();
                Some(terminal)
            }
        }
    }
    fn cancel(&mut self, grace: Duration) {
        if let Some(running) = &mut self.running {
            running.cancel(grace);
        } else {
            if let Some(check) = &mut self.check {
                check.cancel(grace);
            }
            self.check.take();
            self.turn.take();
            self.stopped = true;
        }
    }
    fn probe_span(&self) -> Option<Span> {
        self.check
            .as_ref()
            .and_then(|c| c.probe_span())
            .or(self.span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy};
    use std::borrow::Cow;
    use std::cell::Cell;

    const CACHED: Freshness = Freshness::Cached { max_age_ms: 30_000 };

    struct Control {
        probes: Cell<usize>,
        fingerprints: Cell<usize>,
        sends: Cell<usize>,
        cancels: Cell<usize>,
        pending: Cell<bool>,
        key: Cell<u64>,
        authentication: Cell<Authentication>,
        availability: Cell<Availability>,
        fail_send: Cell<bool>,
        supported: Cell<bool>,
    }
    #[derive(Clone)]
    struct Fixture(Rc<Control>);
    impl Provider for Fixture {
        fn id(&self) -> &str {
            "fixture"
        }
        fn capabilities(&self) -> Capabilities {
            crate::codex::CAPABILITIES
        }
        fn timeouts(&self) -> Timeouts {
            crate::codex::LIMITS.timeouts
        }
        fn supports_preparation(&self) -> bool {
            self.0.supported.get()
        }
        fn readiness_key(&self) -> Option<Key> {
            self.0.fingerprints.set(self.0.fingerprints.get() + 1);
            Some(Key {
                files: vec![(PathBuf::from("fixture"), None, Some(self.0.key.get()))],
                capabilities: self.capabilities(),
            })
        }
        fn status(&self) -> Box<dyn Exchange> {
            self.0.probes.set(self.0.probes.get() + 1);
            Box::new(Probe {
                control: self.0.clone(),
                updates: std::collections::VecDeque::from([
                    Update::Status {
                        provider_id: self.id().into(),
                        status: ProviderState {
                            availability: self.0.availability.get(),
                            authentication: self.0.authentication.get(),
                            capabilities: self.capabilities(),
                            models: Cow::Borrowed(&[]),
                            sign_in: None,
                            readiness: None,
                        },
                    },
                    Update::Completed,
                ]),
            })
        }
        fn send(&self, _: Turn) -> Box<dyn Exchange> {
            self.0.sends.set(self.0.sends.get() + 1);
            Box::new(if self.0.fail_send.get() {
                Scripted::failed(Failure {
                    code: ErrorCode::ProviderNotAuthenticated,
                    reason: "AUTH_REJECTED",
                    retryable: false,
                })
            } else {
                Scripted::new([Update::Launched, Update::Started, Update::Completed])
            })
        }
    }
    struct Probe {
        control: Rc<Control>,
        updates: std::collections::VecDeque<Update>,
    }
    impl Exchange for Probe {
        fn next(&mut self, _: Instant) -> Option<Update> {
            if self.control.pending.get() {
                None
            } else {
                self.updates.pop_front()
            }
        }
        fn cancel(&mut self, _: Duration) {
            self.control.cancels.set(self.control.cancels.get() + 1);
        }
    }
    fn setup() -> (Ready, Rc<Control>) {
        let control = Rc::new(Control {
            probes: Cell::new(0),
            fingerprints: Cell::new(0),
            sends: Cell::new(0),
            cancels: Cell::new(0),
            pending: Cell::new(false),
            key: Cell::new(0),
            authentication: Cell::new(Authentication::Authenticated),
            availability: Cell::new(Availability::Available),
            fail_send: Cell::new(false),
            supported: Cell::new(true),
        });
        (Ready::new(Fixture(control.clone())), control)
    }
    fn drain(mut exchange: Box<dyn Exchange>) -> Vec<Update> {
        let mut result = Vec::new();
        while let Some(update) = exchange.next(Instant::now()) {
            let terminal = update.is_terminal();
            result.push(update);
            if terminal {
                break;
            }
        }
        assert!(result.last().is_some_and(Update::is_terminal), "{result:?}");
        result
    }
    fn source(updates: &[Update]) -> Source {
        updates
            .iter()
            .find_map(|u| {
                if let Update::Status { status, .. } = u {
                    status.readiness.map(|r| r.source)
                } else {
                    None
                }
            })
            .unwrap()
    }
    fn turn(check_sign_in: bool) -> Turn {
        Turn {
            system: None,
            messages: vec![Message {
                role: Role::User,
                text: "hello".into(),
            }],
            model: None,
            tools: ToolPolicy::None,
            session: SessionPolicy::Ephemeral,
            continuation: None,
            cleanup_group: None,
            check_sign_in,
        }
    }

    #[test]
    fn repeated_and_concurrent_prepare_share_only_verified_results() {
        let (ready, control) = setup();
        let first = ready.prepare(CACHED);
        let peer = ready.prepare(CACHED);
        assert_eq!(control.probes.get(), 1);
        assert_eq!(source(&drain(peer)), Source::Shared);
        assert_eq!(source(&drain(first)), Source::Fresh);
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Cached);
        assert_eq!(control.probes.get(), 1);
        assert_eq!(control.sends.get(), 0);
    }

    #[test]
    fn fresh_requests_bypass_both_cache_and_in_flight_checks() {
        let (ready, control) = setup();
        drain(ready.prepare(CACHED));
        let first = ready.readiness(Freshness::Fresh);
        let second = ready.readiness(Freshness::Fresh);
        assert_eq!(control.probes.get(), 3);
        assert_eq!(source(&drain(first)), Source::Fresh);
        assert_eq!(source(&drain(second)), Source::Fresh);
        assert_eq!(
            source(&drain(ready.send_with_readiness(turn(true), CACHED))),
            Source::Fresh
        );
        assert_eq!(control.probes.get(), 4);
        assert_eq!(
            source(&drain(ready.send_with_readiness(turn(false), CACHED))),
            Source::Cached
        );
        assert_eq!((control.probes.get(), control.sends.get()), (4, 2));
    }

    #[test]
    fn cancellation_detaches_one_subscriber_and_last_drop_stops_the_probe() {
        let (ready, control) = setup();
        control.pending.set(true);
        let mut first = ready.prepare(CACHED);
        let peer = ready.prepare(CACHED);
        first.cancel(Duration::ZERO);
        assert_eq!(first.next(Instant::now()), Some(Update::Stopped));
        assert_eq!(control.cancels.get(), 0);
        drop(peer);
        assert_eq!(control.cancels.get(), 1);
        control.pending.set(false);
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
        assert_eq!(control.probes.get(), 2);
    }

    #[test]
    fn expiration_key_changes_and_auth_failures_invalidate() {
        let (ready, control) = setup();
        drain(ready.prepare(CACHED));
        ready.state.borrow_mut().cached.as_mut().unwrap().at = Instant::now() - MAX_AGE;
        assert_eq!(
            source(&drain(ready.prepare(Freshness::Cached {
                max_age_ms: u64::MAX
            }))),
            Source::Fresh
        );
        control.key.set(1);
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
        control.fail_send.set(true);
        drain(ready.send(turn(false)));
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
        assert_eq!(control.probes.get(), 4);
    }

    #[test]
    fn delayed_cached_exchanges_revalidate_invalidation_and_configuration_before_launch() {
        let (ready, control) = setup();
        drain(ready.prepare(CACHED));
        let held = ready.prepare(CACHED);
        ready.invalidate_readiness();
        assert_eq!(source(&drain(held)), Source::Fresh);
        assert_eq!(control.probes.get(), 2);
        let mut send = ready.send_with_readiness(turn(false), CACHED);
        assert!(matches!(
            send.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        control.key.set(1);
        assert!(
            matches!(send.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_CHANGED")
        );
        assert_eq!(control.sends.get(), 0);
    }

    #[test]
    fn cached_preparation_and_send_only_fingerprint_at_consumption_boundaries() {
        let (ready, control) = setup();
        drain(ready.prepare(CACHED));
        assert_eq!(control.fingerprints.get(), 2);
        control.fingerprints.set(0);
        let check = ready.prepare(CACHED);
        assert_eq!(control.fingerprints.get(), 0);
        assert_eq!(source(&drain(check)), Source::Cached);
        assert_eq!(control.fingerprints.get(), 2);
        control.fingerprints.set(0);
        let send = ready.send_with_readiness(turn(false), CACHED);
        assert_eq!(control.fingerprints.get(), 0);
        assert_eq!(drain(send).last(), Some(&Update::Completed));
        assert_eq!(control.fingerprints.get(), 2);
        assert_eq!((control.probes.get(), control.sends.get()), (1, 1));
        control.fingerprints.set(0);
        drain(ready.send_with_readiness(turn(true), CACHED));
        assert_eq!(control.fingerprints.get(), 2);
    }

    #[test]
    fn configuration_changes_before_a_cached_status_trigger_a_fresh_probe() {
        let (ready, control) = setup();
        drain(ready.prepare(CACHED));
        let held = ready.prepare(CACHED);
        control.key.set(1);
        assert_eq!(source(&drain(held)), Source::Fresh);
        assert_eq!(control.probes.get(), 2);
    }

    #[test]
    fn shared_send_revalidates_a_peers_completed_evidence_before_launch() {
        let (ready, control) = setup();
        let mut first = ready.prepare(CACHED);
        let mut send = ready.send_with_readiness(turn(false), CACHED);
        assert!(matches!(
            first.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        assert!(matches!(
            send.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        assert_eq!(first.next(Instant::now()), Some(Update::Completed));
        control.key.set(1);
        assert!(
            matches!(send.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_CHANGED")
        );
        assert_eq!(control.sends.get(), 0);
    }

    #[test]
    fn fresh_send_returns_expired_when_status_ages_before_launch() {
        let (ready, control) = setup();
        let mut send = ready.send_with_readiness(turn(false), Freshness::Fresh);
        assert!(matches!(
            send.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        let flight = ready
            .state
            .borrow()
            .flight
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        flight.borrow_mut().observed = Some(Instant::now() - MAX_AGE);
        assert!(
            matches!(send.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_EXPIRED")
        );
        assert_eq!(control.sends.get(), 0);
    }

    #[test]
    fn cached_send_returns_expired_when_status_ages_before_launch() {
        let (ready, control) = setup();
        drain(ready.prepare(CACHED));
        let state = ready.state.borrow();
        let cached = state.cached.as_ref().unwrap();
        let mut hit = CachedHit {
            ready: ready.clone(),
            status: cached.status.clone(),
            at: cached.at,
            key: cached.key.clone(),
            freshness: CACHED,
            inner: None,
            cursor: 0,
            epoch: state.epoch,
        };
        drop(state);
        let Some(Update::Status { status, .. }) = hit.next(Instant::now()) else {
            panic!("cached status")
        };
        hit.at = Instant::now() - MAX_AGE;
        let mut send = PreparedSend {
            check: Some(Box::new(hit)),
            provider: Rc::clone(&ready.provider),
            turn: Some(turn(false)),
            running: None,
            status: Some(status),
            span: None,
            stopped: false,
        };
        assert!(
            matches!(send.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_EXPIRED")
        );
        assert_eq!(control.sends.get(), 0);
    }

    #[test]
    fn shared_completed_evidence_can_expire_while_a_subscriber_pauses() {
        let (ready, control) = setup();
        let mut first = ready.prepare(CACHED);
        let mut send = ready.send_with_readiness(turn(false), CACHED);
        assert!(matches!(
            first.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        assert!(matches!(
            send.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        assert_eq!(first.next(Instant::now()), Some(Update::Completed));
        let flight = ready
            .state
            .borrow()
            .flight
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        flight.borrow_mut().observed = Some(Instant::now() - MAX_AGE);
        assert!(
            matches!(send.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_EXPIRED")
        );
        assert_eq!(control.sends.get(), 0);
    }

    #[test]
    fn concurrent_fresh_sends_do_not_invalidate_each_others_verified_checks() {
        let (ready, control) = setup();
        let first = ready.send_with_readiness(turn(true), CACHED);
        let second = ready.send_with_readiness(turn(true), CACHED);
        for send in [first, second] {
            let updates = drain(send);
            assert_eq!(source(&updates), Source::Fresh);
            assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
        }
        assert_eq!((control.probes.get(), control.sends.get()), (2, 2));
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Cached);
    }

    #[test]
    fn explicit_invalidation_blocks_fresh_and_shared_launch_even_after_wrapper_drop() {
        for shared in [false, true] {
            for drop_wrapper in [false, true] {
                let (ready, control) = setup();
                let mut first = ready.prepare(CACHED);
                let mut send = ready.send_with_readiness(turn(!shared), CACHED);
                if shared {
                    assert!(matches!(
                        first.next(Instant::now()),
                        Some(Update::Status { .. })
                    ));
                }
                assert!(matches!(
                    send.next(Instant::now()),
                    Some(Update::Status { .. })
                ));
                if shared {
                    assert_eq!(first.next(Instant::now()), Some(Update::Completed));
                }
                ready.invalidate_readiness();
                if drop_wrapper {
                    drop(ready);
                }
                assert!(
                    matches!(send.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_CHANGED")
                );
                assert_eq!(control.sends.get(), 0);
            }
        }
    }

    #[test]
    fn one_exchanges_readiness_failure_does_not_revoke_a_peers_valid_evidence() {
        let (ready, control) = setup();
        let mut short =
            ready.send_with_readiness(turn(false), Freshness::Cached { max_age_ms: 1_000 });
        let mut peer = ready.send_with_readiness(turn(false), CACHED);
        assert!(matches!(
            short.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        assert!(matches!(
            peer.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        let flight = ready
            .state
            .borrow()
            .flight
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        flight.borrow_mut().observed = Some(Instant::now() - Duration::from_secs(2));
        assert!(
            matches!(short.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_EXPIRED")
        );
        assert_eq!(drain(peer).last(), Some(&Update::Completed));
        assert_eq!(control.sends.get(), 1);

        let (ready, control) = setup();
        let mut old = ready.send_with_readiness(turn(true), CACHED);
        assert!(matches!(
            old.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        ready.invalidate_readiness();
        let current = ready.send_with_readiness(turn(true), CACHED);
        assert!(
            matches!(old.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_CHANGED")
        );
        assert_eq!(drain(current).last(), Some(&Update::Completed));
        assert_eq!(control.sends.get(), 1);
    }

    #[test]
    fn probe_completion_does_not_extend_the_lifetime_of_old_status() {
        let (ready, _) = setup();
        let mut held = ready.prepare(CACHED);
        assert!(matches!(
            held.next(Instant::now()),
            Some(Update::Status { .. })
        ));
        let flight = ready
            .state
            .borrow()
            .flight
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        flight.borrow_mut().observed = Some(Instant::now() - MAX_AGE);
        assert!(
            matches!(held.next(Instant::now()), Some(Update::Failed(error)) if error.reason == "READINESS_EXPIRED")
        );
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
    }

    #[test]
    fn unknown_signed_out_and_unavailable_results_are_never_cached_or_sent() {
        for (availability, authentication) in [
            (Availability::Available, Authentication::Unknown),
            (Availability::Available, Authentication::Unauthenticated),
            (Availability::Unavailable, Authentication::Unknown),
            (Availability::NotFound, Authentication::Unknown),
        ] {
            let (ready, control) = setup();
            control.availability.set(availability);
            control.authentication.set(authentication);
            assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
            assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
            assert!(matches!(
                drain(ready.send_with_readiness(turn(false), CACHED)).last(),
                Some(Update::Failed(_))
            ));
            assert_eq!((control.probes.get(), control.sends.get()), (3, 0));
        }
    }

    #[test]
    fn invalidation_during_a_check_prevents_old_evidence_from_repopulating() {
        let (ready, control) = setup();
        let old = ready.prepare(CACHED);
        ready.invalidate_readiness();
        drain(old);
        assert_eq!(source(&drain(ready.prepare(CACHED))), Source::Fresh);
        assert_eq!(control.probes.get(), 2);
    }

    #[test]
    fn preparation_is_bounded_and_unsupported_adapters_consume_no_quota() {
        let (ready, control) = setup();
        control.pending.set(true);
        let timed = ready.prepare(CACHED);
        ready
            .state
            .borrow()
            .flight
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .until = Instant::now();
        assert!(
            matches!(drain(timed).last(), Some(Update::Failed(error)) if error.reason == "READINESS_TIMEOUT")
        );
        assert_eq!(control.cancels.get(), 1);
        control.supported.set(false);
        assert!(
            matches!(drain(ready.prepare(CACHED)).last(), Some(Update::Failed(error)) if error.reason == "PREPARATION_UNSUPPORTED")
        );
        assert_eq!((control.probes.get(), control.sends.get()), (1, 0));
    }

    #[test]
    fn bounded_file_fingerprints_notice_content_changes_and_creation() {
        let root =
            std::env::temp_dir().join(format!("seatline-readiness-key-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("auth.json");
        let executable = std::env::current_exe().unwrap();
        let key = || Key::watch(&executable, [config.clone()], crate::codex::CAPABILITIES);
        let missing = key().unwrap();
        std::fs::write(&config, "account-one").unwrap();
        let first = key().unwrap();
        assert!(first != missing);
        std::fs::write(&config, "account-two").unwrap();
        assert!(key().unwrap() != first);
        std::fs::write(&config, vec![0; WATCH_BYTES as usize + 1]).unwrap();
        assert!(key().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
