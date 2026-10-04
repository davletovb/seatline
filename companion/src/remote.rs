//! The reusable remote client (D-01): one application's long-lived connection
//! to the shared broker, used for every request the application makes.
//!
//! [`client::RemoteProvider`]'s own exchanges each start a thread and a
//! runtime, connect and authenticate for one request. A [`RemoteClient`] does
//! that once:
//!
//! - **One runtime, one connection.** A single thread runs the client's IPC.
//!   It connects when the first request needs it (starting the companion if
//!   none is running, as [`client::connect`] does), authenticates once, and
//!   keeps the connection for the requests that follow.
//! - **Unique request IDs.** Each request carries an ID no other request on the
//!   client has used, so the broker's duplicate-ID rule, which closes a
//!   connection, can never be met by the client.
//! - **Routing.** Events are delivered to the request they belong to, in
//!   order, and a cancel names one request: it stops that request and no other.
//! - **Bounded.** The client holds at most [`Limits::max_in_flight`] requests;
//!   a further one fails at once with `QUEUE_FULL`. Each request queues at most
//!   [`Limits::max_unread_bytes`] of unread events, under the slow-consumer
//!   policy of [`seatline_core::backlog`]: the connection is shared, so the
//!   client never waits for one request's consumer, which would stall the
//!   others and have the broker close the connection for not reading. A
//!   consumer that falls too far behind has its own request cancelled, receives
//!   what was queued whole and in order, then one `CONSUMER_TOO_SLOW` failure;
//!   the other requests are unaffected.
//!
//! # Failures and replays
//!
//! A request ends exactly once. When the connection is lost, every request in
//! flight on it ends as `COMPANION_DISCONNECTED`, and **none is replayed**: the
//! broker may already have started a generation for it, so repeating it is the
//! application's decision. The next request connects again, reading the app's
//! grant afresh, so a rotated grant costs the requests in flight at that moment
//! one failure each (the broker closes a connection whose grant changed) and
//! nothing after. A revoked grant likewise ends the connection, and new requests
//! then fail with `APP_NOT_AUTHORIZED`; the broker's other connections, and the
//! client of any other app, are unaffected. A request that was only waiting for
//! the connection when it failed was never sent.
//!
//! Dropping every handle to the client, and every exchange it made, closes the
//! connection and ends its thread; the broker cancels what was still running
//! for it. The connection otherwise stays open for as long as the client does,
//! which keeps the broker running.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use seatline_core::backlog::{self, Admission, Backlog};
use seatline_core::exchange::{Exchange, Scripted, Update};
use seatline_core::protocol::ErrorCode;
use seatline_core::turn::Turn;
use serde_json::{Value, json};
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task::{AbortHandle, JoinHandle};

use crate::{PROTOCOL_VERSION, client, config, wire};

/// The most requests one client keeps in flight by default. The broker queues
/// far fewer for one app (see its `max_app_queue`); the rest are refused there
/// with `QUEUE_FULL`, so this only keeps the client's own table bounded.
pub const MAX_IN_FLIGHT: usize = 64;

/// How long the broker may take to answer the authentication, or to take a
/// frame, before the connection is given up on.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Frames waiting for the connection's reader or writer task.
const FRAMES: usize = 256;

/// What the client holds back, in memory and in requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How much output one request may have queued and unread before the next
    /// event cancels it; see [`seatline_core::backlog`].
    pub max_unread_bytes: usize,
    /// How many requests may be in flight on the client at once.
    pub max_in_flight: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_unread_bytes: backlog::MAX_UNREAD_BYTES,
            max_in_flight: MAX_IN_FLIGHT,
        }
    }
}

/// One application's connection to the broker. Cheap to clone; every clone
/// shares the connection, the request IDs and the limits.
#[derive(Clone)]
pub struct RemoteClient {
    shared: Arc<Shared>,
}

struct Shared {
    commands: tokio_mpsc::UnboundedSender<Command>,
    next_id: AtomicU64,
    in_flight: Arc<AtomicUsize>,
    limits: Limits,
}

