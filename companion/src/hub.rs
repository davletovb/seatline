//! One owner of all provider adapters and exchanges. IO threads can submit
//! bounded commands but cannot choose namespaces, executables or environments.
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use seatline_core::exchange::{Timeouts, Update};
use seatline_core::protocol::ErrorCode;
use seatline_core::telemetry::Sink;
use seatline_core::turn::{Namespace, SessionPolicy, ToolPolicy, Turn, is_cleanup_group};
use seatline_platform::layout::Layout;
use seatline_providers::{Cleanup, Provider, claude, codex, gemini, grok};
use seatline_scheduler::{EndReason, Event, Supervisor, TurnId};
use serde_json::{Value, json};

use crate::{
    PROTOCOL_VERSION,
    config::{self, Grant},
    ledger::{Ledger, Mutation},
    scheduling::{Class, Hints, Policy},
    sessions::{Session, Sessions},
    telemetry::Telemetry,
    wire,
};

const MAX_CONNECTIONS: usize = 32;
const MAX_QUEUE: usize = 64;
const MAX_APP_QUEUE: usize = 8;
const MAX_RUNNING: usize = 8;
const MAX_APP_RUNNING: usize = 2;
const MAX_PROVIDER_RUNNING: usize = 2;
/// Persistent sessions kept across all apps, and by any one app, so a single
/// app cannot use up the ledger the others depend on.
const MAX_SESSIONS: usize = 10_000;
const MAX_APP_SESSIONS: usize = 2_000;
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// The hub's limits, by name, as telemetry reports a broker's configuration.
pub fn limits() -> BTreeMap<&'static str, u64> {
    BTreeMap::from([
        ("max_connections", MAX_CONNECTIONS as u64),
        ("max_queue", MAX_QUEUE as u64),
        ("max_app_queue", MAX_APP_QUEUE as u64),
        ("max_running", MAX_RUNNING as u64),
        ("max_app_running", MAX_APP_RUNNING as u64),
        ("max_provider_running", MAX_PROVIDER_RUNNING as u64),
        ("max_sessions", MAX_SESSIONS as u64),
        ("max_app_sessions", MAX_APP_SESSIONS as u64),
        ("prune_interval_ms", PRUNE_INTERVAL.as_millis() as u64),
    ])
}

/// Effective startup policy for phase reports. Configuration changes take
/// effect on the next broker start.
pub fn limits_for(root: &std::path::Path) -> io::Result<BTreeMap<&'static str, u64>> {
    let mut limits = limits();
    limits.extend(Policy::load(root)?.limits());
    Ok(limits)
}

pub enum Command {
    Open {
        connection: u64,
        grant: Box<Grant>,
        output: tokio::sync::mpsc::Sender<Value>,
    },
    Request {
        connection: u64,
        value: Value,
    },
    Close(u64),
    /// Asks whether the hub has no work at all: nothing queued, running, being
    /// written to the ledger or cleaned up. The broker asks while it is being
    /// stopped, to leave as soon as it can without ending a request.
    Quiet(tokio::sync::oneshot::Sender<bool>),
}

struct Connection {
    grant: Grant,
    output: tokio::sync::mpsc::Sender<Value>,
}
// Providers outlive connections, including compatibility clients that open
// one connection per exchange. Keep the originating grant alongside them so
// a reconnect cannot inherit a previous authorization/workspace's evidence.
struct RetainedProvider {
    grant: Grant,
    provider: Box<dyn Provider>,
}
struct Request {
    connection: u64,
    id: String,
    app: String,
    provider: String,
    method: String,
    params: Value,
    hints: Hints,
    expires: Instant,
}
impl Request {
    fn continuation(&self) -> &Value {
        if matches!(
            self.method.as_str(),
            "send_ready" | "send_ready_with_policy"
        ) {
            &self.params["turn"]["continuation"]
        } else {
            &self.params["continuation"]
        }
    }
}
struct Active {
    request: Request,
    persistent: bool,
    /// A local failure can end the client-visible request before the
    /// supervisor finishes stopping/reaping its exchange. Keep its slot until
    /// Ended, but release its request ID and never forward more output after
    /// this boundary.
    terminal_sent: bool,
    gate: Rc<Cell<bool>>,
}
struct PendingCleanup {
    request: Request,
    result: Receiver<io::Result<()>>,
    completed: Box<dyn FnOnce()>,
    sessions: Vec<String>,
}

enum LedgerAction {
    Insert {
        token: String,
        waiters: Vec<TurnId>,
    },
    Remove {
        tokens: Vec<String>,
        cleanup: Option<Box<PendingCleanup>>,
    },
}
struct PendingLedger {
    result: Receiver<io::Result<()>>,
    action: LedgerAction,
}

/// Stop reading provider output at a new native session until its token is
/// durably stored. Backpressure stays in the process's bounded pipes instead
/// of collecting answer text on the hub. Cancellation always bypasses the gate.
struct SessionGate {
    exchange: Box<dyn seatline_core::exchange::Exchange>,
    open: Rc<Cell<bool>>,
    cancelled: bool,
}
impl seatline_core::exchange::Exchange for SessionGate {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if !self.open.get() && !self.cancelled {
            return None;
        }
        let update = self.exchange.next(deadline)?;
        if matches!(update, Update::Session(_)) && !self.cancelled {
            self.open.set(false);
        }
        Some(update)
    }
    fn cancel(&mut self, grace: Duration) {
        self.cancelled = true;
        self.open.set(true);
        self.exchange.cancel(grace);
    }
    fn probe_span(&self) -> Option<seatline_core::telemetry::Span> {
        self.exchange.probe_span()
    }
    fn result_at(&self) -> Option<Instant> {
        self.exchange.result_at()
    }
}

pub fn start(root: PathBuf) -> io::Result<SyncSender<Command>> {
    start_with(root, None)
}

/// [`start`], with phase telemetry going to `telemetry` when it is given.
pub fn start_with(
    root: PathBuf,
    telemetry: Option<Arc<dyn Sink>>,
) -> io::Result<SyncSender<Command>> {
    start_joinable(root, telemetry).map(|(send, _)| send)
}

/// [`start_with`], also returning the hub's thread. Once every sender is
/// dropped the hub closes its connections and ends what is running (providers
/// are stopped and reaped, ledger writes finish); joining the thread waits for
/// that, which a process that is about to exit should do.
pub fn start_joinable(
    root: PathBuf,
    telemetry: Option<Arc<dyn Sink>>,
) -> io::Result<(SyncSender<Command>, std::thread::JoinHandle<()>)> {
    let ledger = root.join("sessions.json");
    let sessions = match std::fs::read(&ledger) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Sessions::default(),
        Err(error) => return Err(error),
    };
    let policy = Policy::load(&root)?;
    let ledger = Ledger::new(root.clone(), sessions.clone())?;
    let (send, receive) = mpsc::sync_channel(128);
    let thread = std::thread::spawn(move || {
        Hub {
            root,
            connections: BTreeMap::new(),
            queue: VecDeque::new(),
            providers: BTreeMap::new(),
            supervisor: Supervisor::new(),
            active: BTreeMap::new(),
            sessions,
            ledger,
            ledger_jobs: Vec::new(),
            policy,
            last_app: None,
            interactive_streak: 0,
            quiet_ticks: 0,
            cleanup: Vec::new(),
            next_check: Instant::now(),
            next_prune: Instant::now() + PRUNE_INTERVAL,
            missing_grants: BTreeSet::new(),
            telemetry: telemetry.map_or_else(Telemetry::disabled, Telemetry::new),
        }
        .run(receive);
    });
    Ok((send, thread))
}

struct Hub {
    root: PathBuf,
    connections: BTreeMap<u64, Connection>,
    queue: VecDeque<Request>,
    providers: BTreeMap<(String, String), RetainedProvider>,
    supervisor: Supervisor,
    active: BTreeMap<TurnId, Active>,
    sessions: Sessions,
    ledger: Ledger,
    ledger_jobs: Vec<PendingLedger>,
    policy: Policy,
    last_app: Option<String>,
    interactive_streak: usize,
    /// Short bursts follow progress; quiet active work backs off to 5 ms.
    quiet_ticks: u64,
    cleanup: Vec<PendingCleanup>,
    next_check: Instant,
    next_prune: Instant,
    /// Apps whose grant was missing at the previous sweep. Only apps missing at
    /// two sweeps in a row lose their sessions, so a grant that is being
    /// rewritten is never mistaken for a revoked one.
    missing_grants: BTreeSet<String>,
    telemetry: Telemetry,
}

enum LedgerError {
    Full,
    Storage,
}

