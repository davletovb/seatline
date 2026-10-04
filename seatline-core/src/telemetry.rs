//! Optional, privacy-safe phase timing for provider requests.
//!
//! Nothing here runs unless a host asks for it. A host that wants timings
//! creates a [`Timeline`] for a request, lets the scheduler stamp it with the
//! boundaries it observes, and hands the finished [`Record`] to a [`Sink`]. A
//! host that doesn't never creates one: the disabled path is an `Option` that
//! stays `None`, with no clock reads and no allocation.
//!
//! # Marks and phases
//!
//! A timeline holds *marks*: monotonic instants at which a boundary was
//! observed. Phases are the spans between them. Every phase is defined by two
//! marks, and the phases of a request **tile** it: they add up to the request's
//! total, with nothing counted twice and nothing left over.
//!
//! | Phase | From | To |
//! |---|---|---|
//! | `queue_wait` | `received` | `admitted` |
//! | `sign_in_probe` | probe start | probe end (inside `provider_init`'s window) |
//! | `provider_init` | `admitted` | `started`, less `sign_in_probe` |
//! | `first_text` | `started` | `first_text` |
//! | `completion` | `first_text` | `terminal` |
//! | `cleanup` | `terminal` | `released` |
//!
//! A request that ends early stops at the phase it was in: the span up to its
//! terminal update belongs to that phase, and the later phases are absent
//! rather than zero. A phase an adapter cannot observe is absent too; absence
//! never means "instant".
//!
//! `status` requests are a readiness check, so their whole run to the
//! `Status` update is `sign_in_probe`; cleanup requests (`forget`,
//! `cleanup`) run `queue_wait` and then `cleanup` for the work itself.
//!
//! # What the marks are
//!
//! A mark is taken when the host's thread *observes* the boundary, not when the
//! provider produced it, so a mark includes the host's own polling delay. That
//! is the time an application actually waits, and it means differences below
//! the polling interval cannot be resolved. All instants are
//! [`Instant`]s, so every duration is monotonic; no wall-clock time is stored.
//!
//! # What is never recorded
//!
//! Prompts, answer text, tokens, account names, file paths, provider error
//! text and native session handles never enter a timeline or a record. A record
//! holds identifiers the application chose (the request ID and the app name),
//! static provider and failure-reason names, and durations.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

use crate::exchange::Update;

/// The version of the record format. It changes only when a field is removed
/// or its meaning changes; adding a field does not.
pub const SCHEMA: u32 = 1;

/// What a request asks for, which decides how its time divides into phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A turn: `send`.
    Send,
    /// A readiness check: `status`.
    Status,
    /// File-system cleanup: `forget` and `cleanup`.
    Cleanup,
}

impl Kind {
    /// The kind of a wire method. An unknown method counts as a send: it fails
    /// before it runs, and its record says so.
    pub fn of_method(method: &str) -> Self {
        match method {
            "status" | "readiness" | "prepare" => Self::Status,
            "forget" | "cleanup" => Self::Cleanup,
            _ => Self::Send,
        }
    }
}

/// A stretch of time an exchange measured itself, because its updates do not
/// show it. Only the sign-in probe is one today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: Instant,
    /// `None` while the span is still running.
    pub end: Option<Instant>,
}

impl Span {
    pub fn begin(at: Instant) -> Self {
        Self {
            start: at,
            end: None,
        }
    }

    /// Ends the span. Only the first call counts.
    pub fn finish(&mut self, at: Instant) {
        self.end.get_or_insert(at);
    }
}

/// The boundaries of one request, in the order the host observed them.
///
/// Every mark is set once: a second call for the same boundary changes nothing,
/// so a repeated update can never move an earlier measurement.
#[derive(Debug, Clone)]
pub struct Timeline {
    kind: Kind,
    received: Instant,
    admitted: Option<Instant>,
    built: Option<Instant>,
    probe: Option<Span>,
    launched: Option<Instant>,
    started: Option<Instant>,
    status: Option<Instant>,
    status_probed: bool,
    first_text: Option<Instant>,
    stop_requested: Option<Instant>,
    terminal: Option<Instant>,
    released: Option<Instant>,
}