impl RemoteClient {
    /// A client for `app`, using the Seatline data directory the environment
    /// names (`SEATLINE_DATA_DIR`, or the user's default). Nothing connects yet.
    pub fn new(app: &str) -> Self {
        Self::start(app, None, Limits::default())
    }

    /// A client for `app` whose grant and broker live under `root`.
    pub fn with_root(app: &str, root: PathBuf) -> Self {
        Self::start(app, Some(root), Limits::default())
    }

    /// A client with `limits` in place of the defaults. `root` is the data
    /// directory, or `None` for the one the environment names.
    pub fn start(app: &str, root: Option<PathBuf>, limits: Limits) -> Self {
        let (commands, receiver) = tokio_mpsc::unbounded_channel();
        let worker = Worker {
            app: app.to_owned(),
            root,
            limits,
            routes: HashMap::new(),
            waiting: VecDeque::new(),
            link: Link::Down,
            closed: false,
        };
        // The thread outlives every handle that can reach it: it ends when
        // the last sender is dropped.
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(worker.run(receiver));
            }
        });
        Self {
            shared: Arc::new(Shared {
                commands,
                next_id: AtomicU64::new(1),
                in_flight: Arc::new(AtomicUsize::new(0)),
                limits,
            }),
        }
    }

    /// `method` for `provider` with `params`, as one request on the shared
    /// connection. The returned exchange ends exactly once.
    pub fn request(&self, method: &str, provider: &str, params: Value) -> Box<dyn Exchange> {
        self.request_with_scheduling(
            method,
            provider,
            params,
            crate::scheduling::Hints::default(),
        )
    }

    /// A request with bounded scheduling hints and opt-in queued/admitted events.
    pub fn request_with_scheduling(
        &self,
        method: &str,
        provider: &str,
        params: Value,
        scheduling: crate::scheduling::Hints,
    ) -> Box<dyn Exchange> {
        let id = format!("r{}", self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        let frame = json!({"id": id, "method": method, "provider": provider, "params": params, "scheduling": scheduling});
        // A frame the broker cannot read ends the connection, so one that
        // cannot be sent is refused before it gets near it.
        if serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() > wire::MAX_FRAME) {
            return refused(
                ErrorCode::InvalidRequest,
                wire::reason::INVALID_REQUEST,
                false,
            );
        }
        let slot = Slot::take(&self.shared.in_flight, self.shared.limits.max_in_flight);
        let Some(slot) = slot else {
            return refused(ErrorCode::ProviderFailed, wire::reason::QUEUE_FULL, true);
        };
        let (events, updates) = mpsc::channel();
        let backlog = Arc::new(Backlog::default());
        let route = Route {
            events,
            backlog: Arc::clone(&backlog),
            frame: Some(frame),
            overflowed: false,
            cancelled: false,
        };
        let start = Command::Start {
            id: id.clone(),
            route,
        };
        if self.shared.commands.send(start).is_err() {
            return refused(
                ErrorCode::ProviderFailed,
                wire::reason::COMPANION_DISCONNECTED,
                true,
            );
        }
        Box::new(Remote {
            id,
            updates,
            backlog,
            commands: self.shared.commands.clone(),
            slot: Some(slot),
            ended: false,
        })
    }

    /// The readiness of `provider`.
    pub fn status(&self, provider: &str) -> Box<dyn Exchange> {
        self.request("status", provider, Value::Null)
    }

    /// A turn on `provider`.
    pub fn send(&self, provider: &str, turn: &Turn) -> Box<dyn Exchange> {
        self.request("send", provider, json!(turn))
    }

    /// Forgets the native sessions `sessions` on `provider`.
    pub fn forget(&self, provider: &str, sessions: &[String]) -> Box<dyn Exchange> {
        self.request("forget", provider, json!({"sessions": sessions}))
    }

    /// Cleans up what the turns of `group` left on `provider`.
    pub fn cleanup(&self, provider: &str, group: &str) -> Box<dyn Exchange> {
        self.request("cleanup", provider, json!({"group": group}))
    }

    /// Ends every request in flight as stopped and closes the connection. A
    /// request made afterwards fails at once.
    pub fn close(&self) {
        let _ = self.shared.commands.send(Command::Close);
    }
}