impl Hub {
    fn run(mut self, input: Receiver<Command>) {
        loop {
            match input.recv_timeout(self.wait_time(Instant::now())) {
                Ok(command) => self.command(command),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            for _ in 0..31 {
                match input.try_recv() {
                    Ok(command) => self.command(command),
                    Err(_) => break,
                }
            }
            self.tick();
        }
        for connection in self.connections.keys().copied().collect::<Vec<_>>() {
            self.close(connection);
        }
        self.supervisor.shutdown(Duration::from_secs(2));
        let until = Instant::now() + Duration::from_secs(4);
        while (!self.supervisor.is_empty()
            || !self.ledger_jobs.is_empty()
            || !self.cleanup.is_empty())
            && Instant::now() < until
        {
            self.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn authorized(&self, connection: u64) -> bool {
        self.connections.get(&connection).is_some_and(|entry| {
            config::load_grant(&self.root, &entry.grant.app)
                .is_ok_and(|current| current.same_provider_scope(&entry.grant))
        })
    }

    fn quiet(&self) -> bool {
        self.queue.is_empty()
            && self.active.is_empty()
            && self.ledger_jobs.is_empty()
            && self.cleanup.is_empty()
    }

    fn command(&mut self, command: Command) {
        // A question is not work: it does not restart the 1 ms burst timing.
        if !matches!(command, Command::Quiet(_)) {
            self.quiet_ticks = 0;
        }
        match command {
            Command::Open {
                connection,
                grant,
                output,
            } => {
                if self.connections.len() >= MAX_CONNECTIONS {
                    return;
                }
                self.connections.insert(
                    connection,
                    Connection {
                        grant: *grant,
                        output,
                    },
                );
                if !self.authorized(connection) {
                    self.close(connection);
                    return;
                }
                self.emit(
                    connection,
                    json!({"type":"ready","version":PROTOCOL_VERSION}),
                );
            }
            Command::Close(connection) => self.close(connection),
            Command::Quiet(reply) => {
                let _ = reply.send(self.quiet());
            }
            Command::Request { connection, value } => {
                if !self.authorized(connection) {
                    self.invalidate_connection(connection);
                    self.close(connection);
                    return;
                }
                let Some(id) = value["id"]
                    .as_str()
                    .filter(|id| wire::valid_id(id))
                    .map(str::to_owned)
                else {
                    self.close(connection);
                    return;
                };
                if value["method"] == "cancel" {
                    if let Some(target) = value["target"].as_str() {
                        let mut kept = VecDeque::new();
                        while let Some(request) = self.queue.pop_front() {
                            if request.connection == connection && request.id == target {
                                self.event(&request, Update::Stopped);
                            } else {
                                kept.push_back(request);
                            }
                        }
                        self.queue = kept;
                        for (turn, active) in &self.active {
                            if active.request.connection == connection
                                && active.request.id == target
                            {
                                self.supervisor.cancel(*turn);
                            }
                        }
                    }
                    return;
                }
                let entry = &self.connections[&connection];
                let Some(provider) = value["provider"]
                    .as_str()
                    .filter(|p| entry.grant.providers.iter().any(|allowed| allowed == p))
                else {
                    self.emit(connection, json!({"id":id,"event":wire::encode_update(&Update::Failed(wire::failure(ErrorCode::InvalidRequest,wire::reason::APP_NOT_AUTHORIZED,false)))}));
                    return;
                };
                let hints = match value
                    .get("scheduling")
                    .cloned()
                    .map(serde_json::from_value::<Hints>)
                    .transpose()
                {
                    Ok(hints) => hints.unwrap_or_default(),
                    Err(_) => {
                        self.emit(connection, json!({"id":id,"event":wire::encode_update(&Update::Failed(wire::failure(ErrorCode::InvalidRequest, wire::reason::INVALID_REQUEST, false)))}));
                        return;
                    }
                };
                let queue_timeout_ms = hints
                    .queue_timeout_ms
                    .unwrap_or(self.policy.queue_timeout_ms)
                    .clamp(1, self.policy.queue_timeout_ms);
                let request = Request {
                    connection,
                    id,
                    app: entry.grant.app.clone(),
                    provider: provider.to_owned(),
                    method: value["method"].as_str().unwrap_or("").to_owned(),
                    params: value["params"].clone(),
                    hints,
                    expires: Instant::now() + Duration::from_millis(queue_timeout_ms),
                };
                if self
                    .queue
                    .iter()
                    .chain(
                        self.active
                            .values()
                            .filter(|active| !active.terminal_sent)
                            .map(|active| &active.request),
                    )
                    .chain(self.cleanup.iter().map(|p| &p.request))
                    .chain(self.ledger_jobs.iter().filter_map(|p| match &p.action {
                        LedgerAction::Remove {
                            cleanup: Some(c), ..
                        } => Some(&c.request),
                        _ => None,
                    }))
                    .any(|other| other.connection == connection && other.id == request.id)
                {
                    self.close(connection);
                    return;
                }
                self.telemetry.received(
                    connection,
                    &request.id,
                    &request.app,
                    &request.provider,
                    &request.method,
                );
                if self.queue.len() >= MAX_QUEUE
                    || self
                        .queue
                        .iter()
                        .filter(|other| other.app == request.app)
                        .count()
                        >= MAX_APP_QUEUE
                {
                    self.event(
                        &request,
                        Update::Failed(wire::failure(
                            ErrorCode::ProviderFailed,
                            wire::reason::QUEUE_FULL,
                            true,
                        )),
                    );
                } else {
                    if request.hints.events {
                        self.event(
                            &request,
                            Update::Queued {
                                ahead: self.queue.len() as u32,
                                timeout_ms: queue_timeout_ms,
                            },
                        );
                    }
                    self.queue.push_back(request);
                }
            }
        }
    }

    fn invalidate_connection(&mut self, connection: u64) {
        if let Some(entry) = self.connections.get(&connection) {
            let app = entry.grant.app.clone();
            self.invalidate_app(&app);
        }
    }

    fn invalidate_app(&mut self, app: &str) {
        for ((owner, _), retained) in &self.providers {
            if owner == app {
                retained.provider.invalidate_readiness();
            }
        }
        self.providers.retain(|(owner, _), _| owner != app);
    }

    fn close(&mut self, connection: u64) {
        self.connections.remove(&connection);
        for request in &self.queue {
            if request.connection == connection {
                self.telemetry.abandoned(connection, &request.id);
            }
        }
        self.queue
            .retain(|request| request.connection != connection);
        for (turn, active) in &self.active {
            if active.request.connection == connection {
                self.supervisor.cancel(*turn);
            }
        }
    }

    fn emit(&mut self, connection: u64, value: Value) {
        if self
            .connections
            .get(&connection)
            .is_some_and(|entry| entry.output.try_send(value).is_err())
        {
            self.close(connection);
        }
    }

    fn event(&mut self, request: &Request, update: Update) {
        if update.is_terminal() {
            // Only a request that never reached the scheduler still has its
            // timeline here; a scheduled one is recorded when it ends.
            self.telemetry
                .unscheduled(request.connection, &request.id, &update);
        }
        if let Update::Delta(text) = update {
            let mut rest = text.as_str();
            while !rest.is_empty() {
                let mut end = rest.len().min(16 * 1024);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                self.emit(
                    request.connection,
                    json!({"id":request.id,"event":{"type":"delta","text":&rest[..end]}}),
                );
                rest = &rest[end..];
            }
        } else {
            self.emit(
                request.connection,
                json!({"id":request.id,"event":wire::encode_update(&update)}),
            );
        }
    }

    /// Returns an already durable token, or submits one ordered mutation and
    /// pauses this exchange. Peers reporting the same native session join it.
    fn session_token(
        &mut self,
        turn: TurnId,
        app: &str,
        provider: &str,
        native: String,
    ) -> Result<Option<String>, LedgerError> {
        let token = match self.sessions.token(app, provider, &native) {
            Some(token) if !self.removing(token) => Some(token.clone()),
            Some(_) => self
                .sessions
                .tokens(app, provider, &native)
                .find(|token| !self.removing(token))
                .cloned(),
            None => None,
        };
        if let Some(token) = token {
            if let Some(job) = self.ledger_jobs.iter_mut().find(|job| matches!(&job.action, LedgerAction::Insert { token: pending, .. } if pending == &token)) {
                if let LedgerAction::Insert { waiters, .. } = &mut job.action { waiters.push(turn); }
                return Ok(None);
            }
            return Ok(Some(token));
        }
        if self.sessions.len() >= MAX_SESSIONS || self.sessions.app_len(app) >= MAX_APP_SESSIONS {
            return Err(LedgerError::Full);
        }
        let token = config::random_token().map_err(|_| LedgerError::Storage)?;
        let session = Session {
            app: app.into(),
            provider: provider.into(),
            native,
        };
        if self.ledger_jobs.len() >= crate::ledger::CAPACITY {
            return Err(LedgerError::Storage);
        }
        let result = self
            .ledger
            .submit(Mutation::Insert(token.clone(), session.clone()))
            .map_err(|_| LedgerError::Storage)?;
        self.sessions.insert(token.clone(), session);
        self.ledger_jobs.push(PendingLedger {
            result,
            action: LedgerAction::Insert {
                token,
                waiters: vec![turn],
            },
        });
        Ok(None)
    }

    fn removing(&self, token: &str) -> bool {
        self.ledger_jobs.iter().any(|job| matches!(&job.action, LedgerAction::Remove { tokens, .. } if tokens.iter().any(|pending| pending == token)))
    }

    fn remove_sessions(&mut self, tokens: Vec<String>, cleanup: Option<PendingCleanup>) {
        if tokens.is_empty() {
            if let Some(cleanup) = cleanup {
                (cleanup.completed)();
                self.event(&cleanup.request, Update::Completed);
            }
            return;
        }
        if self.ledger_jobs.len() < crate::ledger::CAPACITY {
            if let Ok(result) = self.ledger.submit(Mutation::Remove(tokens.clone())) {
                self.ledger_jobs.push(PendingLedger {
                    result,
                    action: LedgerAction::Remove {
                        tokens,
                        cleanup: cleanup.map(Box::new),
                    },
                });
                return;
            }
        }
        if let Some(cleanup) = cleanup {
            self.event(
                &cleanup.request,
                Update::Failed(wire::failure(
                    ErrorCode::InternalError,
                    wire::reason::CLEANUP_FAILED,
                    true,
                )),
            );
        }
    }

    fn poll_ledger(&mut self) -> bool {
        // Results are committed in submission order even if the hub was busy.
        let mut progressed = false;
        while let Some(job) = self.ledger_jobs.first() {
            let result = match job.result.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(io::Error::other("ledger worker stopped"))
                }
            };
            progressed = true;
            let job = self.ledger_jobs.remove(0);
            match job.action {
                LedgerAction::Insert { token, waiters } => {
                    if result.is_err() {
                        self.sessions.remove(&token);
                    }
                    for turn in waiters {
                        if let Some(mut active) = self.active.remove(&turn) {
                            if !active.terminal_sent {
                                let update = if result.is_ok() {
                                    Update::Session(token.clone())
                                } else {
                                    self.supervisor.cancel(turn);
                                    Update::Failed(wire::failure(
                                        ErrorCode::InternalError,
                                        wire::reason::SESSION_STORE_FAILED,
                                        false,
                                    ))
                                };
                                active.terminal_sent = update.is_terminal();
                                if active.terminal_sent {
                                    self.telemetry.client_saw(turn, &update);
                                }
                                self.event(&active.request, update);
                            }
                            active.gate.set(true);
                            self.active.insert(turn, active);
                        }
                    }
                }
                LedgerAction::Remove { tokens, cleanup } => {
                    if result.is_ok() {
                        for token in tokens {
                            self.sessions.remove(&token);
                        }
                    }
                    if let Some(cleanup) = cleanup {
                        let update = if result.is_ok() {
                            (cleanup.completed)();
                            Update::Completed
                        } else {
                            Update::Failed(wire::failure(
                                ErrorCode::InternalError,
                                wire::reason::CLEANUP_FAILED,
                                true,
                            ))
                        };
                        self.event(&cleanup.request, update);
                    }
                }
            }
        }
        progressed
    }

    /// Two missing-grant sweeps are still required. Tokens with a pending
    /// insert are pruned on the next sweep, after that insert is committed.
    fn prune_revoked_sessions(&mut self) {
        let apps: BTreeSet<String> = self.sessions.values().map(|s| s.app.clone()).collect();
        let missing: BTreeSet<String> = apps.into_iter().filter(|app| {
            config::app_path(&self.root, app).is_ok_and(|path| matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound))
        }).collect();
        let tokens = self
            .sessions
            .iter()
            .filter(|(token, s)| {
                missing.contains(&s.app)
                    && self.missing_grants.contains(&s.app)
                    && !self.ledger_jobs.iter().any(|job| match &job.action {
                        LedgerAction::Insert { token: pending, .. } => pending == *token,
                        LedgerAction::Remove { tokens, .. } => tokens.contains(token),
                    })
            })
            .map(|(token, _)| token.clone())
            .collect();
        self.missing_grants = missing;
        self.remove_sessions(tokens, None);
    }

    /// With no exchanges or filesystem work to poll, wake only for commands or
    /// the next authorization/prune/queue timer. Active waits ramp from 1 to
    /// 5 ms without progress, and commands/results restart the short burst.
    fn wait_time(&self, now: Instant) -> Duration {
        let mut until = self.next_prune;
        if !self.connections.is_empty() {
            until = until.min(self.next_check);
        }
        if let Some(expires) = self.queue.iter().map(|r| r.expires).min() {
            until = until.min(expires);
        }
        let timer = until.saturating_duration_since(now);
        if !self.active.is_empty() || !self.cleanup.is_empty() || !self.ledger_jobs.is_empty() {
            timer.min(Duration::from_millis(1 + self.quiet_ticks))
        } else {
            timer
        }
    }

    fn tick(&mut self) {
        if Instant::now() >= self.next_check {
            let revoked: Vec<_> = self
                .connections
                .keys()
                .copied()
                .filter(|id| !self.authorized(*id))
                .collect();
            for id in revoked {
                self.invalidate_connection(id);
                self.close(id);
            }
            self.next_check = Instant::now() + Duration::from_secs(1);
        }
        let mut progressed = self.poll_ledger();
        if Instant::now() >= self.next_prune {
            self.prune_revoked_sessions();
            self.next_prune = Instant::now() + PRUNE_INTERVAL;
        }
        let events = self.supervisor.poll(Duration::from_millis(1));
        progressed |= !events.is_empty();
        for event in events {
            match event {
                Event::Update { turn_id, update } => {
                    let Some(mut active) = self.active.remove(&turn_id) else {
                        continue;
                    };
                    if active.terminal_sent {
                        self.active.insert(turn_id, active);
                        continue;
                    }
                    let update = match update {
                        Update::Session(native) if active.persistent => {
                            let app = active.request.app.clone();
                            let provider = active.request.provider.clone();
                            match self.session_token(turn_id, &app, &provider, native) {
                                Ok(Some(token)) => {
                                    active.gate.set(true);
                                    Update::Session(token)
                                }
                                Ok(None) => {
                                    self.active.insert(turn_id, active);
                                    continue;
                                }
                                Err(error) => {
                                    self.supervisor.cancel(turn_id);
                                    Update::Failed(wire::failure(
                                        ErrorCode::InternalError,
                                        match error {
                                            LedgerError::Full => {
                                                wire::reason::SESSION_LIMIT_REACHED
                                            }
                                            LedgerError::Storage => {
                                                wire::reason::SESSION_STORE_FAILED
                                            }
                                        },
                                        false,
                                    ))
                                }
                            }
                        }
                        Update::Session(_) => {
                            self.active.insert(turn_id, active);
                            continue;
                        }
                        other => other,
                    };
                    if update.is_terminal() {
                        // The hub ended this request itself and will stop the
                        // exchange: the client was told this, whatever the
                        // scheduler later ends with.
                        self.telemetry.client_saw(turn_id, &update);
                    }
                    active.terminal_sent = update.is_terminal();
                    self.event(&active.request, update);
                    self.active.insert(turn_id, active);
                }
                Event::Ended { turn_id, reason } => {
                    self.telemetry.ended(turn_id, &reason, &mut self.supervisor);
                    if let Some(active) = self.active.remove(&turn_id).filter(|a| !a.terminal_sent)
                    {
                        self.event(
                            &active.request,
                            match reason {
                                EndReason::Completed => Update::Completed,
                                EndReason::Cancelled => Update::Stopped,
                                EndReason::Failed(error) => Update::Failed(error),
                                EndReason::Timeout(_) => Update::Failed(wire::failure(
                                    ErrorCode::ProviderFailed,
                                    wire::reason::PROVIDER_TIMEOUT,
                                    true,
                                )),
                                _ => Update::Failed(wire::failure(
                                    ErrorCode::InternalError,
                                    wire::reason::PROVIDER_FAILED,
                                    true,
                                )),
                            },
                        );
                    }
                }
            }
        }
        let mut index = 0;
        while index < self.cleanup.len() {
            match self.cleanup[index].result.try_recv() {
                Ok(result) => {
                    progressed = true;
                    let pending = self.cleanup.swap_remove(index);
                    if result.is_ok() {
                        self.remove_sessions(pending.sessions.clone(), Some(pending));
                    } else {
                        self.event(
                            &pending.request,
                            Update::Failed(wire::failure(
                                ErrorCode::InternalError,
                                wire::reason::CLEANUP_FAILED,
                                true,
                            )),
                        );
                    }
                }
                Err(mpsc::TryRecvError::Empty) => index += 1,
                Err(mpsc::TryRecvError::Disconnected) => {
                    progressed = true;
                    let pending = self.cleanup.swap_remove(index);
                    self.event(
                        &pending.request,
                        Update::Failed(wire::failure(
                            ErrorCode::InternalError,
                            wire::reason::CLEANUP_FAILED,
                            true,
                        )),
                    );
                }
            }
        }
        let now = Instant::now();
        let mut kept = VecDeque::new();
        while let Some(request) = self.queue.pop_front() {
            if request.expires <= now {
                self.event(
                    &request,
                    Update::Failed(wire::failure(
                        ErrorCode::ProviderFailed,
                        wire::reason::QUEUE_TIMEOUT,
                        true,
                    )),
                );
            } else {
                kept.push_back(request);
            }
        }
        self.queue = kept;
        while let Some(index) = self.next_request() {
            progressed = true;
            let request = self.queue.remove(index).unwrap();
            if Self::interactive(&request) {
                self.interactive_streak = self
                    .interactive_streak
                    .saturating_add(1)
                    .min(self.policy.interactive_burst);
            } else {
                self.interactive_streak = 0;
            }
            self.last_app = Some(request.app.clone());
            self.admit(request);
        }
        self.quiet_ticks = if progressed {
            0
        } else {
            (self.quiet_ticks + 1).min(4)
        };
    }

