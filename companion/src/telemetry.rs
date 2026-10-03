//! The broker's side of phase telemetry (see [`seatline_core::telemetry`]).
//!
//! Telemetry is off unless the broker is started with `SEATLINE_TELEMETRY_FILE`
//! naming a file, or a host passes its own [`Sink`] to [`crate::hub::start_with`].
//! When it is off the hub holds no timelines and takes no extra clock reads.
//!
//! The file is JSON lines, one [`Record`] per line, appended through a bounded
//! queue and written by a thread of its own, so a slow disk can cost records
//! but never a request's time.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use seatline_core::exchange::Update;
use seatline_core::telemetry::{
    BrokerRecord, ConnectionRecord, Identity, Kind, Outcome, Record, RequestRecord, SCHEMA, Sink,
    Timeline,
};
use seatline_scheduler::{EndReason, Supervisor, TimeoutKind, TurnId};

/// The environment variable that turns on the broker's telemetry file.
pub const FILE_VARIABLE: &str = "SEATLINE_TELEMETRY_FILE";

/// Records waiting for the writer thread. A full queue drops records.
const QUEUE: usize = 1024;
/// The telemetry file stops growing at this size.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Requests whose timelines the hub keeps while they wait. Past it, new
/// requests go untimed rather than letting a flood grow the map.
const MAX_WAITING: usize = 4096;

/// A sink that appends JSON lines to a file.
pub struct JsonLines {
    records: SyncSender<Record>,
    dropped: Arc<AtomicU64>,
}

impl JsonLines {
    /// Opens `path` for appending, creating it readable only by its owner where
    /// the platform has such permissions.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let written = file.metadata()?.len();
        let (records, queue) = mpsc::sync_channel(QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = Arc::clone(&dropped);
        std::thread::spawn(move || write_lines(file, written, queue, &lost));
        Ok(Self { records, dropped })
    }
}

