//! One owner of all provider adapters and exchanges. IO threads can submit
//! bounded commands but cannot choose namespaces, executables or environments.
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use seatline_core::exchange::{Timeouts, Update};
use seatline_core::protocol::ErrorCode;
use seatline_core::turn::{Namespace, SessionPolicy, ToolPolicy, Turn, is_cleanup_group};
use seatline_platform::layout::Layout;
use seatline_providers::{Cleanup, Provider, claude, codex, gemini, grok};
use seatline_scheduler::{EndReason, Event, Supervisor, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    PROTOCOL_VERSION,
    config::{self, Grant},
    wire,
};

const MAX_CONNECTIONS: usize = 32;
const MAX_QUEUE: usize = 64;
const MAX_APP_QUEUE: usize = 8;
const MAX_RUNNING: usize = 8;
const MAX_APP_RUNNING: usize = 2;
const MAX_PROVIDER_RUNNING: usize = 2;

pub enum Command {
    Open {
        connection: u64,
        grant: Grant,
        output: tokio::sync::mpsc::Sender<Value>,
    },
    Request {
        connection: u64,
        value: Value,
    },
    Close(u64),
}

struct Connection {
    grant: Grant,
    output: tokio::sync::mpsc::Sender<Value>,
}
struct Request {
    connection: u64,
    id: String,
    app: String,
    provider: String,
    method: String,
    params: Value,
}
struct Active {
    request: Request,
    persistent: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    app: String,
    provider: String,
    native: String,
}
struct PendingCleanup {
    request: Request,
    result: Receiver<io::Result<()>>,
    completed: Box<dyn FnOnce()>,
    sessions: Vec<String>,
}

pub fn start(root: PathBuf) -> io::Result<SyncSender<Command>> {
    let ledger = root.join("sessions.json");
    let sessions = match std::fs::read(&ledger) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(error),
    };
    let (send, receive) = mpsc::sync_channel(128);
    std::thread::spawn(move || {
        Hub {
            root,
            connections: BTreeMap::new(),
            queue: VecDeque::new(),
            providers: BTreeMap::new(),
            supervisor: Supervisor::new(),
            active: BTreeMap::new(),
            sessions,
            cleanup: Vec::new(),
            next_check: Instant::now(),
        }
        .run(receive);
    });
    Ok(send)
}

struct Hub {
    root: PathBuf,
    connections: BTreeMap<u64, Connection>,
    queue: VecDeque<Request>,
    providers: BTreeMap<(String, String), Box<dyn Provider>>,
    supervisor: Supervisor,
    active: BTreeMap<TurnId, Active>,
    sessions: BTreeMap<String, Session>,
    cleanup: Vec<PendingCleanup>,
    next_check: Instant,
}

