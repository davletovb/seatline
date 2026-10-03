//! One owner of all provider adapters and exchanges. IO threads can submit
//! bounded commands but cannot choose namespaces, executables or environments.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
/// Persistent sessions kept across all apps, and by any one app, so a single
/// app cannot use up the ledger the others depend on.
const MAX_SESSIONS: usize = 10_000;
const MAX_APP_SESSIONS: usize = 2_000;
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

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
    /// A local failure can end the client-visible request before the
    /// supervisor finishes stopping/reaping its exchange. Keep its slot until
    /// Ended, but release its request ID and never forward more output after
    /// this boundary.
    terminal_sent: bool,
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
            next_prune: Instant::now() + PRUNE_INTERVAL,
            missing_grants: BTreeSet::new(),
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
    next_prune: Instant,
    /// Apps whose grant was missing at the previous sweep. Only apps missing at
    /// two sweeps in a row lose their sessions, so a grant that is being
    /// rewritten is never mistaken for a revoked one.
    missing_grants: BTreeSet<String>,
}

enum LedgerError {
    Full,
    Storage,
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
                    self.emit(connection, json!({"id":id,"event":wire::encode_update(&Update::Failed(wire::failure(ErrorCode::InvalidRequest,wire::reason::APP_NOT_AUTHORIZED,false)))}));
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
                    .chain(
                        self.active
                            .values()
                            .filter(|active| !active.terminal_sent)
                            .map(|active| &active.request),
                    )
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
                            wire::reason::QUEUE_FULL,
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

    /// The broker token for a provider-native session, creating it on first use.
    /// A session that already has a token costs nothing: no cap check, no write.
    fn session_token(
        &mut self,
        app: &str,
        provider: &str,
        native: String,
    ) -> Result<String, LedgerError> {
        if let Some((token, _)) = self.sessions.iter().find(|(_, session)| {
            session.app == app && session.provider == provider && session.native == native
        }) {
            return Ok(token.clone());
        }
        let app_sessions = self.sessions.values().filter(|s| s.app == app).count();
        if self.sessions.len() >= MAX_SESSIONS || app_sessions >= MAX_APP_SESSIONS {
            return Err(LedgerError::Full);
        }
        let token = config::random_token().map_err(|_| LedgerError::Storage)?;
        self.sessions.insert(
            token.clone(),
            Session {
                app: app.to_owned(),
                provider: provider.to_owned(),
                native,
            },
        );
        if self.save_sessions().is_err() {
            self.sessions.remove(&token);
            return Err(LedgerError::Storage);
        }
        Ok(token)
    }