impl Timeline {
    /// A timeline for a request the host received at `received`.
    pub fn new(kind: Kind, received: Instant) -> Self {
        Self {
            kind,
            received,
            admitted: None,
            built: None,
            probe: None,
            launched: None,
            started: None,
            status: None,
            status_probed: false,
            first_text: None,
            stop_requested: None,
            terminal: None,
            released: None,
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The request left the queue and the adapter was asked to build its
    /// exchange.
    pub fn admitted(&mut self, at: Instant) {
        self.admitted.get_or_insert(at);
    }

    /// The adapter returned its exchange. Between `admitted` and this, the
    /// host's thread was busy building it: finding the executable, preparing
    /// the workspace, and starting the first process.
    pub fn built(&mut self, at: Instant) {
        self.built.get_or_insert(at);
    }

    /// The sign-in probe the exchange ran, as it reported it.
    pub fn set_probe(&mut self, span: Span) {
        self.probe.get_or_insert(span);
    }

    /// Whether `update` would set a mark that is not set yet. A host that
    /// reads the clock for [`Timeline::observe`] can ask first, so that a
    /// flood of progress, or every delta after the first, costs no clock read.
    pub fn wants(&self, update: &Update) -> bool {
        match update {
            Update::Launched => self.launched.is_none(),
            Update::Started => self.started.is_none(),
            Update::Delta(text) => !text.is_empty() && self.first_text.is_none(),
            Update::Status { .. } => self.status.is_none(),
            Update::Completed | Update::Failed(_) | Update::Stopped => self.terminal.is_none(),
            _ => false,
        }
    }

    /// Stamps what an update shows, when the host observed it at `at`.
    pub fn observe(&mut self, update: &Update, at: Instant) {
        match update {
            Update::Launched => {
                self.launched.get_or_insert(at);
            }
            Update::Started => {
                self.started.get_or_insert(at);
            }
            Update::Delta(text) if !text.is_empty() => {
                self.first_text.get_or_insert(at);
            }
            Update::Status { status, .. } => {
                if self.status.is_none() {
                    self.status_probed = status
                        .readiness
                        .is_none_or(|r| r.source == crate::readiness::Source::Fresh);
                }
                self.status.get_or_insert(at);
            }
            Update::Completed | Update::Failed(_) | Update::Stopped => {
                self.terminal.get_or_insert(at);
            }
            _ => {}
        }
    }

    /// The host asked the request to stop: a cancel, a timeout, or a shutdown.
    pub fn stop_requested(&mut self, at: Instant) {
        self.stop_requested.get_or_insert(at);
    }

    /// The request ended. For one that ended without a terminal update, such
    /// as a process that had to be abandoned, this is when the host gave up.
    pub fn terminal(&mut self, at: Instant) {
        self.terminal.get_or_insert(at);
    }

    /// The exchange was dropped, which kills and reaps its process.
    pub fn released(&mut self, at: Instant) {
        self.released.get_or_insert(at);
    }

    /// Whether an exchange's terminal update has been observed.
    pub fn has_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Whether any answer text was shown.
    pub fn text(&self) -> bool {
        self.first_text.is_some()
    }

    /// Readiness checks this request ran: the sign-in probe of a send, or the
    /// check a status request is.
    pub fn probes(&self) -> u8 {
        match self.kind {
            Kind::Send => u8::from(self.probe.is_some()),
            Kind::Status => u8::from(self.status_probed || self.probe.is_some()),
            Kind::Cleanup => 0,
        }
    }

    /// Provider processes this request launched for its turn.
    pub fn launches(&self) -> u8 {
        u8::from(self.launched.is_some())
    }

    /// Every mark, as microseconds since `received`.
    pub fn marks(&self) -> Marks {
        let at = |mark: Option<Instant>| mark.map(|at| micros(self.received, at));
        Marks {
            admitted: at(self.admitted),
            built: at(self.built),
            probe_started: at(self.probe.map(|span| span.start)),
            probe_ended: at(self.probe.and_then(|span| span.end)),
            launched: at(self.launched),
            started: at(self.started),
            status: at(self.status),
            first_text: at(self.first_text),
            stop_requested: at(self.stop_requested),
            terminal: at(self.terminal),
            released: at(self.released),
        }
    }

    /// The request's whole run: until its process was released, or until it
    /// ended if nothing was released.
    pub fn total_us(&self) -> u64 {
        self.marks().total()
    }

    /// The phases the marks show, in microseconds. They add up to
    /// [`Timeline::total_us`] exactly, because both come from the same
    /// truncated marks.
    pub fn phases(&self) -> Phases {
        let mut phases = self.marks().phases(self.kind);
        if self.kind == Kind::Status && self.status.is_some() && !self.status_probed {
            phases.provider_init = phases.sign_in_probe.take();
        }
        phases
    }
}

/// `to` minus `from` in whole microseconds, never negative.
fn micros(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_micros()).unwrap_or(u64::MAX)
}

macro_rules! sparse {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$field_meta:meta])* $field:ident),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
        pub struct $name {
            $(
                $(#[$field_meta])*
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $field: Option<u64>,
            )*
        }
    };
}