    fn interactive(request: &Request) -> bool {
        request.hints.interactive || Class::of(&request.method) == Class::Readiness
    }

    fn running_requests(&self) -> impl Iterator<Item = &Request> {
        self.active
            .values()
            .map(|a| &a.request)
            .chain(self.cleanup.iter().map(|c| &c.request))
            .chain(self.ledger_jobs.iter().filter_map(|j| match &j.action {
                LedgerAction::Remove {
                    cleanup: Some(c), ..
                } => Some(&c.request),
                _ => None,
            }))
    }

    fn has_capacity(&self, request: &Request, running: &[&Request]) -> bool {
        let class = Class::of(&request.method);
        let class_count = running
            .iter()
            .filter(|r| Class::of(&r.method) == class)
            .count();
        // Generation leaves a readiness reserve. Cleanup has its own bounded
        // lane and uses spare global capacity; all classes obey that ceiling.
        let class_limit = match class {
            Class::Generation => self.policy.generation_limit(),
            Class::Readiness => self.policy.max_readiness_running,
            Class::Cleanup => self.policy.max_cleanup_running,
        };
        let same_class = |r: &&Request| Class::of(&r.method) == class;
        if running.len() >= self.policy.max_running
            || class_count >= class_limit
            || running
                .iter()
                .filter(|r| same_class(r) && r.app == request.app)
                .count()
                >= self.policy.max_app_running
            || running
                .iter()
                .filter(|r| same_class(r) && r.provider == request.provider)
                .count()
                >= self.policy.max_provider_running
        {
            return false;
        }
        true
    }

    fn eligible(&self, index: usize, request: &Request) -> bool {
        let class = Class::of(&request.method);
        let running: Vec<_> = self.running_requests().collect();
        if !self.has_capacity(request, &running) {
            return false;
        }
        // Drain this pair once cleanup has an admission slot. A cleanup
        // waiting for another app to release the cleanup lane must not
        // unnecessarily hold back this app's generations. Keep the barrier
        // through the last generation's completion so interactive work
        // cannot overtake cleanup. Cancellation/expiry removes the barrier.
        if class == Class::Generation
            && self.queue.iter().take(index).any(|earlier| {
                Class::of(&earlier.method) == Class::Cleanup
                    && earlier.app == request.app
                    && earlier.provider == request.provider
                    && self.has_capacity(earlier, &running)
            })
        {
            return false;
        }
        let cleanup_conflict = running.iter().any(|r| {
            r.app == request.app
                && r.provider == request.provider
                && (Class::of(&r.method) == Class::Cleanup || class == Class::Cleanup)
                && Class::of(&r.method) != Class::Readiness
                && class != Class::Readiness
        });
        if cleanup_conflict {
            return false;
        }
        !(class == Class::Generation
            && request.continuation().is_string()
            && running.iter().any(|r| {
                r.app == request.app
                    && r.provider == request.provider
                    && r.continuation() == request.continuation()
            }))
    }

    fn next_request(&self) -> Option<usize> {
        let eligible: Vec<_> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(index, r)| self.eligible(*index, r))
            .collect();
        let preferred = if self.interactive_streak >= self.policy.interactive_burst
            && eligible.iter().any(|(_, r)| !Self::interactive(r))
        {
            false
        } else {
            eligible.iter().any(|(_, r)| Self::interactive(r))
        };
        // Rotation across eligible apps in this tier, then FIFO within the
        // chosen app. No application names or provider-specific priorities.
        eligible
            .iter()
            .filter(|(_, r)| Self::interactive(r) == preferred)
            .min_by_key(|(i, r)| {
                let wrapped = self.last_app.as_ref().is_some_and(|last| r.app <= *last);
                (wrapped, &r.app, *i)
            })
            .map(|(index, _)| *index)
    }

    #[allow(clippy::map_entry)] // Admission errors also need mutable access to the connection table.
    fn admit(&mut self, request: Request) {
        // Recheck queued work at admission, including cache hits, so a grant
        // change cannot reuse evidence from the previous app/workspace scope.
        if !self.authorized(request.connection) {
            self.telemetry.abandoned(request.connection, &request.id);
            self.invalidate_connection(request.connection);
            self.close(request.connection);
            return;
        }
        self.telemetry.admitted(request.connection, &request.id);
        if request.hints.events {
            self.event(&request, Update::Admitted);
        }
        let key = (request.app.clone(), request.provider.clone());
        let Some(connection) = self.connections.get(&request.connection) else {
            return;
        };
        let grant = connection.grant.clone();
        if self.providers.iter().any(|((app, _), retained)| {
            app == &request.app && !retained.grant.same_provider_scope(&grant)
        }) {
            self.invalidate_app(&request.app);
        }
        if !self.providers.contains_key(&key) {
            let Ok(namespace) = Namespace::fixed(&request.app) else {
                return;
            };
            let layout = match grant.cache_title.as_deref() {
                Some(title) => match Layout::with_cache_title(namespace, title) {
                    Ok(layout) => layout,
                    Err(_) => {
                        self.event(
                            &request,
                            Update::Failed(wire::failure(
                                ErrorCode::InvalidRequest,
                                wire::reason::INVALID_REQUEST,
                                false,
                            )),
                        );
                        return;
                    }
                },
                None => Layout::new(namespace),
            };
            let provider: Box<dyn Provider> = match request.provider.as_str() {
                "codex" => Box::new(codex::Codex::installed(&layout)),
                "claude" => Box::new(
                    claude::Claude::installed(&layout)
                        .with_isolated_launch(self.policy.claude_isolation),
                ),
                "gemini" => Box::new(gemini::Gemini::installed(&layout)),
                "grok" => Box::new(grok::Grok::installed(&layout)),
                _ => {
                    self.event(
                        &request,
                        Update::Failed(wire::failure(
                            ErrorCode::ProviderNotFound,
                            wire::reason::EXECUTABLE_NOT_FOUND,
                            false,
                        )),
                    );
                    return;
                }
            };
            self.providers.insert(
                key.clone(),
                RetainedProvider {
                    grant,
                    provider: Box::new(seatline_providers::readiness::Ready::boxed(provider)),
                },
            );
        }
        let result = catch_unwind(AssertUnwindSafe(|| self.build(&request, &key)));
        match result {
            Ok(Ok(Built::Exchange(exchange, timeouts, persistent))) => {
                let gate = Rc::new(Cell::new(true));
                let exchange: Box<dyn seatline_core::exchange::Exchange> = if persistent {
                    Box::new(SessionGate {
                        exchange,
                        open: gate.clone(),
                        cancelled: false,
                    })
                } else {
                    exchange
                };
                let grace = Duration::from_secs(2);
                let turn = match self.telemetry.hand_off(request.connection, &request.id) {
                    Some((identity, timeline)) => {
                        let turn =
                            self.supervisor
                                .start_timed(exchange, Some(timeouts), grace, timeline);
                        self.telemetry.scheduled(turn, identity);
                        turn
                    }
                    None => self.supervisor.start(exchange, Some(timeouts), grace),
                };
                self.active.insert(
                    turn,
                    Active {
                        request,
                        persistent,
                        terminal_sent: false,
                        gate,
                    },
                );
            }
            Ok(Ok(Built::Cleanup(cleanup, sessions))) => {
                let result = match seatline_core::work::Worker::cleanup()
                    .and_then(|pool| pool.reserve())
                    .and_then(|permit| permit.submit(cleanup.work))
                {
                    Ok(result) => result,
                    Err(_) => {
                        self.event(
                            &request,
                            Update::Failed(wire::failure(
                                ErrorCode::InternalError,
                                wire::reason::CLEANUP_BACKLOG_FULL,
                                true,
                            )),
                        );
                        return;
                    }
                };
                self.cleanup.push(PendingCleanup {
                    request,
                    result,
                    completed: cleanup.completed,
                    sessions,
                });
            }
            Ok(Err(error)) => self.event(&request, Update::Failed(error)),
            Err(_) => self.event(
                &request,
                Update::Failed(wire::failure(
                    ErrorCode::InternalError,
                    wire::reason::PROVIDER_FAILED,
                    true,
                )),
            ),
        }
    }

    fn build(
        &self,
        request: &Request,
        key: &(String, String),
    ) -> Result<Built, seatline_core::protocol::Failure> {
        let invalid = || {
            wire::failure(
                ErrorCode::InvalidRequest,
                wire::reason::INVALID_REQUEST,
                false,
            )
        };
        let provider = self.providers[key].provider.as_ref();
        let limits = Timeouts {
            max_turn: Duration::from_secs(15 * 60).min(provider.timeouts().max_turn),
            ..provider.timeouts()
        };
        match request.method.as_str() {
            "status" | "readiness" | "prepare" => Ok(Built::Exchange(
                match request.method.as_str() {
                    "status" => provider.status(),
                    "prepare" => provider.prepare(
                        serde_json::from_value(request.params.clone()).map_err(|_| invalid())?,
                    ),
                    _ => provider.readiness(
                        serde_json::from_value(request.params.clone()).map_err(|_| invalid())?,
                    ),
                },
                Timeouts {
                    start: Duration::from_secs(30),
                    idle: Duration::from_secs(30),
                    max_turn: Duration::from_secs(30),
                    stop_grace: Duration::from_secs(2),
                },
                false,
            )),
            "send" | "send_ready" | "send_ready_with_policy" => {
                let policy = if request.method == "send_ready_with_policy" {
                    Some(
                        serde_json::from_value::<seatline_core::readiness::SignInPolicy>(
                            request.params["allowed_sign_in"].clone(),
                        )
                        .map_err(|_| invalid())?,
                    )
                } else {
                    None
                };
                let freshness = if request.method != "send" {
                    Some(
                        serde_json::from_value::<seatline_core::readiness::Freshness>(
                            request.params["freshness"].clone(),
                        )
                        .map_err(|_| invalid())?,
                    )
                } else {
                    None
                };
                let params = if freshness.is_some() {
                    &request.params["turn"]
                } else {
                    &request.params
                };
                let mut turn: Turn =
                    serde_json::from_value(params.clone()).map_err(|_| invalid())?;
                turn.validate().map_err(|_| invalid())?;
                if turn.tools == ToolPolicy::ProviderDefault
                    && !self.connections[&request.connection]
                        .grant
                        .allow_provider_default
                {
                    // The app can act on this: it is refused by local policy, not malformed.
                    return Err(wire::failure(
                        ErrorCode::InvalidRequest,
                        wire::reason::PROVIDER_DEFAULT_TOOLS_DENIED,
                        false,
                    ));
                }
                if turn.session == SessionPolicy::Persistent
                    && !provider.supports_persistent_session()
                {
                    return Err(wire::failure(
                        ErrorCode::InvalidRequest,
                        wire::reason::PERSISTENT_SESSION_UNSUPPORTED,
                        false,
                    ));
                }
                if let Some(token) = turn.continuation.as_ref() {
                    let session = self
                        .sessions
                        .get(token)
                        .filter(|session| {
                            session.app == request.app
                                && session.provider == request.provider
                                && !self.removing(token)
                        })
                        .ok_or_else(|| {
                            wire::failure(
                                ErrorCode::InvalidRequest,
                                wire::reason::UNKNOWN_SESSION,
                                false,
                            )
                        })?;
                    turn.continuation = Some(session.native.clone());
                }
                let persistent = turn.session == SessionPolicy::Persistent;
                Ok(Built::Exchange(
                    match (freshness, policy) {
                        (Some(freshness), Some(policy)) => {
                            provider.send_with_readiness_policy(turn, freshness, policy)
                        }
                        (Some(freshness), None) => provider.send_with_readiness(turn, freshness),
                        (None, _) => provider.send(turn),
                    },
                    limits,
                    persistent,
                ))
            }
            "forget" => {
                let tokens: Vec<String> =
                    serde_json::from_value(request.params["sessions"].clone())
                        .map_err(|_| invalid())?;
                if tokens.len() > 256 {
                    return Err(invalid());
                }
                let mut native = Vec::new();
                for token in &tokens {
                    if let Some(session) = self.sessions.get(token) {
                        if session.app != request.app || session.provider != request.provider {
                            return Err(invalid());
                        }
                        native.push(session.native.clone());
                    }
                }
                Ok(Built::Cleanup(provider.cleanup_sessions(&native), tokens))
            }
            "cleanup" => {
                let group = request.params["group"]
                    .as_str()
                    .filter(|group| is_cleanup_group(group))
                    .ok_or_else(invalid)?;
                Ok(Built::Cleanup(provider.cleanup_group(group), Vec::new()))
            }
            _ => Err(invalid()),
        }
    }
}