/// An exchange that ends at once, as the request was refused before it ran.
fn refused(code: ErrorCode, reason: &'static str, retryable: bool) -> Box<dyn Exchange> {
    Box::new(Scripted::failed(wire::failure(code, reason, retryable)))
}

fn disconnected() -> Update {
    Update::Failed(wire::failure(
        ErrorCode::ProviderFailed,
        wire::reason::COMPANION_DISCONNECTED,
        true,
    ))
}

/// One of the client's places for a request in flight, freed when the request
/// ends or its exchange is dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(in_flight: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        if in_flight.fetch_add(1, Ordering::SeqCst) >= max {
            in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(in_flight)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a request's exchange sends the client's thread.
enum Command {
    Start {
        id: String,
        route: Route,
    },
    /// The consumer asked the request to stop; it still ends with its terminal event.
    Cancel(String),
    /// The consumer dropped the exchange: stop the request and forget it.
    Abandon(String),
    Close,
}

/// The client thread's side of one request.
struct Route {
    events: mpsc::Sender<Update>,
    backlog: Arc<Backlog>,
    /// The request frame, until it has been handed to the connection.
    frame: Option<Value>,
    /// The consumer fell too far behind: what the broker still sends is discarded.
    overflowed: bool,
    /// The consumer asked for the request to stop.
    cancelled: bool,
}

/// A request, as its consumer holds it.
struct Remote {
    id: String,
    updates: mpsc::Receiver<Update>,
    backlog: Arc<Backlog>,
    commands: tokio_mpsc::UnboundedSender<Command>,
    /// The request's place among those in flight, until it ends or the
    /// exchange is dropped, whichever is first: a finished exchange that its
    /// consumer keeps must not count against the client's limit.
    slot: Option<Slot>,
    ended: bool,
}

impl Exchange for Remote {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if self.ended {
            return None;
        }
        match self
            .updates
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(update) => {
                self.backlog.read(&update);
                if update.is_terminal() {
                    self.ended = true;
                    self.slot = None;
                }
                Some(update)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            // The client's thread is gone without having ended the request.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.ended = true;
                self.slot = None;
                Some(disconnected())
            }
        }
    }

    fn cancel(&mut self, _grace: Duration) {
        let _ = self.commands.send(Command::Cancel(self.id.clone()));
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.commands.send(Command::Abandon(self.id.clone()));
        }
    }
}

/// Why a connection could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The app has no grant, or the broker would not have it.
    NotAuthorized,
    /// The broker has all the connections it serves.
    Busy,
    /// No broker could be reached, or it hung up.
    Unreachable,
}

impl Refusal {
    fn update(self) -> Update {
        match self {
            Self::NotAuthorized => Update::Failed(wire::failure(
                ErrorCode::InvalidRequest,
                wire::reason::APP_NOT_AUTHORIZED,
                false,
            )),
            Self::Busy => Update::Failed(wire::failure(
                ErrorCode::ProviderFailed,
                wire::reason::QUEUE_FULL,
                true,
            )),
            Self::Unreachable => disconnected(),
        }
    }
}

/// An open, authenticated connection, and the tasks that serve it.
struct Conn {
    writer: tokio_mpsc::Sender<Value>,
    frames: tokio_mpsc::Receiver<io::Result<Value>>,
    tasks: [AbortHandle; 2],
}