sparse! {
    /// When each boundary was observed, in microseconds since the request was
    /// received. A boundary that never happened is absent.
    pub struct Marks {
        /// The request left the queue.
        admitted,
        /// The adapter returned its exchange.
        built,
        /// The sign-in probe began.
        probe_started,
        /// The sign-in probe ended, however it ended.
        probe_ended,
        /// The provider process for the turn was started.
        launched,
        /// The provider accepted the turn.
        started,
        /// A status request's answer arrived.
        status,
        /// The first answer text was shown.
        first_text,
        /// A cancel, timeout or shutdown was requested.
        stop_requested,
        /// The request's terminal update was observed.
        terminal,
        /// The exchange was dropped and its process reaped.
        released,
    }
}

impl Marks {
    /// The request's whole run: until its process was released, or until it
    /// ended if nothing was released.
    pub fn total(&self) -> u64 {
        self.released.or(self.terminal).unwrap_or(0)
    }

    /// The phases these marks show for a request of `kind`. Working from the
    /// marks as recorded, rather than from the instants they came from, keeps
    /// the sum of the phases equal to [`Marks::total`] to the microsecond, and
    /// lets a reader of a record recompute them.
    pub fn phases(&self, kind: Kind) -> Phases {
        let mut phases = Phases::default();
        let terminal = self.terminal;

        // Waiting to be admitted. A request that never was ended in the queue.
        let Some(admitted) = self.admitted else {
            phases.queue_wait = terminal;
            phases.cleanup = between(terminal, self.released);
            return phases;
        };
        phases.queue_wait = Some(admitted);

        match kind {
            Kind::Cleanup => {
                // The work, and the drop of its exchange when the scheduler ran
                // it, which is in the total.
                phases.cleanup = between(Some(admitted), self.released.or(terminal));
            }
            Kind::Status => {
                phases.sign_in_probe = between(Some(admitted), self.status.or(terminal));
                if self.status.is_some() {
                    phases.completion = between(self.status, terminal);
                }
                phases.cleanup = between(terminal, self.released);
            }
            Kind::Send => {
                // The window from admission to the provider's start-of-turn
                // holds the probe; what is left of it is initialization.
                if let Some(init_end) = self.started.or(terminal) {
                    let window = init_end.saturating_sub(admitted);
                    let probe = self.probe_started.map(|start| {
                        init_end
                            .min(self.probe_ended.unwrap_or(init_end))
                            .saturating_sub(start)
                            .min(window)
                    });
                    phases.sign_in_probe = probe;
                    phases.provider_init = Some(window - probe.unwrap_or(0));
                }
                if self.started.is_some() {
                    phases.first_text = between(self.started, self.first_text.or(terminal));
                    if self.first_text.is_some() {
                        phases.completion = between(self.first_text, terminal);
                    }
                }
                phases.cleanup = between(terminal, self.released);
            }
        }
        phases
    }
}