impl Hub {
    fn run(mut self, input: Receiver<Command>) {
        loop {
            match input.recv_timeout(Duration::from_millis(5)) {
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
        self.supervisor.shutdown(Duration::from_secs(2));
        let until = Instant::now() + Duration::from_secs(4);
        while !self.supervisor.is_empty() && Instant::now() < until {
            self.supervisor.poll(Duration::from_millis(2));
        }
    }

    fn authorized(&self, connection: u64) -> bool {
        self.connections.get(&connection).is_some_and(|entry| {
            config::load_grant(&self.root, &entry.grant.app).is_ok_and(|current| {
                config::same_token(&current.token, &entry.grant.token)
                    && current.providers == entry.grant.providers
                    && current.allow_provider_default == entry.grant.allow_provider_default
            })
        })
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Open {
                connection,
                grant,
                output,
            } => {
                if self.connections.len() >= MAX_CONNECTIONS {
                    return;
                }
                self.connections
                    .insert(connection, Connection { grant, output });
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
            Command::Request { connection, value } => {
                if !self.authorized(connection) {
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
                    self.emit(connection, json!({"id":id,"event":wire::encode_update(&Update::Failed(wire::failure(ErrorCode::InvalidRequest,"APP_NOT_AUTHORIZED",false)))}));
                    return;
                };
                let request = Request {
                    connection,
                    id,
                    app: entry.grant.app.clone(),
                    provider: provider.to_owned(),
                    method: value["method"].as_str().unwrap_or("").to_owned(),
                    params: value["params"].clone(),
                };
                if self
                    .queue
                    .iter()
                    .chain(self.active.values().map(|active| &active.request))
                    .chain(self.cleanup.iter().map(|p| &p.request))
                    .any(|other| other.connection == connection && other.id == request.id)
                {
                    self.close(connection);
                    return;
                }
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
                            "QUEUE_FULL",
                            true,
                        )),
                    );
                } else {
                    self.queue.push_back(request);
                }
            }
        }
    }

    fn close(&mut self, connection: u64) {
        self.connections.remove(&connection);
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

    fn save_sessions(&self) -> io::Result<()> {
        config::write_private(
            &self.root.join("sessions.json"),
            &serde_json::to_vec(&self.sessions)?,
        )
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
                self.close(id);
            }
            self.next_check = Instant::now() + Duration::from_secs(1);
        }
        for event in self.supervisor.poll(Duration::from_millis(1)) {
            match event {
                Event::Update { turn_id, update } => {
                    let Some(active) = self.active.remove(&turn_id) else {
                        continue;
                    };
                    let update = match update {
                        Update::Session(native) if active.persistent => {
                            let token = self
                                .sessions
                                .iter()
                                .find(|(_, session)| {
                                    session.app == active.request.app
                                        && session.provider == active.request.provider
                                        && session.native == native
                                })
                                .map(|(token, _)| token.clone())
                                .map(Ok)
                                .unwrap_or_else(config::random_token);
                            match token.and_then(|token| {
                                if self.sessions.len() >= 10000 {
                                    return Err(io::Error::other("session limit"));
                                }
                                self.sessions.insert(
                                    token.clone(),
                                    Session {
                                        app: active.request.app.clone(),
                                        provider: active.request.provider.clone(),
                                        native,
                                    },
                                );
                                self.save_sessions()?;
                                Ok(token)
                            }) {
                                Ok(token) => Update::Session(token),
                                Err(_) => {
                                    self.supervisor.cancel(turn_id);
                                    Update::Failed(wire::failure(
                                        ErrorCode::InternalError,
                                        "SESSION_STORE_FAILED",
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
                    self.event(&active.request, update);
                    self.active.insert(turn_id, active);
                }
                Event::Ended { turn_id, reason } => {
                    if let Some(active) = self.active.remove(&turn_id) {
                        self.event(
                            &active.request,
                            match reason {
                                EndReason::Completed => Update::Completed,
                                EndReason::Cancelled => Update::Stopped,
                                EndReason::Failed(error) => Update::Failed(error),
                                EndReason::Timeout(_) => Update::Failed(wire::failure(
                                    ErrorCode::ProviderFailed,
                                    "PROVIDER_TIMEOUT",
                                    true,
                                )),
                                _ => Update::Failed(wire::failure(
                                    ErrorCode::InternalError,
                                    "PROVIDER_FAILED",
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
                    let pending = self.cleanup.swap_remove(index);
                    let result = result.and_then(|()| {
                        (pending.completed)();
                        for token in pending.sessions {
                            self.sessions.remove(&token);
                        }
                        self.save_sessions()
                    });
                    self.event(
                        &pending.request,
                        if result.is_ok() {
                            Update::Completed
                        } else {
                            Update::Failed(wire::failure(
                                ErrorCode::InternalError,
                                "CLEANUP_FAILED",
                                true,
                            ))
                        },
                    );
                }
                Err(mpsc::TryRecvError::Empty) => index += 1,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let pending = self.cleanup.swap_remove(index);
                    self.event(
                        &pending.request,
                        Update::Failed(wire::failure(
                            ErrorCode::InternalError,
                            "CLEANUP_FAILED",
                            true,
                        )),
                    );
                }
            }
        }
        for _ in 0..self.queue.len() {
            let Some(request) = self.queue.pop_front() else {
                break;
            };
            let app_count = self
                .active
                .values()
                .filter(|a| a.request.app == request.app)
                .count()
                + self
                    .cleanup
                    .iter()
                    .filter(|c| c.request.app == request.app)
                    .count();
            let provider_count = self
                .active
                .values()
                .filter(|a| a.request.provider == request.provider)
                .count()
                + self
                    .cleanup
                    .iter()
                    .filter(|c| c.request.provider == request.provider)
                    .count();
            let cleaning = self
                .cleanup
                .iter()
                .any(|c| c.request.app == request.app && c.request.provider == request.provider);
            let busy = self
                .active
                .values()
                .any(|a| a.request.app == request.app && a.request.provider == request.provider);
            if self.active.len() + self.cleanup.len() >= MAX_RUNNING
                || app_count >= MAX_APP_RUNNING
                || provider_count >= MAX_PROVIDER_RUNNING
                || cleaning
                || (matches!(request.method.as_str(), "forget" | "cleanup") && busy)
                || (request.method == "send"
                    && request.params["continuation"].is_string()
                    && self.active.values().any(|a| {
                        a.request.app == request.app
                            && a.request.params["continuation"] == request.params["continuation"]
                    }))
            {
                self.queue.push_back(request);
            } else {
                self.admit(request);
            }
        }
    }

    #[allow(clippy::map_entry)] // Admission errors also need mutable access to the connection table.
    fn admit(&mut self, request: Request) {
        let key = (request.app.clone(), request.provider.clone());
        if !self.providers.contains_key(&key) {
            let Ok(namespace) = Namespace::fixed(&request.app) else {
                return;
            };
            let layout = match self.connections[&request.connection].grant.cache_title.as_deref() {
                Some(title) => match Layout::with_cache_title(namespace,title) {
                    Ok(layout) => layout,
                    Err(_) => { self.event(&request,Update::Failed(wire::failure(ErrorCode::InvalidRequest,"INVALID_REQUEST",false))); return; }
                },
                None => Layout::new(namespace),
            };
            let provider: Box<dyn Provider> = match request.provider.as_str() {
                "codex" => Box::new(codex::Codex::installed(&layout)),
                "claude" => Box::new(claude::Claude::installed(&layout)),
                "gemini" => Box::new(gemini::Gemini::installed(&layout)),
                "grok" => Box::new(grok::Grok::installed(&layout)),
                _ => {
                    self.event(
                        &request,
                        Update::Failed(wire::failure(
                            ErrorCode::ProviderNotFound,
                            "EXECUTABLE_NOT_FOUND",
                            false,
                        )),
                    );
                    return;
                }
            };
            self.providers.insert(key.clone(), provider);
        }
        let result = catch_unwind(AssertUnwindSafe(|| self.build(&request, &key)));
        match result {
            Ok(Ok(Built::Exchange(exchange, timeouts, persistent))) => {
                let turn = self
                    .supervisor
                    .start(exchange, Some(timeouts), Duration::from_secs(2));
                self.active.insert(
                    turn,
                    Active {
                        request,
                        persistent,
                    },
                );
            }
            Ok(Ok(Built::Cleanup(cleanup, sessions))) => {
                let (send, result) = mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let _ = send.send((cleanup.work)());
                });
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
                    "PROVIDER_FAILED",
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
        let invalid = || wire::failure(ErrorCode::InvalidRequest, "INVALID_REQUEST", false);
        let provider = self.providers[key].as_ref();
        let limits = Timeouts {
            max_turn: Duration::from_secs(15 * 60).min(provider.timeouts().max_turn),
            ..provider.timeouts()
        };
        match request.method.as_str() {
            "status" => Ok(Built::Exchange(
                provider.status(),
                Timeouts {
                    start: Duration::from_secs(30),
                    idle: Duration::from_secs(30),
                    max_turn: Duration::from_secs(30),
                    stop_grace: Duration::from_secs(2),
                },
                false,
            )),
            "send" => {
                let mut turn: Turn =
                    serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
                turn.validate().map_err(|_| invalid())?;
                if turn.tools == ToolPolicy::ProviderDefault
                    && !self.connections[&request.connection]
                        .grant
                        .allow_provider_default
                {
                    return Err(invalid());
                }
                if turn.session == SessionPolicy::Persistent
                    && !provider.supports_persistent_session()
                {
                    return Err(wire::failure(
                        ErrorCode::InvalidRequest,
                        "PERSISTENT_SESSION_UNSUPPORTED",
                        false,
                    ));
                }
                if let Some(token) = turn.continuation.as_ref() {
                    let session = self
                        .sessions
                        .get(token)
                        .filter(|session| {
                            session.app == request.app && session.provider == request.provider
                        })
                        .ok_or_else(|| {
                            wire::failure(ErrorCode::InvalidRequest, "UNKNOWN_SESSION", false)
                        })?;
                    turn.continuation = Some(session.native.clone());
                }
                let persistent = turn.session == SessionPolicy::Persistent;
                Ok(Built::Exchange(provider.send(turn), limits, persistent))
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