impl Drop for Conn {
    /// Closing the connection is dropping its halves, which the tasks own.
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

enum Link {
    Down,
    Connecting(JoinHandle<Result<Conn, Refusal>>),
    Up(Conn),
}

/// What the worker's loop learned in one turn.
enum Step {
    Command(Option<Command>),
    Frame(Option<io::Result<Value>>),
    Connected(Result<Result<Conn, Refusal>, tokio::task::JoinError>),
}

struct Worker {
    app: String,
    root: Option<PathBuf>,
    limits: Limits,
    routes: HashMap<String, Route>,
    /// The requests that have not been handed to the connection yet, in the
    /// order they were made: the broker serves an app's requests in the order
    /// it receives them.
    waiting: VecDeque<String>,
    link: Link,
    closed: bool,
}

impl Worker {
    async fn run(mut self, mut commands: tokio_mpsc::UnboundedReceiver<Command>) {
        loop {
            let step = match &mut self.link {
                Link::Down => Step::Command(commands.recv().await),
                Link::Connecting(attempt) => tokio::select! {
                    command = commands.recv() => Step::Command(command),
                    connected = attempt => Step::Connected(connected),
                },
                Link::Up(conn) => tokio::select! {
                    command = commands.recv() => Step::Command(command),
                    frame = conn.frames.recv() => Step::Frame(frame),
                },
            };
            match step {
                // Every handle is gone: dropping the worker closes the connection.
                Step::Command(None) => return,
                Step::Command(Some(command)) => self.command(command),
                Step::Frame(Some(Ok(frame))) => self.frame(&frame),
                Step::Frame(Some(Err(_)) | None) => self.lose_connection(),
                Step::Connected(Ok(Ok(conn))) => self.connected(conn),
                Step::Connected(Ok(Err(refusal))) => self.refuse_waiting(refusal),
                Step::Connected(Err(_)) => self.refuse_waiting(Refusal::Unreachable),
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Start { id, route } => {
                if self.closed {
                    let _ = route.events.send(disconnected());
                    return;
                }
                self.waiting.push_back(id.clone());
                self.routes.insert(id, route);
                match self.link {
                    Link::Up(_) => self.flush(),
                    Link::Down => self.connect(),
                    Link::Connecting(_) => {}
                }
            }
            Command::Cancel(id) => self.cancel(&id, false),
            Command::Abandon(id) => self.cancel(&id, true),
            Command::Close => {
                self.closed = true;
                if let Link::Connecting(attempt) = &self.link {
                    attempt.abort();
                }
                self.link = Link::Down;
                self.waiting.clear();
                for (_, route) in self.routes.drain() {
                    let _ = route.events.send(Update::Stopped);
                }
            }
        }
    }

    /// Stops one request. One that never left for the broker is simply ended.
    fn cancel(&mut self, id: &str, forget: bool) {
        let Some(route) = self.routes.get_mut(id) else {
            return;
        };
        if route.frame.is_some() {
            // Still waiting for the connection: the broker never heard of it.
            self.waiting.retain(|waiting| waiting != id);
            if let Some(route) = self.routes.remove(id) {
                let _ = route.events.send(Update::Stopped);
            }
            return;
        }
        // A cancel the consumer asked for, before anything else ended the
        // request, is why it ends.
        route.cancelled |= !route.overflowed;
        self.ask_to_cancel(id);
        if forget {
            self.routes.remove(id);
        }
    }

    fn ask_to_cancel(&mut self, id: &str) {
        self.write(json!({"id": "cancel", "method": "cancel", "target": id}));
    }

    /// Hands a frame to the connection. One it cannot take means the
    /// connection is not being served, and is given up.
    fn write(&mut self, frame: Value) {
        let accepted = match &self.link {
            Link::Up(conn) => conn.writer.try_send(frame).is_ok(),
            _ => false,
        };
        if !accepted {
            self.lose_connection();
        }
    }

    /// Sends the requests that were waiting for a connection, in order.
    fn flush(&mut self) {
        while let Some(id) = self.waiting.pop_front() {
            if let Some(frame) = self
                .routes
                .get_mut(&id)
                .and_then(|route| route.frame.take())
            {
                self.write(frame);
            }
        }
    }

    fn connect(&mut self) {
        let (app, root) = (self.app.clone(), self.root.clone());
        self.link = Link::Connecting(tokio::spawn(establish(app, root)));
    }

    fn connected(&mut self, conn: Conn) {
        self.link = Link::Up(conn);
        self.flush();
    }

    /// The connection could not be made: the requests that waited for it end
    /// as the refusal says, and the next request tries again.
    fn refuse_waiting(&mut self, refusal: Refusal) {
        self.link = Link::Down;
        self.waiting.clear();
        for (_, route) in self.routes.drain() {
            let _ = route.events.send(refusal.update());
        }
    }

    /// The connection was lost, so every request on it ends, and none is
    /// replayed: the broker may have started a generation for it already. One
    /// whose consumer asked it to stop has stopped.
    fn lose_connection(&mut self) {
        self.link = Link::Down;
        self.waiting.clear();
        for (_, route) in self.routes.drain() {
            let _ = route.events.send(if route.cancelled {
                Update::Stopped
            } else {
                disconnected()
            });
        }
    }

    /// Routes one event from the broker to its request.
    fn frame(&mut self, frame: &Value) {
        let Some(id) = frame["id"].as_str() else {
            return;
        };
        let Some(route) = self.routes.get_mut(id) else {
            // A request already ended, or dropped: its late events are not wanted.
            return;
        };
        let Ok(update) = wire::decode_update(frame["event"].clone()) else {
            // The broker said something this client cannot read: not a
            // connection to keep serving requests on.
            self.lose_connection();
            return;
        };
        let id = id.to_owned();
        if update.is_terminal() {
            if let Some(route) = self.routes.remove(&id) {
                let _ = route.events.send(update);
            }
            return;
        }
        if route.overflowed {
            return;
        }
        match route.backlog.admit(&update, self.limits.max_unread_bytes) {
            Admission::Skip => {}
            Admission::Queue => {
                if route.events.send(update).is_err() {
                    // The consumer is gone: stop what it asked for.
                    self.cancel(&id, true);
                }
            }
            Admission::Overflow => {
                route.overflowed = true;
                if route.cancelled {
                    // The consumer asked for the stop: the broker's
                    // terminal event, when it comes, is the ending.
                    return;
                }
                // The consumer cannot keep up. Its request is cancelled and
                // ended here and now, with what it was given intact; what
                // the broker still sends for it finds no request.
                if let Some(route) = self.routes.remove(&id) {
                    let _ = route.events.send(Update::Failed(backlog::too_slow()));
                }
                self.ask_to_cancel(&id);
            }
        }
    }
}

/// Connects, authenticates and starts the tasks that read and write the
/// connection.
async fn establish(app: String, root: Option<PathBuf>) -> Result<Conn, Refusal> {
    let root = match root {
        Some(root) => root,
        None => config::data_dir().map_err(|_| Refusal::NotAuthorized)?,
    };
    // Read afresh for each connection, so a rotated grant is picked up by the
    // next one.
    let grant = config::load_grant(&root, &app).map_err(|_| Refusal::NotAuthorized)?;
    let mut stream = client::connect(&root)
        .await
        .map_err(|_| Refusal::Unreachable)?;
    let hello = tokio::time::timeout(IO_TIMEOUT, async {
        wire::write_frame(
            &mut stream,
            &json!({"version": PROTOCOL_VERSION, "app": grant.app, "token": grant.token}),
        )
        .await?;
        wire::read_frame(&mut stream).await
    })
    .await
    .map_err(|_| Refusal::Unreachable)?
    .map_err(|_| Refusal::Unreachable)?;
    match hello["type"].as_str() {
        Some("ready") => {}
        Some("busy") => return Err(Refusal::Busy),
        _ => return Err(Refusal::NotAuthorized),
    }
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (frames_in, frames) = tokio_mpsc::channel(FRAMES);
    let (writer_in, mut to_write) = tokio_mpsc::channel::<Value>(FRAMES);
    let failed = frames_in.clone();
    let read = tokio::spawn(async move {
        loop {
            let frame = wire::read_frame(&mut reader).await;
            let ended = frame.is_err();
            if frames_in.send(frame).await.is_err() || ended {
                return;
            }
        }
    });
    let write = tokio::spawn(async move {
        while let Some(frame) = to_write.recv().await {
            let written = tokio::time::timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &frame))
                .await
                .map_err(|_| io::Error::other("the broker stopped taking frames"))
                .and_then(|result| result);
            if let Err(error) = written {
                let _ = failed.send(Err(error)).await;
                return;
            }
        }
    });
    Ok(Conn {
        writer: writer_in,
        frames,
        tasks: [read.abort_handle(), write.abort_handle()],
    })
}