enum Built {
    Exchange(Box<dyn seatline_core::exchange::Exchange>, Timeouts, bool),
    Cleanup(Cleanup, Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use seatline_core::exchange::{Exchange, Scripted};
    use seatline_core::protocol::Capabilities;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct Fixture;
    impl Provider for Fixture {
        fn id(&self) -> &str {
            "codex"
        }
        fn capabilities(&self) -> Capabilities {
            codex::CAPABILITIES
        }
        fn timeouts(&self) -> Timeouts {
            Timeouts {
                start: Duration::from_secs(30),
                idle: Duration::from_secs(30),
                max_turn: Duration::from_secs(30),
                stop_grace: Duration::ZERO,
            }
        }
        fn supports_persistent_session(&self) -> bool {
            true
        }
        fn status(&self) -> Box<dyn Exchange> {
            Box::new(Scripted::new([Update::Completed]))
        }
        fn send(&self, _: Turn) -> Box<dyn Exchange> {
            Box::new(Scripted::new([
                Update::Session("raw-native-handle".into()),
                Update::Started,
                Update::Delta("answer".into()),
                Update::Completed,
            ]))
        }
    }
    struct ReadyFixture(std::rc::Rc<std::cell::Cell<usize>>);
    impl Provider for ReadyFixture {
        fn id(&self) -> &str {
            "codex"
        }
        fn capabilities(&self) -> Capabilities {
            codex::CAPABILITIES
        }
        fn timeouts(&self) -> Timeouts {
            codex::LIMITS.timeouts
        }
        fn supports_preparation(&self) -> bool {
            true
        }
        fn readiness_key(&self) -> Option<seatline_providers::readiness::Key> {
            seatline_providers::readiness::Key::watch(
                &std::env::current_exe().unwrap(),
                [],
                self.capabilities(),
            )
        }
        fn status(&self) -> Box<dyn Exchange> {
            self.0.set(self.0.get() + 1);
            Box::new(Scripted::new([
                Update::Status {
                    provider_id: "codex".into(),
                    status: seatline_core::protocol::ProviderState {
                        availability: seatline_core::protocol::Availability::Available,
                        authentication: seatline_core::protocol::Authentication::Authenticated,
                        capabilities: self.capabilities(),
                        models: std::borrow::Cow::Borrowed(&[]),
                        sign_in: None,
                        readiness: None,
                    },
                },
                Update::Completed,
            ]))
        }
        fn send(&self, _: Turn) -> Box<dyn Exchange> {
            Box::new(Scripted::new([
                Update::Launched,
                Update::Started,
                Update::Delta("answer".into()),
                Update::Completed,
            ]))
        }
    }

    #[test]
    fn prepare_is_single_flight_scoped_and_send_ready_preserves_fresh_requests() {
        let (mut hub, mut output) = setup();
        let counts: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|app| {
                let counter = std::rc::Rc::new(std::cell::Cell::new(0));
                install_provider(
                    &mut hub,
                    app,
                    Box::new(seatline_providers::readiness::Ready::new(ReadyFixture(
                        counter.clone(),
                    ))),
                );
                counter
            })
            .collect();
        let cached = json!({"mode":"cached","max_age_ms":30000});
        request(&mut hub, 1, "a", "prepare", cached.clone());
        request(&mut hub, 1, "b", "prepare", cached.clone());
        request(&mut hub, 2, "c", "prepare", cached.clone());
        settle(&mut hub);
        assert_eq!((counts[0].get(), counts[1].get()), (1, 1));
        let events = drain(&mut output[0]);
        for id in ["a", "b"] {
            assert!(
                events
                    .iter()
                    .any(|v| v["id"] == id && v["event"]["type"] == "completed")
            );
        }
        assert!(!events.iter().any(|v| v["event"]["type"] == "launched"));
        let mut ask = turn_with_tools("none");
        ask["session"] = json!("ephemeral");
        request(
            &mut hub,
            1,
            "cached",
            "send_ready",
            json!({"turn":ask,"freshness":cached}),
        );
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert_eq!(
            events[0]["event"]["status"]["readiness"]["source"],
            "cached"
        );
        assert!(events.iter().any(|v| v["event"]["type"] == "completed"));
        assert_eq!(counts[0].get(), 1);
        ask["check_sign_in"] = json!(true);
        request(
            &mut hub,
            1,
            "fresh",
            "send_ready",
            json!({"turn":ask,"freshness":cached}),
        );
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert_eq!(events[0]["event"]["status"]["readiness"]["source"], "fresh");
        assert_eq!(counts[0].get(), 2);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn changed_billing_mode_never_launches_even_when_client_events_are_not_consumed() {
        use seatline_core::protocol::{Authentication, Availability, ProviderState};
        use seatline_core::turn::SignInClassification;
        struct Account {
            file: PathBuf,
            mode: Rc<Cell<SignInClassification>>,
            launches: Rc<Cell<usize>>,
        }
        impl Provider for Account {
            fn id(&self) -> &str {
                "codex"
            }
            fn capabilities(&self) -> Capabilities {
                codex::CAPABILITIES
            }
            fn timeouts(&self) -> Timeouts {
                Fixture.timeouts()
            }
            fn readiness_key(&self) -> Option<seatline_providers::readiness::Key> {
                seatline_providers::readiness::Key::watch(
                    &std::env::current_exe().unwrap(),
                    [self.file.clone()],
                    self.capabilities(),
                )
            }
            fn status(&self) -> Box<dyn Exchange> {
                Box::new(Scripted::new([
                    Update::Status {
                        provider_id: "codex".into(),
                        status: ProviderState {
                            availability: Availability::Available,
                            authentication: Authentication::Authenticated,
                            capabilities: self.capabilities(),
                            models: std::borrow::Cow::Borrowed(&[]),
                            sign_in: Some(self.mode.get()),
                            readiness: None,
                        },
                    },
                    Update::Completed,
                ]))
            }
            fn send(&self, turn: Turn) -> Box<dyn Exchange> {
                self.launches.set(self.launches.get() + 1);
                Fixture.send(turn)
            }
        }
        let (mut hub, mut output) = setup();
        let file = hub.root.join("account.json");
        std::fs::write(&file, "subscription").unwrap();
        let mode = Rc::new(Cell::new(SignInClassification::Subscription));
        let launches = Rc::new(Cell::new(0));
        install_provider(
            &mut hub,
            "first",
            Box::new(seatline_providers::readiness::Ready::new(Account {
                file: file.clone(),
                mode: mode.clone(),
                launches: launches.clone(),
            })),
        );
        let cached = json!({"mode":"cached","max_age_ms":30000});
        request(&mut hub, 1, "approved", "readiness", cached.clone());
        settle(&mut hub);
        let approved = drain(&mut output[0]);
        assert_eq!(approved[0]["event"]["status"]["sign_in"], "subscription");
        mode.set(SignInClassification::ApiKey);
        std::fs::write(file, "api_key").unwrap();
        let mut turn = turn_with_tools("none");
        turn["session"] = json!("ephemeral");
        request(
            &mut hub,
            1,
            "protected",
            "send_ready_with_policy",
            json!({
                "turn":turn,"freshness":cached,"allowed_sign_in":["subscription"]
            }),
        );
        // The supervisor consumes events before delivery. Do not read or
        // react to Status until it has completed the whole request.
        settle(&mut hub);
        assert_eq!(launches.get(), 0);
        let events = drain(&mut output[0]);
        assert_eq!(events[0]["event"]["status"]["sign_in"], "api_key");
        assert_eq!(events[0]["event"]["status"]["readiness"]["source"], "fresh");
        assert_eq!(
            failure_reason(&events).as_deref(),
            Some("SIGN_IN_POLICY_DENIED")
        );
        assert!(!events.iter().any(|e| e["event"]["type"] == "launched"));
        for policy in [
            json!(null),
            json!([]),
            json!(["bad"]),
            json!(vec!["subscription"; 5]),
        ] {
            request(
                &mut hub,
                1,
                "invalid-policy",
                "send_ready_with_policy",
                json!({"turn":turn,"freshness":cached,"allowed_sign_in":policy}),
            );
            settle(&mut hub);
            assert_eq!(
                failure_reason(&drain(&mut output[0])).as_deref(),
                Some("INVALID_REQUEST")
            );
            assert_eq!(launches.get(), 0);
        }
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn grant_and_workspace_changes_drop_only_that_apps_readiness() {
        let (mut hub, mut output) = setup();
        for app in ["first", "second"] {
            install_provider(
                &mut hub,
                app,
                Box::new(seatline_providers::readiness::Ready::new(ReadyFixture(
                    std::rc::Rc::new(std::cell::Cell::new(0)),
                ))),
            );
        }
        let mut grant = config::load_grant(&hub.root, "first").unwrap();
        grant.cache_title = Some("Different workspace".into());
        config::write_private(
            &config::app_path(&hub.root, "first").unwrap(),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        request(
            &mut hub,
            1,
            "revoked",
            "prepare",
            json!({"mode":"cached","max_age_ms":30000}),
        );
        assert!(!hub.connections.contains_key(&1));
        assert!(
            !hub.providers
                .contains_key(&("first".into(), "codex".into()))
        );
        assert!(
            hub.providers
                .contains_key(&("second".into(), "codex".into()))
        );
        request(
            &mut hub,
            2,
            "healthy",
            "prepare",
            json!({"mode":"cached","max_age_ms":30000}),
        );
        settle(&mut hub);
        assert!(
            drain(&mut output[1])
                .iter()
                .any(|v| v["event"]["type"] == "completed")
        );
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    fn reconnect(hub: &mut Hub, grant: Grant) -> tokio::sync::mpsc::Receiver<Value> {
        let (send, mut receive) = tokio::sync::mpsc::channel(64);
        hub.command(Command::Open {
            connection: 3,
            grant: Box::new(grant),
            output: send,
        });
        assert_eq!(receive.try_recv().unwrap()["type"], "ready");
        receive
    }

    #[test]
    fn unchanged_grant_reuses_readiness_after_disconnect_and_reconnect() {
        let (mut hub, mut output) = setup();
        let counter = std::rc::Rc::new(std::cell::Cell::new(0));
        install_provider(
            &mut hub,
            "first",
            Box::new(seatline_providers::readiness::Ready::new(ReadyFixture(
                counter.clone(),
            ))),
        );
        let cached = json!({"mode":"cached","max_age_ms":30000});
        request(&mut hub, 1, "warm", "prepare", cached.clone());
        settle(&mut hub);
        assert_eq!(failure_reason(&drain(&mut output[0])), None);
        hub.command(Command::Close(1));
        let grant = config::load_grant(&hub.root, "first").unwrap();
        let mut reopened = reconnect(&mut hub, grant);
        request(&mut hub, 3, "reuse", "prepare", cached);
        settle(&mut hub);
        let events = drain(&mut reopened);
        assert_eq!(
            events[0]["event"]["status"]["readiness"]["source"],
            "cached"
        );
        assert_eq!(counter.get(), 1);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn disconnected_grant_changes_rebuild_only_that_apps_providers() {
        for change in ["token", "workspace", "providers", "tools", "recreate"] {
            let (mut hub, mut output) = setup();
            let mut counters = Vec::new();
            let cached = json!({"mode":"cached","max_age_ms":30000});
            for (connection, app) in [(1, "first"), (2, "second")] {
                let counter = std::rc::Rc::new(std::cell::Cell::new(0));
                install_provider(
                    &mut hub,
                    app,
                    Box::new(seatline_providers::readiness::Ready::new(ReadyFixture(
                        counter.clone(),
                    ))),
                );
                counters.push(counter);
                request(&mut hub, connection, "warm", "prepare", cached.clone());
            }
            settle(&mut hub);
            for receive in &mut output {
                assert_eq!(failure_reason(&drain(receive)), None);
            }
            // No old connection remains to fail authorized().
            hub.command(Command::Close(1));
            let mut grant = config::load_grant(&hub.root, "first").unwrap();
            match change {
                "token" => grant.token = config::random_token().unwrap(),
                "workspace" => grant.cache_title = Some("FIRST".into()),
                "providers" => grant.providers.push("claude".into()),
                "tools" => grant.allow_provider_default = true,
                "recreate" => {
                    std::fs::remove_file(config::app_path(&hub.root, "first").unwrap()).unwrap();
                    grant.token = config::random_token().unwrap();
                }
                _ => unreachable!(),
            }
            config::write_private(
                &config::app_path(&hub.root, "first").unwrap(),
                &serde_json::to_vec(&grant).unwrap(),
            )
            .unwrap();
            let mut reopened = reconnect(&mut hub, grant.clone());
            // Invalid turn data exercises admission/rebuilding without ever
            // probing an installed real CLI or launching a model turn.
            request(&mut hub, 3, "admit", "send", Value::Null);
            settle(&mut hub);
            assert_eq!(
                failure_reason(&drain(&mut reopened)).as_deref(),
                Some("INVALID_REQUEST")
            );
            assert!(
                hub.providers[&("first".into(), "codex".into())]
                    .grant
                    .same_provider_scope(&grant),
                "{change}"
            );
            request(&mut hub, 2, "healthy", "prepare", cached);
            settle(&mut hub);
            let events = drain(&mut output[1]);
            assert_eq!(
                events[0]["event"]["status"]["readiness"]["source"], "cached",
                "{change}"
            );
            assert_eq!((counters[0].get(), counters[1].get()), (1, 1));
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn changed_workspace_is_validated_again_after_reconnecting() {
        let (mut hub, mut output) = setup();
        install_provider(
            &mut hub,
            "first",
            Box::new(seatline_providers::readiness::Ready::new(ReadyFixture(
                std::rc::Rc::new(std::cell::Cell::new(0)),
            ))),
        );
        let cached = json!({"mode":"cached","max_age_ms":30000});
        request(&mut hub, 1, "warm", "prepare", cached.clone());
        settle(&mut hub);
        assert_eq!(failure_reason(&drain(&mut output[0])), None);
        hub.command(Command::Close(1));
        let mut grant = config::load_grant(&hub.root, "first").unwrap();
        grant.cache_title = Some("Different workspace".into());
        config::write_private(
            &config::app_path(&hub.root, "first").unwrap(),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        let mut reopened = reconnect(&mut hub, grant);
        request(&mut hub, 3, "changed", "prepare", cached);
        settle(&mut hub);
        assert_eq!(
            failure_reason(&drain(&mut reopened)).as_deref(),
            Some("INVALID_REQUEST")
        );
        assert!(
            !hub.providers
                .contains_key(&("first".into(), "codex".into()))
        );
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn cancelling_one_preparation_keeps_its_shared_peer_and_cached_result() {
        let (mut hub, mut output) = setup();
        let counter = std::rc::Rc::new(std::cell::Cell::new(0));
        install_provider(
            &mut hub,
            "first",
            Box::new(seatline_providers::readiness::Ready::new(ReadyFixture(
                counter.clone(),
            ))),
        );
        let cached = json!({"mode":"cached","max_age_ms":30000});
        request(&mut hub, 1, "a", "prepare", cached.clone());
        request(&mut hub, 1, "b", "prepare", cached.clone());
        hub.tick(); // Both requests admitted, the shared check not yet driven.
        hub.command(Command::Request {
            connection: 1,
            value: json!({"id":"cancel","method":"cancel","target":"a"}),
        });
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert_eq!(
            events
                .iter()
                .filter(|v| v["id"] == "a" && v["event"]["type"] == "stopped")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|v| v["id"] == "b" && v["event"]["type"] == "completed")
                .count(),
            1
        );
        assert!(hub.active.is_empty());
        request(&mut hub, 1, "after", "prepare", cached);
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert_eq!(
            events[0]["event"]["status"]["readiness"]["source"],
            "cached"
        );
        assert_eq!(counter.get(), 1);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    fn install_provider(hub: &mut Hub, app: &str, provider: Box<dyn Provider>) {
        let grant = config::load_grant(&hub.root, app).unwrap();
        hub.providers.insert(
            (app.into(), provider.id().into()),
            RetainedProvider { grant, provider },
        );
    }

    fn setup() -> (Hub, Vec<tokio::sync::mpsc::Receiver<Value>>) {
        let root =
            std::env::temp_dir().join(format!("seatline-hub-{}", config::random_token().unwrap()));
        let ledger = Ledger::new(root.clone(), Sessions::default()).unwrap();
        let mut hub = Hub {
            root,
            ledger,
            ledger_jobs: Vec::new(),
            policy: Policy::default(),
            last_app: None,
            interactive_streak: 0,
            quiet_ticks: 0,
            connections: BTreeMap::new(),
            queue: VecDeque::new(),
            providers: BTreeMap::new(),
            supervisor: Supervisor::new(),
            active: BTreeMap::new(),
            sessions: Sessions::default(),
            cleanup: Vec::new(),
            next_check: Instant::now() + Duration::from_secs(60),
            next_prune: Instant::now() + Duration::from_secs(3600),
            missing_grants: BTreeSet::new(),
            telemetry: Telemetry::disabled(),
        };
        let mut outputs = Vec::new();
        for (id, app) in [(1, "first"), (2, "second")] {
            let grant = Grant {
                app: app.into(),
                token: config::random_token().unwrap(),
                providers: vec!["codex".into()],
                allow_provider_default: false,
                extension_origins: Vec::new(),
                web_origins: Vec::new(),
                web_relays: Vec::new(),
                cache_title: None,
                native_adapter: None,
            };
            config::write_private(
                &config::app_path(&hub.root, app).unwrap(),
                &serde_json::to_vec(&grant).unwrap(),
            )
            .unwrap();
            let (output, mut receive) = tokio::sync::mpsc::channel(64);
            hub.command(Command::Open {
                connection: id,
                grant: Box::new(grant),
                output,
            });
            assert_eq!(receive.try_recv().unwrap()["type"], "ready");
            install_provider(&mut hub, app, Box::new(Fixture));
            outputs.push(receive);
        }
        (hub, outputs)
    }
    fn turn(continuation: Option<&str>) -> Value {
        json!({"system":null,"messages":[{"role":"user","text":"hello"}],"model":null,"tools":"none","session":"persistent","continuation":continuation,"cleanup_group":null,"check_sign_in":false})
    }
    fn request(hub: &mut Hub, connection: u64, id: &str, method: &str, params: Value) {
        hub.command(Command::Request {
            connection,
            value: json!({"id":id,"provider":"codex","method":method,"params":params}),
        });
    }
    fn drain(output: &mut tokio::sync::mpsc::Receiver<Value>) -> Vec<Value> {
        let mut values = Vec::new();
        while let Ok(value) = output.try_recv() {
            values.push(value);
        }
        values
    }
    #[test]
    fn session_tokens_are_app_scoped_and_native_handles_stay_private() {
        let (mut hub, mut output) = setup();
        request(&mut hub, 1, "a", "send", turn(None));
        request(&mut hub, 2, "b", "send", turn(None));
        settle(&mut hub);
        let first = drain(&mut output[0]);
        let second = drain(&mut output[1]);
        let token = |events: &[Value]| {
            events
                .iter()
                .find(|v| v["event"]["type"] == "session")
                .unwrap()["event"]["handle"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let (a, b) = (token(&first), token(&second));
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("raw-native-handle")
        );
        request(&mut hub, 2, "steal", "send", turn(Some(&a)));
        request(
            &mut hub,
            2,
            "forget-other",
            "forget",
            json!({"sessions":[a]}),
        );
        settle(&mut hub);
        let failures = drain(&mut output[1]);
        assert_eq!(failures.len(), 2);
        assert!(
            failures
                .iter()
                .all(|value| value["event"]["type"] == "failed")
        );
        assert_eq!(hub.sessions.len(), 2);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }
    #[test]
    fn cancellation_ids_and_revocation_do_not_cross_connections() {
        let (mut hub, mut output) = setup();
        request(&mut hub, 1, "same", "send", turn(None));
        request(&mut hub, 2, "same", "send", turn(None));
        hub.command(Command::Request {
            connection: 1,
            value: json!({"id":"cancel","method":"cancel","target":"same"}),
        });
        assert_eq!(hub.queue.len(), 1);
        assert_eq!(hub.queue[0].connection, 2);
        assert_eq!(drain(&mut output[0])[0]["event"]["type"], "stopped");
        std::fs::remove_file(config::app_path(&hub.root, "first").unwrap()).unwrap();
        hub.next_check = Instant::now();
        hub.tick();
        assert!(!hub.connections.contains_key(&1));
        assert!(hub.connections.contains_key(&2));
        settle(&mut hub);
        assert!(
            drain(&mut output[1])
                .iter()
                .any(|value| value["event"]["type"] == "completed")
        );
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    fn turn_with_tools(tools: &str) -> Value {
        let mut value = turn(None);
        value["tools"] = json!(tools);
        value
    }
    fn failure_reason(events: &[Value]) -> Option<String> {
        events
            .iter()
            .find(|value| value["event"]["type"] == "failed")
            .and_then(|value| value["event"]["reason"].as_str())
            .map(str::to_owned)
    }
    #[test]
    fn provider_default_tools_are_refused_with_a_visible_reason_until_the_grant_allows_them() {
        let (mut hub, mut output) = setup();
        request(&mut hub, 1, "plain", "send", turn_with_tools("none"));
        request(
            &mut hub,
            1,
            "default",
            "send",
            turn_with_tools("provider_default"),
        );
        settle(&mut hub);
        let events = drain(&mut output[0]);
        let outcome = |id: &str| -> Vec<Value> {
            events
                .iter()
                .filter(|value| value["id"] == id)
                .cloned()
                .collect()
        };
        assert!(
            outcome("plain")
                .iter()
                .any(|value| value["event"]["type"] == "completed")
        );
        assert_eq!(
            failure_reason(&outcome("default")).as_deref(),
            Some("PROVIDER_DEFAULT_TOOLS_DENIED")
        );

        // The local administrator opts in; the change takes effect for the next request.
        let mut grant = config::load_grant(&hub.root, "first").unwrap();
        grant.allow_provider_default = true;
        config::write_private(
            &config::app_path(&hub.root, "first").unwrap(),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        // A changed grant closes the old connection; the app reconnects with the new one.
        request(&mut hub, 1, "after-change", "status", Value::Null);
        assert!(!hub.connections.contains_key(&1));
        let (send, mut reopened) = tokio::sync::mpsc::channel(64);
        hub.command(Command::Open {
            connection: 3,
            grant: Box::new(grant),
            output: send,
        });
        assert_eq!(reopened.try_recv().unwrap()["type"], "ready");
        // Revocation also drops the old adapter and its readiness evidence.
        assert!(
            !hub.providers
                .contains_key(&("first".into(), "codex".into()))
        );
        install_provider(&mut hub, "first", Box::new(Fixture));
        request(
            &mut hub,
            3,
            "allowed",
            "send",
            turn_with_tools("provider_default"),
        );
        settle(&mut hub);
        let events = drain(&mut reopened);
        assert_eq!(failure_reason(&events), None);
        assert!(
            events
                .iter()
                .any(|value| value["event"]["type"] == "completed")
        );
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    fn fill_ledger(hub: &mut Hub, app: &str, count: usize) {
        for index in 0..count {
            hub.sessions.insert(
                format!("{app}-{index:060}"),
                Session {
                    app: app.into(),
                    provider: "codex".into(),
                    native: format!("native-{app}-{index}"),
                },
            );
        }
        hub.ledger = Ledger::new(hub.root.clone(), hub.sessions.clone()).unwrap();
    }

    #[test]
    fn ledger_failures_end_the_request_once_and_do_not_forward_late_output() {
        for storage_failure in [false, true] {
            let (mut hub, mut output) = setup();
            let reason = if storage_failure {
                // A directory at the ledger path forces atomic replacement to fail.
                std::fs::create_dir(hub.root.join("sessions.json")).unwrap();
                wire::reason::SESSION_STORE_FAILED
            } else {
                fill_ledger(&mut hub, "first", MAX_APP_SESSIONS);
                wire::reason::SESSION_LIMIT_REACHED
            };
            request(&mut hub, 1, "failing", "send", turn(None));
            let mut other_turn = turn(None);
            other_turn["session"] = json!("ephemeral");
            request(&mut hub, 2, "unrelated", "send", other_turn);
            settle(&mut hub);
            let failed = drain(&mut output[0]);
            assert_eq!(failed.len(), 1, "output leaked after {reason}: {failed:?}");
            assert_eq!(failed[0]["event"]["type"], "failed");
            assert_eq!(failure_reason(&failed).as_deref(), Some(reason));
            let other = drain(&mut output[1]);
            assert!(other.iter().any(|v| v["event"]["type"] == "delta"));
            assert_eq!(other.last().unwrap()["event"]["type"], "completed");
            assert!(hub.active.is_empty());
            assert!(hub.supervisor.is_empty());
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn an_early_terminal_failure_keeps_its_slot_but_releases_its_request_id() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct DelayedStop {
            session_sent: bool,
            release: Rc<Cell<bool>>,
            cancelled: Rc<Cell<bool>>,
        }
        impl Exchange for DelayedStop {
            fn next(&mut self, _: Instant) -> Option<Update> {
                if !std::mem::replace(&mut self.session_sent, true) {
                    Some(Update::Session("new-native-handle".into()))
                } else if self.release.get() {
                    Some(Update::Stopped)
                } else {
                    None
                }
            }
            fn cancel(&mut self, _: Duration) {
                self.cancelled.set(true);
            }
        }

        for storage_failure in [false, true] {
            let (mut hub, mut output) = setup();
            let reason = if storage_failure {
                std::fs::create_dir(hub.root.join("sessions.json")).unwrap();
                wire::reason::SESSION_STORE_FAILED
            } else {
                fill_ledger(&mut hub, "first", MAX_APP_SESSIONS);
                wire::reason::SESSION_LIMIT_REACHED
            };
            let release = Rc::new(Cell::new(false));
            let cancelled = Rc::new(Cell::new(false));
            let turn_id = hub.supervisor.start(
                Box::new(DelayedStop {
                    session_sent: false,
                    release: release.clone(),
                    cancelled: cancelled.clone(),
                }),
                None,
                Duration::from_secs(2),
            );
            hub.active.insert(
                turn_id,
                Active {
                    request: Request {
                        connection: 1,
                        id: "failing".into(),
                        app: "first".into(),
                        provider: "codex".into(),
                        method: "send".into(),
                        params: turn(None),
                        hints: Hints::default(),
                        expires: Instant::now() + Duration::from_secs(30),
                    },
                    persistent: true,
                    terminal_sent: false,
                    gate: Rc::new(Cell::new(true)),
                },
            );
            wait_until(&mut hub, |_| cancelled.get());
            assert!(cancelled.get());
            assert!(hub.active[&turn_id].terminal_sent);
            assert!(!hub.supervisor.is_empty());
            let failed = drain(&mut output[0]);
            assert_eq!(failed.len(), 1);
            assert_eq!(failure_reason(&failed).as_deref(), Some(reason));

            // The client can reuse the ID as soon as it receives the terminal
            // event, even while the old provider still occupies a cleanup slot.
            let mut retry = turn(None);
            retry["session"] = json!("ephemeral");
            request(&mut hub, 1, "failing", "send", retry);
            assert!(hub.connections.contains_key(&1));
            assert_eq!(hub.queue.len(), 1);
            wait_until(&mut hub, |hub| {
                hub.active.len() == 1 && hub.queue.is_empty() && hub.ledger_jobs.is_empty()
            });
            let retried = drain(&mut output[0]);
            assert!(retried.iter().all(|event| event["id"] == "failing"));
            assert!(retried.iter().any(|v| v["event"]["type"] == "delta"));
            assert_eq!(retried.last().unwrap()["event"]["type"], "completed");
            assert_eq!(
                retried
                    .iter()
                    .filter(|v| matches!(
                        v["event"]["type"].as_str(),
                        Some("completed" | "failed" | "stopped")
                    ))
                    .count(),
                1,
            );
            assert_eq!(hub.active.len(), 1);
            assert!(hub.active[&turn_id].terminal_sent);

            release.set(true);
            hub.tick();
            assert!(hub.active.is_empty());
            assert!(hub.supervisor.is_empty());
            assert!(drain(&mut output[0]).is_empty());
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn the_hub_says_whether_it_has_any_work_and_the_question_is_not_work() {
        let (mut hub, _output) = setup();
        let ask = |hub: &mut Hub| {
            let (reply, answer) = tokio::sync::oneshot::channel();
            hub.command(Command::Quiet(reply));
            answer.blocking_recv().unwrap()
        };
        assert!(ask(&mut hub), "a hub with nothing to do is quiet");
        request(&mut hub, 1, "work", "send", turn(None));
        assert!(!ask(&mut hub), "a queued request is work");
        hub.tick();
        assert_eq!(hub.active.len(), 1);
        assert!(!ask(&mut hub), "a running request is work");
        settle(&mut hub);
        assert!(ask(&mut hub), "quiet again once it has finished");
        // Asking is not activity: it must not restart the 1 ms burst timing.
        hub.quiet_ticks = 7;
        ask(&mut hub);
        assert_eq!(hub.quiet_ticks, 7);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn duplicate_outstanding_request_ids_still_close_the_connection() {
        for active in [false, true] {
            let (mut hub, _output) = setup();
            request(&mut hub, 1, "duplicate", "send", turn(None));
            if active {
                hub.tick();
                assert_eq!(hub.active.len(), 1);
                assert!(!hub.active.values().next().unwrap().terminal_sent);
            } else {
                assert_eq!(hub.queue.len(), 1);
            }
            request(&mut hub, 1, "duplicate", "send", turn(None));
            assert!(!hub.connections.contains_key(&1));
            assert!(hub.queue.is_empty());
            settle(&mut hub);
            assert!(hub.active.is_empty());
            assert!(hub.supervisor.is_empty());
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn one_app_cannot_use_up_the_session_ledger_for_the_others() {
        let (mut hub, _output) = setup();
        fill_ledger(&mut hub, "first", MAX_APP_SESSIONS);
        assert!(matches!(
            hub.session_token(0, "first", "codex", "brand-new".into()),
            Err(LedgerError::Full)
        ));
        // A session that already has a token keeps working at the cap, without a rewrite.
        assert!(
            hub.session_token(0, "first", "codex", "native-first-7".into())
                .is_ok()
        );
        // Other apps still have room.
        assert!(
            hub.session_token(0, "second", "codex", "another".into())
                .unwrap_or_else(|_| panic!("second app was locked out"))
                .is_none()
        );
        settle(&mut hub);
        let token = hub.sessions.token("second", "codex", "another").unwrap();
        assert_eq!(hub.sessions[token].app, "second");
        // The global cap still applies.
        let room = MAX_SESSIONS - hub.sessions.len();
        fill_ledger(&mut hub, "third", room);
        assert!(matches!(
            hub.session_token(0, "second", "codex", "over-the-cap".into()),
            Err(LedgerError::Full)
        ));
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn revoking_an_app_frees_its_sessions_after_two_sweeps() {
        let (mut hub, _output) = setup();
        fill_ledger(&mut hub, "first", 3);
        fill_ledger(&mut hub, "second", 2);
        config::write_private(
            &hub.root.join("sessions.json"),
            &serde_json::to_vec(&hub.sessions).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(config::app_path(&hub.root, "first").unwrap()).unwrap();
        hub.prune_revoked_sessions();
        assert_eq!(hub.sessions.len(), 5, "one sweep is not enough");
        hub.prune_revoked_sessions();
        settle(&mut hub);
        assert_eq!(hub.sessions.len(), 2);
        assert!(hub.sessions.values().all(|session| session.app == "second"));
        let saved: BTreeMap<String, Session> =
            serde_json::from_slice(&std::fs::read(hub.root.join("sessions.json")).unwrap())
                .unwrap();
        assert_eq!(saved.len(), 2);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn a_grant_that_reappears_between_sweeps_keeps_its_sessions() {
        let (mut hub, _output) = setup();
        fill_ledger(&mut hub, "first", 2);
        let path = config::app_path(&hub.root, "first").unwrap();
        let grant = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        hub.prune_revoked_sessions();
        std::fs::write(&path, grant).unwrap();
        hub.prune_revoked_sessions();
        hub.prune_revoked_sessions();
        settle(&mut hub);
        assert_eq!(hub.sessions.len(), 2);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    use seatline_core::telemetry::{Memory, Outcome, Record, RequestRecord};

    /// [`setup`], with phase telemetry going to the returned sink.
    fn telemetry_setup() -> (Hub, Vec<tokio::sync::mpsc::Receiver<Value>>, Arc<Memory>) {
        let (mut hub, output) = setup();
        let memory = Arc::new(Memory::new());
        hub.telemetry = Telemetry::new(memory.clone());
        (hub, output, memory)
    }

    fn requests(memory: &Memory) -> Vec<RequestRecord> {
        memory
            .take()
            .into_iter()
            .filter_map(|record| match record {
                Record::Request(record) => Some(*record),
                _ => None,
            })
            .collect()
    }

    fn ticks(hub: &mut Hub, count: usize) {
        for _ in 0..count {
            hub.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Filesystem completion is asynchronous and has no fixed tick count,
    /// especially on Windows. Wait for the state being asserted, with a bound.
    fn wait_until(hub: &mut Hub, mut ready: impl FnMut(&Hub) -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !ready(hub) {
            assert!(
                Instant::now() < until,
                "hub did not reach expected state: queue={}, active={}, ledger={}, cleanup={}",
                hub.queue.len(),
                hub.active.len(),
                hub.ledger_jobs.len(),
                hub.cleanup.len()
            );
            hub.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn settle(hub: &mut Hub) {
        wait_until(hub, |hub| hub.quiet());
    }

    #[test]
    fn a_send_leaves_one_record_whose_marks_are_in_order_and_whose_phases_tile() {
        let (mut hub, mut output, memory) = telemetry_setup();
        request(&mut hub, 1, "a", "send", turn(None));
        settle(&mut hub);
        let events = drain(&mut output[0]);
        let records = requests(&memory);
        assert_eq!(records.len(), 1, "{records:?}");
        let record = &records[0];
        assert_eq!(
            (
                record.connection,
                record.request.as_str(),
                record.app.as_str()
            ),
            (1, "a", "first")
        );
        assert_eq!(
            (record.provider.as_str(), record.method.as_str()),
            ("codex", "send")
        );
        assert_eq!(record.outcome, Outcome::Completed);
        assert!(record.text);
        let marks = record.marks_us;
        let order = [
            marks.admitted,
            marks.built,
            marks.started,
            marks.first_text,
            marks.terminal,
            marks.released,
        ];
        assert!(order.iter().all(Option::is_some), "{marks:?}");
        let order: Vec<u64> = order.into_iter().flatten().collect();
        assert!(order.windows(2).all(|pair| pair[0] <= pair[1]), "{marks:?}");
        assert_eq!(record.phases_us.sum(), record.total_us);

        // Nothing the request or the provider said is in the record.
        let json = serde_json::to_string(&records).unwrap();
        let token = events
            .iter()
            .find(|v| v["event"]["type"] == "session")
            .and_then(|v| v["event"]["handle"].as_str())
            .unwrap();
        for private in ["hello", "answer", "raw-native-handle", token] {
            assert!(!json.contains(private), "{private} leaked into {json}");
        }
        assert_eq!(hub.telemetry.in_flight(), 0);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    /// A turn whose adapter says when it read the provider's final result,
    /// and reports the counts the provider gave.
    struct ReportsItsResult {
        exchange: Scripted,
        result: Instant,
    }
    impl Exchange for ReportsItsResult {
        fn next(&mut self, deadline: Instant) -> Option<Update> {
            self.exchange.next(deadline)
        }
        fn cancel(&mut self, grace: Duration) {
            self.exchange.cancel(grace);
        }
        fn result_at(&self) -> Option<Instant> {
            Some(self.result)
        }
    }
    struct ReportingFixture;
    impl Provider for ReportingFixture {
        fn id(&self) -> &str {
            "codex"
        }
        fn capabilities(&self) -> Capabilities {
            codex::CAPABILITIES
        }
        fn timeouts(&self) -> Timeouts {
            Fixture.timeouts()
        }
        fn supports_persistent_session(&self) -> bool {
            true
        }
        fn status(&self) -> Box<dyn Exchange> {
            Fixture.status()
        }
        fn send(&self, _: Turn) -> Box<dyn Exchange> {
            Box::new(ReportsItsResult {
                exchange: Scripted::new([
                    Update::Session("raw-native-handle".into()),
                    Update::Started,
                    Update::Delta("answer".into()),
                    Update::Usage(seatline_core::turn::Usage {
                        input_tokens: Some(1_000),
                        output_tokens: Some(300),
                        cached_input_tokens: Some(800),
                        cache_write_input_tokens: Some(50),
                        reasoning_output_tokens: Some(250),
                    }),
                    Update::Completed,
                ]),
                result: Instant::now(),
            })
        }
    }

    #[test]
    fn a_persistent_turns_result_time_and_usage_reach_its_record_through_the_session_gate() {
        use seatline_core::telemetry::UsageRecord;
        let (mut hub, mut output, memory) = telemetry_setup();
        // A persistent turn runs behind the hub's session gate, which has to
        // pass the adapter's report on, or the record loses it.
        install_provider(&mut hub, "first", Box::new(ReportingFixture));
        request(&mut hub, 1, "a", "send", turn(None));
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert!(events.iter().any(|v| v["event"]["type"] == "session"));
        let records = requests(&memory);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].outcome, Outcome::Completed);
        assert!(records[0].tail_us.is_some(), "{:?}", records[0]);
        assert_eq!(
            records[0].usage,
            Some(UsageRecord {
                input_tokens: Some(1_000),
                cached_input_tokens: Some(800),
                cache_write_input_tokens: Some(50),
                output_tokens: Some(300),
                reasoning_output_tokens: Some(250),
            })
        );
        // The application is told the totals it always was, and nothing more.
        let usage = events
            .iter()
            .find(|v| v["event"]["type"] == "usage")
            .expect("the usage reached the application");
        assert_eq!(
            usage["event"]["usage"],
            json!({"input_tokens": 1_000, "output_tokens": 300})
        );
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn a_request_cancelled_while_queued_is_recorded_as_waiting_and_cancelled() {
        let (mut hub, _output, memory) = telemetry_setup();
        for id in ["a", "b", "c"] {
            request(&mut hub, 1, id, "send", turn(None));
        }
        // One app runs two at a time: the third is still queued.
        hub.tick();
        assert_eq!(hub.queue.len(), 1);
        hub.command(Command::Request {
            connection: 1,
            value: json!({"id":"cancel","method":"cancel","target":"c"}),
        });
        let records = requests(&memory);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].request, "c");
        assert_eq!(records[0].outcome, Outcome::Cancelled);
        assert!(records[0].phases_us.queue_wait.is_some());
        assert_eq!(records[0].phases_us.provider_init, None);
        assert_eq!(records[0].phases_us.sum(), records[0].total_us);
        settle(&mut hub);
        assert_eq!(requests(&memory).len(), 2, "the other two ran");
        assert_eq!(hub.telemetry.in_flight(), 0);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn a_full_queue_is_a_recorded_refusal_with_its_reason() {
        let (mut hub, _output, memory) = telemetry_setup();
        for index in 0..=MAX_APP_QUEUE {
            request(&mut hub, 1, &format!("r{index}"), "send", turn(None));
        }
        let records = requests(&memory);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].outcome, Outcome::Failed);
        assert_eq!(records[0].detail, Some("QUEUE_FULL"));
        assert_eq!(records[0].request, format!("r{MAX_APP_QUEUE}"));
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn closing_a_connection_ends_the_records_of_its_queued_and_running_requests() {
        let (mut hub, _output, memory) = telemetry_setup();
        for id in ["a", "b", "c"] {
            request(&mut hub, 1, id, "send", turn(None));
        }
        hub.tick();
        hub.command(Command::Close(1));
        settle(&mut hub);
        let mut records = requests(&memory);
        records.sort_by(|a, b| a.request.cmp(&b.request));
        assert_eq!(
            records
                .iter()
                .map(|r| r.request.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert!(records.iter().all(|r| r.outcome == Outcome::Cancelled));
        // The two that were running were asked to stop; the queued one never ran.
        assert!(records[0].marks_us.stop_requested.is_some());
        assert_eq!(records[2].marks_us.admitted, None);
        assert!(records.iter().all(|r| r.phases_us.sum() == r.total_us));
        assert_eq!(hub.telemetry.in_flight(), 0);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn status_and_cleanup_requests_are_recorded_by_their_own_kind() {
        let (mut hub, _output, memory) = telemetry_setup();
        request(&mut hub, 1, "s", "status", Value::Null);
        request(&mut hub, 2, "f", "forget", json!({"sessions": []}));
        let give_up = Instant::now() + Duration::from_secs(5);
        let mut records = Vec::new();
        while records.len() < 2 && Instant::now() < give_up {
            hub.tick();
            records.extend(requests(&memory));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(records.len(), 2, "{records:?}");
        records.sort_by(|a, b| a.method.cmp(&b.method));
        assert_eq!(records[0].method, "forget");
        assert!(records[0].phases_us.cleanup.is_some());
        assert_eq!(records[0].phases_us.provider_init, None);
        assert_eq!(records[1].method, "status");
        assert!(records.iter().all(|r| r.outcome == Outcome::Completed));
        assert!(records.iter().all(|r| r.phases_us.sum() == r.total_us));
        assert_eq!(hub.telemetry.in_flight(), 0);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn every_request_gets_exactly_one_record() {
        let (mut hub, _output, memory) = telemetry_setup();
        for (connection, id) in [(1, "a"), (1, "b"), (2, "a"), (2, "b"), (2, "c")] {
            request(&mut hub, connection, id, "send", turn(None));
        }
        settle(&mut hub);
        let mut seen: Vec<(u64, String)> = requests(&memory)
            .into_iter()
            .map(|record| (record.connection, record.request))
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [(1, "a"), (1, "b"), (2, "a"), (2, "b"), (2, "c")].map(|(c, id)| (c, id.to_owned()))
        );
        assert_eq!(hub.telemetry.in_flight(), 0);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn a_method_nobody_serves_is_a_failed_record_that_does_not_echo_it() {
        let (mut hub, _output, memory) = telemetry_setup();
        request(&mut hub, 1, "odd", "shell-me-a-secret", Value::Null);
        settle(&mut hub);
        let records = requests(&memory);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].method, "unknown");
        assert_eq!(records[0].outcome, Outcome::Failed);
        assert_eq!(records[0].detail, Some("INVALID_REQUEST"));
        assert!(!serde_json::to_string(&records).unwrap().contains("secret"));
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn a_ledger_failure_is_recorded_as_the_failure_the_client_was_told() {
        let (mut hub, mut output, memory) = telemetry_setup();
        fill_ledger(&mut hub, "first", MAX_APP_SESSIONS);
        // The turn is persistent, so its session handle needs a ledger slot.
        request(&mut hub, 1, "full", "send", turn(None));
        settle(&mut hub);
        let told = drain(&mut output[0]);
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0]["event"]["type"], "failed");
        let records = requests(&memory);
        assert_eq!(records.len(), 1, "{records:?}");
        // The scheduler may have seen the exchange complete already, or end it
        // as cancelled after the hub stopped it: either way the record says what
        // the client was told, not how the exchange ended.
        assert_eq!(records[0].outcome, Outcome::Failed);
        assert_eq!(records[0].detail, Some("SESSION_LIMIT_REACHED"));
        assert_eq!(records[0].phases_us.sum(), records[0].total_us);
        assert_eq!(hub.telemetry.in_flight(), 0);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn with_telemetry_off_the_hub_keeps_no_timelines() {
        let (mut hub, mut output) = setup();
        request(&mut hub, 1, "a", "send", turn(None));
        request(&mut hub, 2, "b", "send", turn(None));
        settle(&mut hub);
        assert!(!hub.telemetry.enabled());
        assert_eq!(hub.telemetry.in_flight(), 0);
        // The requests still ran.
        assert!(
            drain(&mut output[0])
                .iter()
                .any(|v| v["event"]["type"] == "completed")
        );
        std::fs::remove_dir_all(&hub.root).unwrap();
    }
    #[test]
    fn slow_persistence_gates_the_handle_and_answer_but_other_apps_and_cancel_keep_running() {
        let (mut hub, mut output) = setup();
        let (release, wait) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        let root = hub.root.clone();
        hub.ledger = Ledger::with_writer(hub.sessions.clone(), move |sessions| {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            config::write_private(&root.join("sessions.json"), &serde_json::to_vec(sessions)?)
        })
        .unwrap();
        request(&mut hub, 1, "slow", "send", turn(None));
        let mut ephemeral = turn(None);
        ephemeral["session"] = json!("ephemeral");
        request(&mut hub, 2, "other", "send", ephemeral.clone());
        ticks(&mut hub, 6);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            drain(&mut output[0]).is_empty(),
            "undurable handle/answer escaped"
        );
        assert_eq!(
            drain(&mut output[1]).last().unwrap()["event"]["type"],
            "completed"
        );
        hub.command(Command::Request {
            connection: 1,
            value: json!({"id":"cancel","method":"cancel","target":"slow"}),
        });
        ticks(&mut hub, 4);
        assert_eq!(
            drain(&mut output[0]).last().unwrap()["event"]["type"],
            "stopped"
        );
        request(&mut hub, 1, "slow", "send", ephemeral);
        ticks(&mut hub, 4);
        assert_eq!(
            drain(&mut output[0]).last().unwrap()["event"]["type"],
            "completed"
        );
        release.send(()).unwrap();
        settle(&mut hub);
        assert!(
            drain(&mut output[0]).is_empty(),
            "cancelled handle acknowledged late"
        );
        assert!(hub.active.is_empty());
        assert!(hub.ledger_jobs.is_empty());
        let restarted: Sessions =
            serde_json::from_slice(&std::fs::read(hub.root.join("sessions.json")).unwrap())
                .unwrap();
        assert_eq!(restarted.len(), 1);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn a_successful_session_is_announced_only_after_a_durable_write_and_reuse_does_not_rewrite() {
        let (mut hub, mut output) = setup();
        let (release, wait) = mpsc::channel();
        let root = hub.root.clone();
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = writes.clone();
        hub.ledger = Ledger::with_writer(hub.sessions.clone(), move |sessions| {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            wait.recv().unwrap();
            // Even after release, a durable write can outlive several ticks.
            std::thread::sleep(Duration::from_millis(25));
            config::write_private(&root.join("sessions.json"), &serde_json::to_vec(sessions)?)
        })
        .unwrap();
        request(&mut hub, 1, "a", "send", turn(None));
        ticks(&mut hub, 4);
        assert!(drain(&mut output[0]).is_empty());
        release.send(()).unwrap();
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert_eq!(events[0]["event"]["type"], "session");
        let token = events[0]["event"]["handle"].as_str().unwrap();
        let restarted: Sessions =
            serde_json::from_slice(&std::fs::read(hub.root.join("sessions.json")).unwrap())
                .unwrap();
        assert_eq!(restarted[token].native, "raw-native-handle");
        request(&mut hub, 1, "b", "send", turn(Some(token)));
        settle(&mut hub);
        assert_eq!(
            drain(&mut output[0]).last().unwrap()["event"]["type"],
            "completed"
        );
        assert_eq!(writes.load(std::sync::atomic::Ordering::Relaxed), 1);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn queue_events_deadlines_and_idle_wait_are_bounded_and_legacy_clients_are_unchanged() {
        let (mut hub, mut output) = setup();
        let now = Instant::now();
        hub.next_check = now + Duration::from_secs(1);
        hub.next_prune = now + Duration::from_secs(60);
        assert_eq!(hub.wait_time(now), Duration::from_secs(1));
        hub.command(Command::Request { connection:1, value:json!({"id":"timed","provider":"codex","method":"status","params":{},"scheduling":{"events":true,"queue_timeout_ms":100}}) });
        assert_eq!(drain(&mut output[0])[0]["event"]["type"], "queued");
        hub.queue[0].expires = now;
        hub.tick();
        let events = drain(&mut output[0]);
        assert_eq!(events.len(), 1);
        assert_eq!(failure_reason(&events).as_deref(), Some("QUEUE_TIMEOUT"));
        assert!(hub.supervisor.is_empty());
        hub.command(Command::Request { connection:1, value:json!({"id":"visible","provider":"codex","method":"status","params":{},"scheduling":{"events":true}}) });
        settle(&mut hub);
        let events = drain(&mut output[0]);
        assert_eq!(
            events
                .iter()
                .map(|v| v["event"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["queued", "admitted", "completed"]
        );
        request(&mut hub, 1, "legacy", "status", json!({}));
        settle(&mut hub);
        assert_eq!(drain(&mut output[0]).len(), 1);
        hub.connections.clear();
        assert_eq!(hub.wait_time(now), Duration::from_secs(60));
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn app_rotation_and_bounded_interactive_bursts_do_not_starve_regular_work() {
        let (mut hub, _output) = setup();
        // Three apps, repeated interactive work, and a regular request. Test
        // admission opportunities, independently of provider/network duration.
        for (i, app, interactive) in [
            (0, "a", true),
            (1, "a", true),
            (2, "b", true),
            (3, "c", true),
            (4, "c", false),
            (5, "b", true),
            (6, "a", true),
        ] {
            hub.queue.push_back(Request {
                connection: 1,
                id: i.to_string(),
                app: app.into(),
                provider: "codex".into(),
                method: "send".into(),
                params: turn(None),
                hints: Hints {
                    interactive,
                    ..Hints::default()
                },
                expires: Instant::now() + Duration::from_secs(30),
            });
        }
        let mut chosen = Vec::new();
        while let Some(index) = hub.next_request() {
            let request = hub.queue.remove(index).unwrap();
            if Hub::interactive(&request) {
                hub.interactive_streak += 1;
            } else {
                hub.interactive_streak = 0;
            }
            hub.last_app = Some(request.app.clone());
            chosen.push(request.id);
        }
        assert_eq!(&chosen[..4], ["0", "2", "3", "4"]);
        assert_eq!(chosen.len(), 7);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn waiting_cleanup_drains_staggered_generations_before_new_work_without_blocking_other_apps() {
        for method in ["cleanup", "forget"] {
            let (mut hub, mut output) = setup();
            let gates: Rc<RefCell<HashMap<String, Rc<Cell<bool>>>>> =
                Rc::new(RefCell::new(HashMap::new()));
            let started = Rc::new(RefCell::new(Vec::<String>::new()));
            let cleaned = Rc::new(Cell::new(0));
            struct Pending {
                release: Rc<Cell<bool>>,
                cursor: u8,
            }
            impl Exchange for Pending {
                fn next(&mut self, _: Instant) -> Option<Update> {
                    match self.cursor {
                        0 => {
                            self.cursor = 1;
                            Some(Update::Launched)
                        }
                        1 => {
                            self.cursor = 2;
                            Some(Update::Started)
                        }
                        2 if self.release.get() => {
                            self.cursor = 3;
                            Some(Update::Completed)
                        }
                        _ => None,
                    }
                }
                fn cancel(&mut self, _: Duration) {
                    self.release.set(true);
                }
            }
            struct Draining {
                gates: Rc<RefCell<HashMap<String, Rc<Cell<bool>>>>>,
                started: Rc<RefCell<Vec<String>>>,
                cleaned: Rc<Cell<usize>>,
            }
            impl Provider for Draining {
                fn id(&self) -> &str {
                    "codex"
                }
                fn capabilities(&self) -> Capabilities {
                    Fixture.capabilities()
                }
                fn timeouts(&self) -> Timeouts {
                    Fixture.timeouts()
                }
                fn status(&self) -> Box<dyn Exchange> {
                    Fixture.status()
                }
                fn send(&self, turn: Turn) -> Box<dyn Exchange> {
                    let id = turn.messages[0].text.clone();
                    if id == "newer" {
                        assert_eq!(self.cleaned.get(), 1, "newer generation overtook cleanup");
                    }
                    self.started.borrow_mut().push(id.clone());
                    let release = Rc::new(Cell::new(false));
                    self.gates.borrow_mut().insert(id, release.clone());
                    Box::new(Pending { release, cursor: 0 })
                }
                fn cleanup_sessions(&self, _: &[String]) -> Cleanup {
                    let cleaned = self.cleaned.clone();
                    Cleanup::new(|| Ok(()), move || cleaned.set(cleaned.get() + 1))
                }
                fn cleanup_group(&self, _: &str) -> Cleanup {
                    self.cleanup_sessions(&[])
                }
            }
            install_provider(
                &mut hub,
                "first",
                Box::new(Draining {
                    gates: gates.clone(),
                    started: started.clone(),
                    cleaned: cleaned.clone(),
                }),
            );
            let ask = |name: &str| {
                let mut turn = turn_with_tools("none");
                turn["session"] = json!("ephemeral");
                turn["messages"][0]["text"] = json!(name);
                turn
            };
            request(&mut hub, 1, "first", "send", ask("first"));
            request(&mut hub, 1, "second", "send", ask("second"));
            wait_until(&mut hub, |_| started.borrow().len() == 2);
            request(
                &mut hub,
                1,
                "cleanup",
                method,
                if method == "cleanup" {
                    json!({"group":"run"})
                } else {
                    json!({"sessions":[]})
                },
            );
            request(&mut hub, 1, "newer", "send", ask("newer"));
            hub.queue.back_mut().unwrap().hints.interactive = true;
            gates.borrow()["first"].set(true);
            wait_until(&mut hub, |hub| hub.active.len() == 1);
            assert_eq!(&*started.borrow(), &["first", "second"]);
            assert_eq!(cleaned.get(), 0);
            assert_eq!(hub.queue.len(), 2);
            request(&mut hub, 2, "other", "send", ask("other"));
            request(&mut hub, 1, "readiness", "status", json!({}));
            wait_until(&mut hub, |hub| {
                !hub.active
                    .values()
                    .any(|a| a.request.id == "other" || a.request.id == "readiness")
                    && hub.queue.len() == 2
            });
            assert!(
                drain(&mut output[1])
                    .iter()
                    .any(|e| e["id"] == "other" && e["event"]["type"] == "completed")
            );
            assert!(
                drain(&mut output[0])
                    .iter()
                    .any(|e| e["id"] == "readiness" && e["event"]["type"] == "completed")
            );
            gates.borrow()["second"].set(true);
            wait_until(&mut hub, |_| started.borrow().len() == 3);
            assert_eq!(cleaned.get(), 1);
            let events = drain(&mut output[0]);
            assert!(
                events
                    .iter()
                    .any(|e| e["id"] == "cleanup" && e["event"]["type"] == "completed"),
                "{events:?}"
            );
            gates.borrow()["newer"].set(true);
            settle(&mut hub);
            assert_eq!(hub.queue.len(), 0);
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn another_apps_full_cleanup_lane_does_not_hold_back_generations() {
        for method in ["cleanup", "forget"] {
            let (mut hub, mut output) = setup();
            hub.policy.max_cleanup_running = 1;
            struct Held(Rc<Cell<bool>>);
            impl Exchange for Held {
                fn next(&mut self, _: Instant) -> Option<Update> {
                    self.0.replace(false).then_some(Update::Completed)
                }
                fn cancel(&mut self, _: Duration) {
                    self.0.set(true);
                }
            }
            let released = Rc::new(Cell::new(false));
            let id = hub
                .supervisor
                .start(Box::new(Held(released.clone())), None, Duration::ZERO);
            hub.active.insert(
                id,
                Active {
                    request: Request {
                        connection: 2,
                        id: "other-cleanup".into(),
                        app: "second".into(),
                        provider: "codex".into(),
                        method: "cleanup".into(),
                        params: json!({"group":"other"}),
                        hints: Hints::default(),
                        expires: Instant::now() + Duration::from_secs(30),
                    },
                    persistent: false,
                    terminal_sent: false,
                    gate: Rc::new(Cell::new(true)),
                },
            );
            request(
                &mut hub,
                1,
                "waiting-cleanup",
                method,
                if method == "cleanup" {
                    json!({"group":"run"})
                } else {
                    json!({"sessions":[]})
                },
            );
            let mut ask = turn_with_tools("none");
            ask["session"] = json!("ephemeral");
            request(&mut hub, 1, "generation", "send", ask);
            wait_until(&mut hub, |hub| {
                hub.queue.len() == 1 && hub.active.len() == 1
            });
            assert_eq!(hub.queue[0].id, "waiting-cleanup");
            let events = drain(&mut output[0]);
            assert!(
                events
                    .iter()
                    .any(|e| e["id"] == "generation" && e["event"]["type"] == "completed"),
                "{events:?}"
            );
            released.set(true);
            settle(&mut hub);
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn cancelling_or_expiring_a_cleanup_releases_its_generation_barrier() {
        for cancel in [true, false] {
            let (mut hub, mut output) = setup();
            struct Pending;
            impl Exchange for Pending {
                fn next(&mut self, _: Instant) -> Option<Update> {
                    None
                }
                fn cancel(&mut self, _: Duration) {}
            }
            let active = hub
                .supervisor
                .start(Box::new(Pending), None, Duration::ZERO);
            hub.active.insert(
                active,
                Active {
                    request: Request {
                        connection: 1,
                        id: "existing".into(),
                        app: "first".into(),
                        provider: "codex".into(),
                        method: "send".into(),
                        params: turn_with_tools("none"),
                        hints: Hints::default(),
                        expires: Instant::now() + Duration::from_secs(30),
                    },
                    persistent: false,
                    terminal_sent: false,
                    gate: Rc::new(Cell::new(true)),
                },
            );
            request(&mut hub, 1, "cleanup", "cleanup", json!({"group":"run"}));
            let mut later = turn_with_tools("none");
            later["session"] = json!("ephemeral");
            request(&mut hub, 1, "later", "send", later);
            hub.tick();
            assert_eq!(hub.queue.len(), 2);
            assert_eq!(hub.active.len(), 1);
            if cancel {
                hub.command(Command::Request {
                    connection: 1,
                    value: json!({"id":"cancel","method":"cancel","target":"cleanup"}),
                });
            } else {
                hub.queue[0].expires = Instant::now();
            }
            wait_until(&mut hub, |hub| {
                hub.queue.is_empty() && hub.active.len() == 1
            });
            let events = drain(&mut output[0]);
            assert!(
                events
                    .iter()
                    .any(|e| e["id"] == "later" && e["event"]["type"] == "completed"),
                "{events:?}"
            );
            assert!(events.iter().any(|e| e["id"] == "cleanup"
                && e["event"]["type"] == if cancel { "stopped" } else { "failed" }));
            if !cancel {
                assert_eq!(failure_reason(&events).as_deref(), Some("QUEUE_TIMEOUT"));
            }
            std::fs::remove_dir_all(&hub.root).unwrap();
        }
    }

    #[test]
    fn readiness_is_admitted_under_generation_contention_without_exceeding_limits() {
        let (mut hub, mut output) = setup();
        struct Pending;
        impl Exchange for Pending {
            fn next(&mut self, _: Instant) -> Option<Update> {
                None
            }
            fn cancel(&mut self, _: Duration) {}
        }
        for (id, connection, app) in [("long1", 1, "first"), ("long2", 2, "second")] {
            let turn = hub
                .supervisor
                .start(Box::new(Pending), None, Duration::ZERO);
            hub.active.insert(
                turn,
                Active {
                    request: Request {
                        connection,
                        id: id.into(),
                        app: app.into(),
                        provider: "codex".into(),
                        method: "send".into(),
                        params: turn_with_tools("none"),
                        hints: Hints::default(),
                        expires: Instant::now() + Duration::from_secs(30),
                    },
                    persistent: false,
                    terminal_sent: false,
                    gate: Rc::new(Cell::new(true)),
                },
            );
        }
        request(&mut hub, 1, "blocked", "send", turn_with_tools("none"));
        request(&mut hub, 2, "ready", "status", json!({}));
        hub.tick();
        assert_eq!(hub.queue.len(), 1);
        assert_eq!(hub.active.len(), 3);
        assert!(hub.active.len() <= hub.policy.max_running);
        hub.tick();
        assert_eq!(
            drain(&mut output[1]).last().unwrap()["event"]["type"],
            "completed"
        );
        assert_eq!(hub.active.len(), 2);
        assert!(drain(&mut output[0]).is_empty());
        assert_eq!(hub.wait_time(Instant::now()), Duration::from_millis(1));
        for delay in [2, 3, 4, 5, 5] {
            hub.tick();
            assert_eq!(hub.wait_time(Instant::now()), Duration::from_millis(delay));
        }
        request(&mut hub, 2, "wakeup", "status", json!({}));
        assert_eq!(hub.wait_time(Instant::now()), Duration::from_millis(1));
        std::fs::remove_dir_all(&hub.root).unwrap();
    }
    #[test]
    fn cleanup_is_acknowledged_after_durable_removal_and_failure_keeps_retry_state() {
        let (mut hub, mut output) = setup();
        let completed = Rc::new(Cell::new(0));
        struct CleanupFixture(Rc<Cell<usize>>);
        impl Provider for CleanupFixture {
            fn id(&self) -> &str {
                "codex"
            }
            fn capabilities(&self) -> Capabilities {
                Fixture.capabilities()
            }
            fn timeouts(&self) -> Timeouts {
                Fixture.timeouts()
            }
            fn supports_persistent_session(&self) -> bool {
                true
            }
            fn status(&self) -> Box<dyn Exchange> {
                Fixture.status()
            }
            fn send(&self, turn: Turn) -> Box<dyn Exchange> {
                Fixture.send(turn)
            }
            fn cleanup_sessions(&self, _: &[String]) -> Cleanup {
                let completed = self.0.clone();
                Cleanup::new(|| Ok(()), move || completed.set(completed.get() + 1))
            }
        }
        install_provider(
            &mut hub,
            "first",
            Box::new(CleanupFixture(completed.clone())),
        );
        fill_ledger(&mut hub, "first", 1);
        let token = hub.sessions.keys().next().unwrap().clone();
        let (release, wait) = mpsc::channel();
        hub.ledger = Ledger::with_writer(hub.sessions.clone(), move |_| {
            wait.recv().unwrap();
            Err(io::Error::other("injected removal failure"))
        })
        .unwrap();
        request(&mut hub, 1, "forget", "forget", json!({"sessions":[token]}));
        wait_until(&mut hub, |hub| !hub.ledger_jobs.is_empty());
        assert!(!hub.ledger_jobs.is_empty());
        assert!(hub.sessions.contains_key(&token));
        assert_eq!(completed.get(), 0);
        assert!(drain(&mut output[0]).is_empty());
        let mut ephemeral = turn(None);
        ephemeral["session"] = json!("ephemeral");
        request(&mut hub, 2, "other", "send", ephemeral);
        ticks(&mut hub, 6);
        assert_eq!(
            drain(&mut output[1]).last().unwrap()["event"]["type"],
            "completed"
        );
        release.send(()).unwrap();
        settle(&mut hub);
        assert_eq!(
            failure_reason(&drain(&mut output[0])).as_deref(),
            Some("CLEANUP_FAILED")
        );
        assert!(hub.sessions.contains_key(&token));
        assert_eq!(completed.get(), 0);
        hub.ledger = Ledger::new(hub.root.clone(), hub.sessions.clone()).unwrap();
        request(&mut hub, 1, "retry", "forget", json!({"sessions":[token]}));
        settle(&mut hub);
        assert_eq!(
            drain(&mut output[0]).last().unwrap()["event"]["type"],
            "completed"
        );
        assert!(!hub.sessions.contains_key(&token));
        assert_eq!(completed.get(), 1);
        let restarted: Sessions =
            serde_json::from_slice(&std::fs::read(hub.root.join("sessions.json")).unwrap())
                .unwrap();
        assert!(restarted.is_empty());
        std::fs::remove_dir_all(&hub.root).unwrap();
    }
    #[test]
    fn a_pending_revocation_removal_cannot_be_resumed_or_reacknowledged_after_reauthorization() {
        let (mut hub, mut output) = setup();
        fill_ledger(&mut hub, "first", 1);
        let token = hub.sessions.keys().next().unwrap().clone();
        let (release, wait) = mpsc::channel();
        let root = hub.root.clone();
        let mut first = true;
        hub.ledger = Ledger::with_writer(hub.sessions.clone(), move |sessions| {
            if first {
                first = false;
                wait.recv().unwrap();
            }
            config::write_private(&root.join("sessions.json"), &serde_json::to_vec(sessions)?)
        })
        .unwrap();
        let grant = config::app_path(&hub.root, "first").unwrap();
        let saved = std::fs::read(&grant).unwrap();
        std::fs::remove_file(&grant).unwrap();
        hub.prune_revoked_sessions();
        hub.prune_revoked_sessions();
        std::fs::write(&grant, saved).unwrap();
        request(&mut hub, 1, "resume", "send", turn(Some(&token)));
        ticks(&mut hub, 4);
        assert_eq!(
            failure_reason(&drain(&mut output[0])).as_deref(),
            Some("UNKNOWN_SESSION")
        );
        assert!(
            hub.session_token(0, "first", "codex", "native-first-0".into())
                .unwrap_or_else(|_| panic!("fresh reservation failed"))
                .is_none()
        );
        release.send(()).unwrap();
        settle(&mut hub);
        assert!(!hub.sessions.contains_key(&token));
        assert!(
            hub.sessions
                .token("first", "codex", "native-first-0")
                .is_some()
        );
        assert_eq!(hub.sessions.len(), 1);
        let restarted: Sessions =
            serde_json::from_slice(&std::fs::read(hub.root.join("sessions.json")).unwrap())
                .unwrap();
        assert_eq!(restarted.len(), 1);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }
}