/// `to` minus `from`, when both are known, and never negative.
fn between(from: Option<u64>, to: Option<u64>) -> Option<u64> {
    Some(to?.saturating_sub(from?))
}

sparse! {
    /// The phases of a request, in microseconds. See the module documentation
    /// for what each one spans.
    pub struct Phases {
        queue_wait,
        sign_in_probe,
        provider_init,
        first_text,
        completion,
        cleanup,
    }
}

impl Phases {
    /// The sum of the phases that are present.
    pub fn sum(&self) -> u64 {
        [
            self.queue_wait,
            self.sign_in_probe,
            self.provider_init,
            self.first_text,
            self.completion,
            self.cleanup,
        ]
        .into_iter()
        .flatten()
        .fold(0, u64::saturating_add)
    }
}

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    /// The provider or the runtime failed it. `detail` names the reason.
    Failed,
    /// Stopped on request: a cancel, a closed connection, or a revoked grant.
    /// The `stop_requested` mark says when the stop was asked for.
    Cancelled,
    /// Stopped by a limit. `detail` is `start`, `idle` or `absolute`.
    TimedOut,
    /// Ended by a bug: a panicking adapter or scheduler, or a process that
    /// stopped by itself. `detail` says which.
    Aborted,
}

/// Which request a record describes. Everything here was chosen by the
/// application or by the runtime's own configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The broker's own number for the connection; it restarts with the
    /// broker.
    pub connection: u64,
    /// The request ID the application chose. Applications that turn telemetry
    /// on should not put private data in request IDs.
    pub request: String,
    pub app: String,
    pub provider: String,
    pub method: String,
}

/// One request's phases, outcome and counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestRecord {
    pub schema: u32,
    pub connection: u64,
    pub request: String,
    pub app: String,
    pub provider: String,
    pub method: String,
    pub outcome: Outcome,
    /// The failure reason, timeout limit, or abort cause. Always a static
    /// name, never provider output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'static str>,
    /// Whether any answer text was shown.
    pub text: bool,
    /// Readiness checks the request ran.
    pub probes: u8,
    /// Provider processes launched for the turn itself.
    pub launches: u8,
    pub marks_us: Marks,
    pub phases_us: Phases,
    pub total_us: u64,
}

impl RequestRecord {
    pub fn new(
        identity: Identity,
        timeline: &Timeline,
        outcome: Outcome,
        detail: Option<&'static str>,
    ) -> Self {
        Self {
            schema: SCHEMA,
            connection: identity.connection,
            request: identity.request,
            app: identity.app,
            provider: identity.provider,
            method: identity.method,
            outcome,
            detail,
            text: timeline.text(),
            probes: timeline.probes(),
            launches: timeline.launches(),
            marks_us: timeline.marks(),
            phases_us: timeline.phases(),
            total_us: timeline.total_us(),
        }
    }
}

/// How long a connection took to authenticate, on the broker's side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionRecord {
    pub schema: u32,
    pub connection: u64,
    pub app: String,
    /// From the broker reading the authentication frame to `ready` being on
    /// the wire. It leaves out the client's own connect and its wait for the
    /// answer.
    pub handshake_us: u64,
}

/// What a broker ran with. Written once, when it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokerRecord {
    pub schema: u32,
    pub version: &'static str,
    pub protocol: u32,
    pub os: &'static str,
    pub arch: &'static str,
    /// The broker's effective limits and intervals.
    pub limits: BTreeMap<&'static str, u64>,
}

/// One line of telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Request(Box<RequestRecord>),
    Connection(ConnectionRecord),
    Broker(BrokerRecord),
    /// Records that could not be kept, because the sink was full. Written
    /// before the next record that is kept.
    Dropped {
        schema: u32,
        count: u64,
    },
}