impl Sink for JsonLines {
    fn record(&self, record: Record) {
        if self.records.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn write_lines(
    mut file: File,
    mut written: u64,
    queue: mpsc::Receiver<Record>,
    dropped: &AtomicU64,
) {
    for record in queue {
        let lost = dropped.swap(0, Ordering::Relaxed);
        let mut batch = Vec::new();
        if lost > 0 {
            batch.push(Record::Dropped {
                schema: SCHEMA,
                count: lost,
            });
        }
        batch.push(record);
        for record in batch {
            let Ok(mut line) = serde_json::to_vec(&record) else {
                continue;
            };
            line.push(b'\n');
            if written + line.len() as u64 > MAX_FILE_BYTES {
                // The file is full: later records are lost, and said to be.
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if file.write_all(&line).is_ok() {
                written += line.len() as u64;
            }
        }
    }
}

/// The sink the environment asks for, if any. A file that cannot be opened is
/// reported on standard error and leaves telemetry off: it must never keep the
/// broker from serving.
pub fn from_env() -> Option<Arc<dyn Sink>> {
    let path = std::env::var_os(FILE_VARIABLE).filter(|path| !path.is_empty())?;
    match JsonLines::open(Path::new(&path)) {
        Ok(sink) => Some(Arc::new(sink)),
        Err(error) => {
            eprintln!("Seatline: telemetry is off, cannot open {FILE_VARIABLE}: {error}");
            None
        }
    }
}

/// What a broker ran with, as the record its telemetry starts with.
pub fn broker_record(limits: std::collections::BTreeMap<&'static str, u64>) -> Record {
    Record::Broker(BrokerRecord {
        schema: SCHEMA,
        version: env!("CARGO_PKG_VERSION"),
        protocol: crate::PROTOCOL_VERSION,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        limits,
    })
}

/// A connection's handshake, on the broker's side.
pub fn handshake_record(connection: u64, app: String, took: Duration) -> Record {
    Record::Connection(ConnectionRecord {
        schema: SCHEMA,
        connection,
        app,
        handshake_us: u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
    })
}

/// How a scheduler ending maps to a record's outcome and detail.
pub fn outcome_of(reason: &EndReason) -> (Outcome, Option<&'static str>) {
    match reason {
        EndReason::Completed => (Outcome::Completed, None),
        EndReason::Failed(failure) => (Outcome::Failed, Some(failure.reason)),
        EndReason::Cancelled => (Outcome::Cancelled, None),
        EndReason::Timeout(TimeoutKind::Start) => (Outcome::TimedOut, Some("start")),
        EndReason::Timeout(TimeoutKind::Idle) => (Outcome::TimedOut, Some("idle")),
        EndReason::Timeout(TimeoutKind::Absolute) => (Outcome::TimedOut, Some("absolute")),
        EndReason::StoppedUnexpectedly => (Outcome::Aborted, Some("stopped_unexpectedly")),
        EndReason::AdapterPanicked { .. } => (Outcome::Aborted, Some("adapter_panicked")),
        EndReason::SchedulerPanicked { .. } => (Outcome::Aborted, Some("scheduler_panicked")),
    }
}

/// How a request that never reached the scheduler ended, from its terminal
/// update: a refusal, a cancel while queued, or finished cleanup work.
fn outcome_of_update(update: &Update) -> (Outcome, Option<&'static str>) {
    match update {
        Update::Completed => (Outcome::Completed, None),
        Update::Failed(failure) => (Outcome::Failed, Some(failure.reason)),
        _ => (Outcome::Cancelled, None),
    }
}

struct Waiting {
    identity: Identity,
    timeline: Timeline,
}

/// The hub's bookkeeping for the timelines of requests in flight.
///
/// A request's timeline lives in one of three places: here while it waits in
/// the queue (or runs cleanup work on the hub's own thread), in the scheduler
/// while its exchange runs, and nowhere once its record has been written.
pub struct Telemetry {
    sink: Option<Arc<dyn Sink>>,
    waiting: HashMap<(u64, String), Waiting>,
    scheduled: HashMap<TurnId, Identity>,
}

impl Telemetry {
    pub fn disabled() -> Self {
        Self {
            sink: None,
            waiting: HashMap::new(),
            scheduled: HashMap::new(),
        }
    }

    pub fn new(sink: Arc<dyn Sink>) -> Self {
        Self {
            sink: Some(sink),
            ..Self::disabled()
        }
    }

    pub fn enabled(&self) -> bool {
        self.sink.is_some()
    }

    /// A request reached the hub.
    ///
    /// The method is whatever the client sent, so only the methods the broker
    /// serves are kept by name: anything else is `unknown`, and no record can
    /// carry text a client chose beyond its request ID.
    pub fn received(&mut self, connection: u64, id: &str, app: &str, provider: &str, method: &str) {
        if !self.enabled() || self.waiting.len() >= MAX_WAITING {
            return;
        }
        let method = match method {
            "send" | "status" | "forget" | "cleanup" => method,
            _ => "unknown",
        };
        self.waiting.insert(
            (connection, id.to_owned()),
            Waiting {
                identity: Identity {
                    connection,
                    request: id.to_owned(),
                    app: app.to_owned(),
                    provider: provider.to_owned(),
                    method: method.to_owned(),
                },
                timeline: Timeline::new(Kind::of_method(method), Instant::now()),
            },
        );
    }

    /// The request left the queue.
    pub fn admitted(&mut self, connection: u64, id: &str) {
        if !self.enabled() {
            return;
        }
        if let Some(waiting) = self.waiting.get_mut(&(connection, id.to_owned())) {
            waiting.timeline.admitted(Instant::now());
        }
    }

    /// The adapter returned its exchange: the timeline goes to the scheduler,
    /// which will stamp the rest. Hand the turn it started back to
    /// [`Telemetry::scheduled`].
    pub fn hand_off(&mut self, connection: u64, id: &str) -> Option<(Identity, Timeline)> {
        if !self.enabled() {
            return None;
        }
        let mut waiting = self.waiting.remove(&(connection, id.to_owned()))?;
        waiting.timeline.built(Instant::now());
        Some((waiting.identity, waiting.timeline))
    }

    /// The scheduler is running `identity`'s request as `turn`.
    pub fn scheduled(&mut self, turn: TurnId, identity: Identity) {
        self.scheduled.insert(turn, identity);
    }

    /// A scheduled turn ended: write its record.
    pub fn ended(&mut self, turn: TurnId, reason: &EndReason, supervisor: &mut Supervisor) {
        if !self.enabled() {
            return;
        }
        let identity = self.scheduled.remove(&turn);
        let Some(timeline) = supervisor.take_timeline(turn) else {
            return;
        };
        let Some(identity) = identity else {
            return;
        };
        let (outcome, detail) = outcome_of(reason);
        self.write(RequestRecord::new(identity, &timeline, outcome, detail));
    }

    /// A request that never reached the scheduler ended with `update`: it was
    /// refused, cancelled in the queue, or finished its cleanup work.
    pub fn unscheduled(&mut self, connection: u64, id: &str, update: &Update) {
        if !self.enabled() {
            return;
        }
        let Some(mut waiting) = self.waiting.remove(&(connection, id.to_owned())) else {
            return;
        };
        waiting.timeline.observe(update, Instant::now());
        waiting.timeline.terminal(Instant::now());
        let (outcome, detail) = outcome_of_update(update);
        self.write(RequestRecord::new(
            waiting.identity,
            &waiting.timeline,
            outcome,
            detail,
        ));
    }

    /// A queued request was dropped because its connection closed.
    pub fn abandoned(&mut self, connection: u64, id: &str) {
        self.unscheduled(connection, id, &Update::Stopped);
    }

    fn write(&self, record: RequestRecord) {
        if let Some(sink) = &self.sink {
            sink.record(Record::Request(Box::new(record)));
        }
    }

    /// Requests whose timelines the hub holds: queued, running cleanup, or
    /// running in the scheduler.
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.waiting.len() + self.scheduled.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seatline_core::protocol::{ErrorCode, Failure};
    use seatline_core::telemetry::Memory;

    #[test]
    fn a_disabled_hub_keeps_nothing() {
        let mut telemetry = Telemetry::disabled();
        telemetry.received(1, "a", "app", "codex", "send");
        telemetry.admitted(1, "a");
        assert!(telemetry.hand_off(1, "a").is_none());
        telemetry.unscheduled(1, "a", &Update::Completed);
        assert_eq!(telemetry.in_flight(), 0);
        assert!(!telemetry.enabled());
    }

    #[test]
    fn every_ending_maps_to_one_outcome() {
        let failure = Failure {
            code: ErrorCode::ProviderFailed,
            reason: "PROVIDER_FAILED",
            retryable: false,
        };
        let cases = [
            (EndReason::Completed, Outcome::Completed, None),
            (
                EndReason::Failed(failure),
                Outcome::Failed,
                Some("PROVIDER_FAILED"),
            ),
            (EndReason::Cancelled, Outcome::Cancelled, None),
            (
                EndReason::Timeout(TimeoutKind::Start),
                Outcome::TimedOut,
                Some("start"),
            ),
            (
                EndReason::Timeout(TimeoutKind::Idle),
                Outcome::TimedOut,
                Some("idle"),
            ),
            (
                EndReason::Timeout(TimeoutKind::Absolute),
                Outcome::TimedOut,
                Some("absolute"),
            ),
            (
                EndReason::StoppedUnexpectedly,
                Outcome::Aborted,
                Some("stopped_unexpectedly"),
            ),
            (
                EndReason::AdapterPanicked {
                    maybe_started: true,
                },
                Outcome::Aborted,
                Some("adapter_panicked"),
            ),
            (
                EndReason::SchedulerPanicked {
                    maybe_started: true,
                },
                Outcome::Aborted,
                Some("scheduler_panicked"),
            ),
        ];
        for (reason, outcome, detail) in cases {
            assert_eq!(outcome_of(&reason), (outcome, detail), "{reason:?}");
        }
    }

    #[test]
    fn a_method_a_client_made_up_never_reaches_a_record() {
        let memory = Arc::new(Memory::new());
        let mut telemetry = Telemetry::new(memory.clone());
        let invented = "x".repeat(4096);
        telemetry.received(1, "r", "app", "codex", &invented);
        telemetry.unscheduled(1, "r", &Update::Stopped);
        telemetry.received(1, "s", "app", "codex", "status");
        telemetry.unscheduled(1, "s", &Update::Stopped);
        let records = memory.take();
        let methods: Vec<&str> = records
            .iter()
            .map(|record| match record {
                Record::Request(record) => record.method.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(methods, ["unknown", "status"]);
        assert!(!serde_json::to_string(&records).unwrap().contains("xxxx"));
    }

    #[test]
    fn a_refusal_is_a_record_with_a_static_reason() {
        let memory = Arc::new(Memory::new());
        let mut telemetry = Telemetry::new(memory.clone());
        telemetry.received(4, "r", "app", "codex", "send");
        telemetry.unscheduled(
            4,
            "r",
            &Update::Failed(Failure {
                code: ErrorCode::ProviderFailed,
                reason: "QUEUE_FULL",
                retryable: true,
            }),
        );
        let records = memory.take();
        let [Record::Request(record)] = records.as_slice() else {
            panic!("{records:?}");
        };
        assert_eq!(record.outcome, Outcome::Failed);
        assert_eq!(record.detail, Some("QUEUE_FULL"));
        assert_eq!(record.phases_us.sum(), record.total_us);
        assert_eq!(telemetry.in_flight(), 0);
    }

    #[test]
    fn the_waiting_map_is_bounded() {
        let mut telemetry = Telemetry::new(Arc::new(Memory::new()));
        for index in 0..MAX_WAITING + 10 {
            telemetry.received(1, &index.to_string(), "app", "codex", "send");
        }
        assert_eq!(telemetry.in_flight(), MAX_WAITING);
    }

    #[test]
    fn a_full_queue_drops_records_instead_of_blocking() {
        let (records, _queue) = mpsc::sync_channel(1);
        let sink = JsonLines {
            records,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        for connection in 0..4 {
            sink.record(handshake_record(connection, "app".into(), Duration::ZERO));
        }
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn the_file_sink_writes_one_json_line_per_record_and_says_what_it_lost() {
        let path = std::env::temp_dir().join(format!(
            "seatline-telemetry-{}-{}.jsonl",
            std::process::id(),
            crate::config::random_token().unwrap()
        ));
        let (records, queue) = mpsc::sync_channel(8);
        let dropped = Arc::new(AtomicU64::new(0));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let lost = Arc::clone(&dropped);
        let writer = std::thread::spawn(move || write_lines(file, 0, queue, &lost));
        let sink = JsonLines { records, dropped };
        sink.record(handshake_record(1, "app".into(), Duration::from_micros(7)));
        // Records the sink had to drop are counted, and the count is written
        // before the next record that is kept.
        sink.dropped.fetch_add(3, Ordering::Relaxed);
        sink.record(handshake_record(2, "app".into(), Duration::from_micros(9)));
        drop(sink);
        writer.join().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(lines.iter().all(|line| line["schema"] == SCHEMA));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line["kind"] == "connection")
                .count(),
            2,
            "{text}"
        );
        let lost: u64 = lines
            .iter()
            .filter(|line| line["kind"] == "dropped")
            .map(|line| line["count"].as_u64().unwrap())
            .sum();
        assert_eq!(lost, 3, "{text}");
    }
}