    /// Drops the sessions of apps whose grant no longer exists, so revoking an
    /// app also frees its share of the ledger and its tokens stop being usable.
    fn prune_revoked_sessions(&mut self) {
        let apps: BTreeSet<String> = self.sessions.values().map(|s| s.app.clone()).collect();
        let missing: BTreeSet<String> = apps
            .into_iter()
            .filter(|app| {
                config::app_path(&self.root, app).is_ok_and(|path| {
                    matches!(std::fs::symlink_metadata(path),
                        Err(error) if error.kind() == io::ErrorKind::NotFound)
                })
            })
            .collect();
        let revoked: Vec<String> = missing
            .iter()
            .filter(|app| self.missing_grants.contains(*app))
            .cloned()
            .collect();
        self.missing_grants = missing;
        if revoked.is_empty() {
            return;
        }
        let removed: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, session)| revoked.contains(&session.app))
            .map(|(token, _)| token.clone())
            .collect();
        let removed: Vec<_> = removed
            .into_iter()
            .filter_map(|token| self.sessions.remove(&token).map(|session| (token, session)))
            .collect();
        if self.save_sessions().is_err() {
            // Keep them in memory too, and try again at the next sweep.
            self.sessions.extend(removed);
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
        if Instant::now() >= self.next_prune {
            self.prune_revoked_sessions();
            self.next_prune = Instant::now() + PRUNE_INTERVAL;
        }
        for event in self.supervisor.poll(Duration::from_millis(1)) {
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
                            match self.session_token(&app, &provider, native) {
                                Ok(token) => Update::Session(token),
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
                    active.terminal_sent = update.is_terminal();
                    self.event(&active.request, update);
                    self.active.insert(turn_id, active);
                }
                Event::Ended { turn_id, reason } => {
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
                    let pending = self.cleanup.swap_remove(index);
                    let result = result.and_then(|()| {
                        (pending.completed)();
                        let removed: Vec<_> = pending
                            .sessions
                            .into_iter()
                            .filter_map(|token| {
                                self.sessions.remove(&token).map(|session| (token, session))
                            })
                            .collect();
                        let saved = self.save_sessions();
                        if saved.is_err() {
                            self.sessions.extend(removed);
                        }
                        saved
                    });
                    self.event(
                        &pending.request,
                        if result.is_ok() {
                            Update::Completed
                        } else {
                            Update::Failed(wire::failure(
                                ErrorCode::InternalError,
                                wire::reason::CLEANUP_FAILED,
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
                            wire::reason::CLEANUP_FAILED,
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
            let layout = match self.connections[&request.connection]
                .grant
                .cache_title
                .as_deref()
            {
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
                "claude" => Box::new(claude::Claude::installed(&layout)),
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
                        terminal_sent: false,
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
                            session.app == request.app && session.provider == request.provider
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

#[cfg(test)]
mod tests {
    use super::*;
    use seatline_core::exchange::{Exchange, Scripted};
    use seatline_core::protocol::Capabilities;

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
    fn setup() -> (Hub, Vec<tokio::sync::mpsc::Receiver<Value>>) {
        let root =
            std::env::temp_dir().join(format!("seatline-hub-{}", config::random_token().unwrap()));
        let mut hub = Hub {
            root,
            connections: BTreeMap::new(),
            queue: VecDeque::new(),
            providers: BTreeMap::new(),
            supervisor: Supervisor::new(),
            active: BTreeMap::new(),
            sessions: BTreeMap::new(),
            cleanup: Vec::new(),
            next_check: Instant::now() + Duration::from_secs(60),
            next_prune: Instant::now() + Duration::from_secs(3600),
            missing_grants: BTreeSet::new(),
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
            hub.providers
                .insert((app.into(), "codex".into()), Box::new(Fixture));
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
        for _ in 0..5 {
            hub.tick();
        }
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
        for _ in 0..5 {
            hub.tick();
        }
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
        for _ in 0..5 {
            hub.tick();
        }
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
        for _ in 0..8 {
            hub.tick();
        }
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
        request(
            &mut hub,
            3,
            "allowed",
            "send",
            turn_with_tools("provider_default"),
        );
        for _ in 0..8 {
            hub.tick();
        }
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
            for _ in 0..8 {
                hub.tick();
            }
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
                    },
                    persistent: true,
                    terminal_sent: false,
                },
            );
            hub.tick();
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
            for _ in 0..8 {
                hub.tick();
            }
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
            for _ in 0..8 {
                hub.tick();
            }
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
            hub.session_token("first", "codex", "brand-new".into()),
            Err(LedgerError::Full)
        ));
        // A session that already has a token keeps working at the cap, without a rewrite.
        assert!(
            hub.session_token("first", "codex", "native-first-7".into())
                .is_ok()
        );
        // Other apps still have room.
        let token = hub
            .session_token("second", "codex", "another".into())
            .unwrap_or_else(|_| panic!("second app was locked out"));
        assert_eq!(hub.sessions[&token].app, "second");
        // The global cap still applies.
        let room = MAX_SESSIONS - hub.sessions.len();
        fill_ledger(&mut hub, "third", room);
        assert!(matches!(
            hub.session_token("second", "codex", "over-the-cap".into()),
            Err(LedgerError::Full)
        ));
        std::fs::remove_dir_all(&hub.root).unwrap();
    }

    #[test]
    fn revoking_an_app_frees_its_sessions_after_two_sweeps() {
        let (mut hub, _output) = setup();
        fill_ledger(&mut hub, "first", 3);
        fill_ledger(&mut hub, "second", 2);
        hub.save_sessions().unwrap();
        std::fs::remove_file(config::app_path(&hub.root, "first").unwrap()).unwrap();
        hub.prune_revoked_sessions();
        assert_eq!(hub.sessions.len(), 5, "one sweep is not enough");
        hub.prune_revoked_sessions();
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
        assert_eq!(hub.sessions.len(), 2);
        std::fs::remove_dir_all(&hub.root).unwrap();
    }
}