/// Where records go. A sink must not block: it is called from the thread that
/// serves requests.
pub trait Sink: Send + Sync {
    fn record(&self, record: Record);
}

/// A sink that keeps records in memory, for tests and in-process hosts.
#[derive(Default)]
pub struct Memory {
    records: Mutex<Vec<Record>>,
}

impl Memory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes everything recorded so far.
    pub fn take(&self) -> Vec<Record> {
        std::mem::take(&mut *self.records.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Sink for Memory {
    fn record(&self, record: Record) {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(record);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::protocol::{ErrorCode, Failure};

    /// Instants that no clock decides: `base` plus a number of microseconds.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Self(Instant::now())
        }

        fn at(&self, micros: u64) -> Instant {
            self.0 + Duration::from_micros(micros)
        }
    }

    fn delta(text: &str) -> Update {
        Update::Delta(text.to_owned())
    }

    fn failed() -> Update {
        Update::Failed(Failure {
            code: ErrorCode::ProviderFailed,
            reason: "PROVIDER_FAILED",
            retryable: false,
        })
    }

    /// A send that ran every phase, with a probe inside its initialization.
    fn full_send(clock: &Clock) -> Timeline {
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.admitted(clock.at(100));
        timeline.built(clock.at(150));
        let mut probe = Span::begin(clock.at(120));
        probe.finish(clock.at(520));
        timeline.set_probe(probe);
        timeline.observe(&Update::Launched, clock.at(600));
        timeline.observe(&Update::Started, clock.at(900));
        timeline.observe(&delta("hi"), clock.at(1_000));
        timeline.observe(&Update::Completed, clock.at(1_300));
        timeline.released(clock.at(1_350));
        timeline
    }

    #[test]
    fn the_phases_of_a_full_send_tile_its_total() {
        let clock = Clock::new();
        let timeline = full_send(&clock);
        let phases = timeline.phases();
        assert_eq!(phases.queue_wait, Some(100));
        assert_eq!(phases.sign_in_probe, Some(400));
        // 100..900 is 800, less the 400 the probe took.
        assert_eq!(phases.provider_init, Some(400));
        assert_eq!(phases.first_text, Some(100));
        assert_eq!(phases.completion, Some(300));
        assert_eq!(phases.cleanup, Some(50));
        assert_eq!(phases.sum(), timeline.total_us());
        assert_eq!(timeline.total_us(), 1_350);
        assert_eq!((timeline.probes(), timeline.launches()), (1, 1));
        assert!(timeline.text());
    }

    #[test]
    fn marks_are_offsets_from_receipt_and_missing_marks_are_absent() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(1_000));
        timeline.admitted(clock.at(1_250));
        let marks = timeline.marks();
        assert_eq!(marks.admitted, Some(250));
        assert_eq!(marks.started, None);
        let json = serde_json::to_value(marks).unwrap();
        assert_eq!(json, serde_json::json!({"admitted": 250}));
    }

    #[test]
    fn a_send_without_a_probe_has_no_probe_phase() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.admitted(clock.at(10));
        timeline.observe(&Update::Launched, clock.at(20));
        timeline.observe(&Update::Started, clock.at(50));
        timeline.observe(&delta("x"), clock.at(60));
        timeline.observe(&Update::Completed, clock.at(70));
        timeline.released(clock.at(70));
        let phases = timeline.phases();
        assert_eq!(phases.sign_in_probe, None);
        assert_eq!(phases.provider_init, Some(40));
        assert_eq!(phases.cleanup, Some(0));
        assert_eq!(phases.sum(), timeline.total_us());
        assert_eq!(timeline.probes(), 0);
    }

    #[test]
    fn a_request_that_ends_during_initialization_stops_there() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.admitted(clock.at(10));
        timeline.observe(&Update::Launched, clock.at(30));
        timeline.observe(&failed(), clock.at(90));
        timeline.released(clock.at(95));
        let phases = timeline.phases();
        assert_eq!(phases.provider_init, Some(80));
        assert_eq!(phases.first_text, None);
        assert_eq!(phases.completion, None);
        assert_eq!(phases.cleanup, Some(5));
        assert_eq!(phases.sum(), timeline.total_us());
        assert!(!timeline.text());
    }

    #[test]
    fn a_probe_that_was_still_running_when_the_request_ended_ends_with_it() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.admitted(clock.at(10));
        timeline.set_probe(Span::begin(clock.at(20)));
        timeline.observe(&Update::Stopped, clock.at(120));
        timeline.released(clock.at(125));
        let phases = timeline.phases();
        assert_eq!(phases.sign_in_probe, Some(100));
        // The 10 microseconds before the probe began are initialization.
        assert_eq!(phases.provider_init, Some(10));
        assert_eq!(phases.sum(), timeline.total_us());
    }

    #[test]
    fn an_answer_with_no_text_spends_its_run_waiting_for_text() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.admitted(clock.at(5));
        timeline.observe(&Update::Started, clock.at(25));
        timeline.observe(&delta(""), clock.at(30));
        timeline.observe(&Update::Completed, clock.at(80));
        timeline.released(clock.at(80));
        let phases = timeline.phases();
        assert!(!timeline.text(), "an empty delta is not visible text");
        assert_eq!(phases.first_text, Some(55));
        assert_eq!(phases.completion, None);
        assert_eq!(phases.sum(), timeline.total_us());
    }

    #[test]
    fn a_request_cancelled_in_the_queue_only_waited() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.observe(&Update::Stopped, clock.at(700));
        let phases = timeline.phases();
        assert_eq!(phases.queue_wait, Some(700));
        assert_eq!(phases.sum(), 700);
        assert_eq!(timeline.total_us(), 700);
        assert_eq!(phases.provider_init, None);
    }

    /// A status update, whose contents do not matter to the timeline.
    fn status_update() -> Update {
        Update::Status {
            provider_id: "codex".into(),
            status: crate::protocol::ProviderState {
                availability: crate::protocol::Availability::Available,
                authentication: crate::protocol::Authentication::Authenticated,
                capabilities: crate::protocol::Capabilities {
                    streaming: crate::protocol::Capability::Supported,
                    continuation: crate::protocol::Capability::Supported,
                    web_search: crate::protocol::Capability::Supported,
                    model_selection: crate::protocol::Capability::Supported,
                    cancellation: crate::protocol::Capability::Supported,
                    tool_isolation: crate::protocol::Capability::Supported,
                },
                models: std::borrow::Cow::Borrowed(&[]),
                sign_in: None,
                readiness: None,
            },
        }
    }

    #[test]
    fn cached_and_shared_readiness_count_no_probe_and_tile_local_work() {
        let clock = Clock::new();
        for source in [
            crate::readiness::Source::Cached,
            crate::readiness::Source::Shared,
        ] {
            let mut update = status_update();
            if let Update::Status { status, .. } = &mut update {
                status.readiness = Some(crate::readiness::Readiness { source, age_ms: 0 });
            }
            let mut timeline = Timeline::new(Kind::Status, clock.at(0));
            timeline.admitted(clock.at(2));
            timeline.observe(&update, clock.at(10));
            timeline.terminal(clock.at(12));
            timeline.released(clock.at(15));
            assert_eq!(timeline.probes(), 0);
            assert_eq!(timeline.phases().sign_in_probe, None);
            assert_eq!(timeline.phases().provider_init, Some(8));
            assert_eq!(timeline.phases().sum(), timeline.total_us());
        }
    }

    #[test]
    fn a_status_request_is_one_readiness_check() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Status, clock.at(0));
        timeline.admitted(clock.at(40));
        let status = status_update();
        timeline.observe(&status, clock.at(1_040));
        timeline.observe(&Update::Completed, clock.at(1_050));
        timeline.released(clock.at(1_060));
        let phases = timeline.phases();
        assert_eq!(phases.queue_wait, Some(40));
        assert_eq!(phases.sign_in_probe, Some(1_000));
        assert_eq!(phases.provider_init, None);
        assert_eq!(phases.completion, Some(10));
        assert_eq!(phases.cleanup, Some(10));
        assert_eq!(phases.sum(), timeline.total_us());
        assert_eq!((timeline.probes(), timeline.launches()), (1, 0));
    }

    #[test]
    fn a_cleanup_turn_the_scheduler_runs_counts_the_drop_of_its_exchange_too() {
        // A host can run a cleanup under the scheduler, which drops the exchange
        // after its terminal update: that time is in the total, so it is in the
        // cleanup phase, and the phases still add up.
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Cleanup, clock.at(0));
        timeline.admitted(clock.at(300));
        timeline.observe(&Update::Completed, clock.at(310));
        timeline.released(clock.at(2_395));
        let phases = timeline.phases();
        assert_eq!(phases.queue_wait, Some(300));
        assert_eq!(
            phases.cleanup,
            Some(2_095),
            "work and release, from admission"
        );
        assert_eq!(phases.sum(), timeline.total_us());
        assert_eq!(timeline.total_us(), 2_395);
    }

    #[test]
    fn a_cleanup_request_waits_and_then_works() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Cleanup, clock.at(0));
        timeline.admitted(clock.at(300));
        timeline.observe(&Update::Completed, clock.at(900));
        let phases = timeline.phases();
        assert_eq!(phases.queue_wait, Some(300));
        assert_eq!(phases.cleanup, Some(600));
        assert_eq!(phases.sum(), timeline.total_us());
        assert_eq!(timeline.total_us(), 900);
    }

    #[test]
    fn a_mark_is_set_once() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.admitted(clock.at(10));
        timeline.admitted(clock.at(99));
        timeline.observe(&Update::Started, clock.at(20));
        timeline.observe(&Update::Started, clock.at(88));
        timeline.observe(&delta("a"), clock.at(30));
        timeline.observe(&delta("b"), clock.at(77));
        timeline.observe(&Update::Completed, clock.at(40));
        timeline.observe(&failed(), clock.at(66));
        timeline.stop_requested(clock.at(35));
        timeline.stop_requested(clock.at(36));
        let marks = timeline.marks();
        assert_eq!(marks.admitted, Some(10));
        assert_eq!(marks.started, Some(20));
        assert_eq!(marks.first_text, Some(30));
        assert_eq!(marks.terminal, Some(40));
        assert_eq!(marks.stop_requested, Some(35));
    }

    #[test]
    fn a_timeline_only_wants_updates_that_would_set_a_mark() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        for update in [
            Update::Launched,
            Update::Started,
            delta("a"),
            Update::Completed,
        ] {
            assert!(timeline.wants(&update), "{update:?} is a new boundary");
            timeline.observe(&update, clock.at(1));
            assert!(!timeline.wants(&update), "{update:?} was already marked");
        }
        for update in [
            Update::Activity,
            delta(""),
            Update::Session("native".into()),
            failed(),
            Update::Stopped,
        ] {
            assert!(!timeline.wants(&update), "{update:?}");
        }
    }

    #[test]
    fn updates_that_are_not_boundaries_leave_no_mark() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        for update in [
            Update::Activity,
            Update::Session("native".into()),
            Update::SessionLost(crate::exchange::SessionLoss::Suspected),
        ] {
            timeline.observe(&update, clock.at(10));
        }
        assert_eq!(timeline.marks(), Marks::default());
    }

    #[test]
    fn phases_tile_every_shape_of_request() {
        let clock = Clock::new();
        let mut shapes = vec![full_send(&clock)];
        for end in [Update::Completed, failed(), Update::Stopped] {
            for stop_after in 0..=5 {
                let mut timeline = Timeline::new(Kind::Send, clock.at(0));
                let marks: [&dyn Fn(&mut Timeline); 5] = [
                    &|t| t.admitted(clock.at(10)),
                    &|t| t.set_probe(Span::begin(clock.at(12))),
                    &|t| t.observe(&Update::Started, clock.at(40)),
                    &|t| t.observe(&delta("x"), clock.at(60)),
                    &|_| {},
                ];
                for mark in marks.iter().take(stop_after) {
                    mark(&mut timeline);
                }
                timeline.observe(&end, clock.at(90));
                timeline.released(clock.at(100));
                shapes.push(timeline);
            }
        }
        for timeline in shapes {
            assert_eq!(
                timeline.phases().sum(),
                timeline.total_us(),
                "{timeline:?} does not tile"
            );
        }
    }

    #[test]
    fn phases_tile_every_kind_of_request_however_far_it_got() {
        let clock = Clock::new();
        for kind in [Kind::Send, Kind::Status, Kind::Cleanup] {
            // Which boundaries a request reached, in the order it can reach them.
            for admitted in [false, true] {
                for reached in 0..=3 {
                    for released in [false, true] {
                        let mut timeline = Timeline::new(kind, clock.at(0));
                        if admitted {
                            timeline.admitted(clock.at(10));
                        }
                        let boundaries = [Update::Started, status_update(), delta("text")];
                        for (index, update) in boundaries.iter().take(reached).enumerate() {
                            timeline.observe(update, clock.at(20 + 10 * index as u64));
                        }
                        timeline.observe(&Update::Completed, clock.at(70));
                        if released {
                            timeline.released(clock.at(95));
                        }
                        assert_eq!(
                            timeline.phases().sum(),
                            timeline.total_us(),
                            "{kind:?} admitted={admitted} reached={reached} released={released}: {:?}",
                            timeline.marks()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_record_has_exactly_the_documented_fields_and_no_content() {
        let clock = Clock::new();
        let record = RequestRecord::new(
            Identity {
                connection: 7,
                request: "req-1".into(),
                app: "app-a".into(),
                provider: "codex".into(),
                method: "send".into(),
            },
            &full_send(&clock),
            Outcome::Completed,
            None,
        );
        let json = serde_json::to_value(Record::Request(Box::new(record))).unwrap();
        let object = json.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "app",
                "connection",
                "kind",
                "launches",
                "marks_us",
                "method",
                "outcome",
                "phases_us",
                "probes",
                "provider",
                "request",
                "schema",
                "text",
                "total_us"
            ]
        );
        assert_eq!(object["kind"], "request");
        assert_eq!(object["outcome"], "completed");
        assert_eq!(object["schema"], SCHEMA);
    }

    #[test]
    fn a_failure_record_carries_a_static_reason_only() {
        let clock = Clock::new();
        let mut timeline = Timeline::new(Kind::Send, clock.at(0));
        timeline.observe(&failed(), clock.at(5));
        let record = RequestRecord::new(
            Identity {
                connection: 1,
                request: "r".into(),
                app: "a".into(),
                provider: "codex".into(),
                method: "send".into(),
            },
            &timeline,
            Outcome::Failed,
            Some("QUEUE_FULL"),
        );
        let json = serde_json::to_value(Record::Request(Box::new(record))).unwrap();
        assert_eq!(json["detail"], "QUEUE_FULL");
        assert_eq!(json["outcome"], "failed");
    }

    #[test]
    fn the_other_records_name_their_kind() {
        let handshake = serde_json::to_value(Record::Connection(ConnectionRecord {
            schema: SCHEMA,
            connection: 3,
            app: "a".into(),
            handshake_us: 120,
        }))
        .unwrap();
        assert_eq!(handshake["kind"], "connection");
        assert_eq!(handshake["handshake_us"], 120);
        let dropped = serde_json::to_value(Record::Dropped {
            schema: SCHEMA,
            count: 4,
        })
        .unwrap();
        assert_eq!(
            dropped,
            serde_json::json!({"kind":"dropped","schema":1,"count":4})
        );
    }

    #[test]
    fn memory_hands_records_over_once() {
        let memory = Memory::new();
        memory.record(Record::Dropped {
            schema: SCHEMA,
            count: 1,
        });
        assert_eq!(memory.take().len(), 1);
        assert!(memory.take().is_empty());
    }
}
