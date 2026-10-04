//! The reusable remote client (D-01) against a scripted broker: requests are
//! routed to their own exchange over one connection, a cancel stops one
//! request, a slow consumer is stopped alone, a lost connection ends what was
//! in flight without replaying it and the next request reconnects, a changed
//! grant costs only the requests in flight, and nothing one app does reaches
//! another.
//!
//! The broker here speaks the real wire protocol and applies the real hub's
//! rules that matter to a client (it authenticates against the grant file,
//! re-checks it on every frame and closes the connection if it changed), but
//! runs nothing: every reply is the test's. No test asserts a time threshold.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interprocess::local_socket::{ListenerOptions, tokio::prelude::*};
use seatline_companion::client::RemoteProvider;
use seatline_companion::remote::{Limits, RemoteClient};
use seatline_companion::{client, config, wire};
use seatline_core::discovery::SearchPath;
use seatline_core::exchange::{Exchange, Update};
use seatline_providers::{Provider, codex::Codex};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, split};
use tokio::sync::{Notify, mpsc, oneshot};

/// What a handler is given about the connection a frame came in on.
struct Ctx {
    out: mpsc::UnboundedSender<Value>,
}

type Handler = Arc<dyn Fn(&Ctx, &Value) + Send + Sync>;

struct Connection {
    out: mpsc::UnboundedSender<Value>,
    hangup: Arc<Notify>,
}

struct FakeBroker {
    root: PathBuf,
    /// Every frame accepted after a handshake, with its connection's number.
    seen: Arc<Mutex<Vec<(usize, Value)>>>,
    /// The handshakes completed.
    connections: Arc<AtomicUsize>,
    /// The connections that ended.
    closed: Arc<AtomicUsize>,
    open: Arc<Mutex<Vec<Connection>>>,
    /// Answer the next handshakes with `busy` instead of `ready`.
    busy: Arc<AtomicBool>,
    /// How long, in milliseconds, a handshake takes.
    delay: Arc<AtomicUsize>,
    stop: Option<oneshot::Sender<()>>,
}

fn event(id: &str, event: Value) -> Value {
    json!({"id": id, "event": event})
}

fn delta(id: &str, text: &str) -> Value {
    event(id, json!({"type": "delta", "text": text}))
}

fn completed(id: &str) -> Value {
    event(id, json!({"type": "completed"}))
}

fn stopped(id: &str) -> Value {
    event(id, json!({"type": "stopped"}))
}

fn id_of(frame: &Value) -> String {
    frame["id"].as_str().unwrap().to_owned()
}

fn label_of(frame: &Value) -> String {
    frame["params"]["label"].as_str().unwrap_or("").to_owned()
}

impl FakeBroker {
    fn start(apps: &[&str], handler: Handler) -> Self {
        let root = std::env::temp_dir().join(format!(
            "seatline-remote-{}",
            &config::random_token().unwrap()[..12]
        ));
        std::fs::create_dir_all(&root).unwrap();
        for app in apps {
            grant(&root, app);
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(AtomicBool::new(false));
        let delay = Arc::new(AtomicUsize::new(0));
        let (stop, stopped) = oneshot::channel();
        let (listening, ready) = std::sync::mpsc::channel();
        let shared = (
            root.clone(),
            seen.clone(),
            connections.clone(),
            closed.clone(),
            open.clone(),
            busy.clone(),
            delay.clone(),
        );
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(serve(shared, handler, stopped, listening));
        });
        ready.recv().expect("the scripted broker never listened");
        Self {
            root,
            seen,
            connections,
            closed,
            open,
            busy,
            delay,
            stop: Some(stop),
        }
    }

    fn client(&self, app: &str) -> RemoteClient {
        RemoteClient::with_root(app, self.root.clone())
    }

    fn client_with(&self, app: &str, limits: Limits) -> RemoteClient {
        RemoteClient::start(app, Some(self.root.clone()), limits)
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The frames of `method` that came in, in order.
    fn frames(&self, method: &str) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, frame)| frame["method"] == method)
            .map(|(_, frame)| frame.clone())
            .collect()
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut(&FakeBroker) -> bool) {
        let give_up = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(Instant::now() < give_up, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Closes connection `number` from the broker's side.
    fn hang_up(&self, number: usize) {
        self.open.lock().unwrap()[number - 1].hangup.notify_one();
    }

    fn say(&self, number: usize, frame: Value) {
        let _ = self.open.lock().unwrap()[number - 1].out.send(frame);
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        drop(self.stop.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn grant(root: &std::path::Path, app: &str) {
    let grant = config::grant_from_args(app, "codex", &[]).unwrap();
    config::write_private(
        &config::app_path(root, app).unwrap(),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
}

type Shared = (
    PathBuf,
    Arc<Mutex<Vec<(usize, Value)>>>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Connection>>>,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
);

async fn serve(
    shared: Shared,
    handler: Handler,
    mut stop: oneshot::Receiver<()>,
    listening: std::sync::mpsc::Sender<()>,
) {
    let listener = ListenerOptions::new()
        .name(client::socket_name(&shared.0).unwrap())
        .create_tokio()
        .unwrap();
    let _ = listening.send(());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok(stream) = accepted else { continue };
                let (shared, handler) = (shared.clone(), handler.clone());
                tokio::spawn(async move { let _ = connection(stream, shared, handler).await; });
            }
            _ = &mut stop => return,
        }
    }
}

async fn connection(
    mut stream: interprocess::local_socket::tokio::Stream,
    (root, seen, connections, closed, open, busy, delay): Shared,
    handler: Handler,
) -> std::io::Result<()> {
    let auth = wire::read_frame(&mut stream).await?;
    let app = auth["app"].as_str().unwrap_or("").to_owned();
    let token = auth["token"].as_str().unwrap_or("").to_owned();
    let granted = config::load_grant(&root, &app)?;
    if !config::same_token(&granted.token, &token) {
        return Ok(());
    }
    tokio::time::sleep(Duration::from_millis(delay.load(Ordering::SeqCst) as u64)).await;
    if busy.load(Ordering::SeqCst) {
        wire::write_frame(&mut stream, &json!({"type":"busy"})).await?;
        return Ok(());
    }
    wire::write_frame(&mut stream, &json!({"type":"ready","version":1})).await?;
    let number = connections.fetch_add(1, Ordering::SeqCst) + 1;
    let (mut reader, mut writer) = split(stream);
    let (out, mut replies) = mpsc::unbounded_channel::<Value>();
    let hangup = Arc::new(Notify::new());
    open.lock().unwrap().push(Connection {
        out: out.clone(),
        hangup: hangup.clone(),
    });
    let writing = tokio::spawn(async move {
        while let Some(frame) = replies.recv().await {
            if wire::write_frame(&mut writer, &frame).await.is_err() {
                return;
            }
        }
        let _ = writer.shutdown().await;
    });
    let ctx = Ctx { out };
    let result = async {
        loop {
            let frame = tokio::select! {
                frame = wire::read_frame(&mut reader) => frame?,
                () = hangup.notified() => return Ok::<(), std::io::Error>(()),
            };
            // The hub re-checks the grant on every frame, and closes a
            // connection whose grant is gone or changed without serving it.
            let current = config::load_grant(&root, &app);
            if !current.is_ok_and(|current| config::same_token(&current.token, &token)) {
                return Ok(());
            }
            seen.lock().unwrap().push((number, frame.clone()));
            handler(&ctx, &frame);
        }
    }
    .await;
    writing.abort();
    closed.fetch_add(1, Ordering::SeqCst);
    result
}

/// Reads `exchange` to its end.
fn drain(exchange: &mut dyn Exchange) -> Vec<Update> {
    let give_up = Instant::now() + Duration::from_secs(10);
    let mut updates = Vec::new();
    while Instant::now() < give_up {
        if let Some(update) = exchange.next(Instant::now() + Duration::from_millis(100)) {
            let terminal = update.is_terminal();
            updates.push(update);
            if terminal {
                assert!(exchange.next(Instant::now()).is_none(), "ended twice");
                return updates;
            }
        }
    }
    panic!("the request never ended: {updates:?}");
}

fn text(updates: &[Update]) -> String {
    updates
        .iter()
        .filter_map(|update| match update {
            Update::Delta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn failure_reason(updates: &[Update]) -> Option<(&'static str, bool)> {
    match updates.last() {
        Some(Update::Failed(failure)) => Some((failure.reason, failure.retryable)),
        _ => None,
    }
}

/// A broker that completes `quick` requests at once and holds `hold` ones.
fn by_label() -> Handler {
    Arc::new(|ctx, frame| {
        let id = id_of(frame);
        match (frame["method"].as_str(), label_of(frame).as_str()) {
            (Some("cancel"), _) => {
                let _ = ctx
                    .out
                    .send(stopped(frame["target"].as_str().unwrap_or("")));
            }
            (_, "hold") => {}
            _ => {
                let _ = ctx.out.send(delta(&id, "ok"));
                let _ = ctx.out.send(completed(&id));
            }
        }
    })
}

fn request(client: &RemoteClient, label: &str) -> Box<dyn Exchange> {
    client.request("send", "codex", json!({"label": label}))
}

#[test]
fn concurrent_requests_are_routed_to_their_own_exchange_over_one_connection() {
    let handler: Handler = Arc::new(|ctx, frame| {
        let (id, label) = (id_of(frame), label_of(frame));
        let out = ctx.out.clone();
        tokio::spawn(async move {
            for k in 0..20 {
                let _ = out.send(delta(&id, &format!("{label}:{k};")));
                if k % 5 == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            let _ = out.send(completed(&id));
        });
    });
    let broker = FakeBroker::start(&["app"], handler);
    let client = broker.client("app");
    // Exchanges are not sent between threads: each thread makes its own, all
    // at once, and reads it.
    let start = Arc::new(std::sync::Barrier::new(8));
    let readers: Vec<_> = (0..8)
        .map(|index| {
            let (client, start) = (client.clone(), start.clone());
            std::thread::spawn(move || {
                let label = format!("q{index}");
                start.wait();
                let mut exchange = request(&client, &label);
                (label, drain(exchange.as_mut()))
            })
        })
        .collect();
    for reader in readers {
        let (label, updates) = reader.join().unwrap();
        let expected: String = (0..20).map(|k| format!("{label}:{k};")).collect();
        assert_eq!(
            text(&updates),
            expected,
            "{label} got another request's text"
        );
        assert_eq!(updates.last(), Some(&Update::Completed));
        assert_eq!(updates.iter().filter(|u| u.is_terminal()).count(), 1);
    }
    // One connection, one handshake, and eight different valid request IDs.
    assert_eq!(broker.connections(), 1);
    let ids: std::collections::BTreeSet<String> = broker.frames("send").iter().map(id_of).collect();
    assert_eq!(ids.len(), 8);
    assert!(ids.iter().all(|id| wire::valid_id(id)), "{ids:?}");
}

#[test]
fn a_cancel_stops_one_request_and_no_other() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client("app");
    let mut first = request(&client, "hold");
    let mut second = request(&client, "hold");
    broker.wait_for("both requests to arrive", |b| b.frames("send").len() == 2);
    let first_id = id_of(&broker.frames("send")[0]);

    first.cancel(Duration::ZERO);
    assert_eq!(drain(first.as_mut()), [Update::Stopped]);
    let cancels = broker.frames("cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["target"], first_id.as_str());

    // The other request is still running, and finishes as its own.
    assert!(
        second
            .next(Instant::now() + Duration::from_millis(100))
            .is_none()
    );
    broker.say(1, delta(&id_of(&broker.frames("send")[1]), "still here"));
    broker.say(1, completed(&id_of(&broker.frames("send")[1])));
    let updates = drain(second.as_mut());
    assert_eq!(text(&updates), "still here");
    assert_eq!(updates.last(), Some(&Update::Completed));
    assert_eq!(broker.frames("cancel").len(), 1);
    assert_eq!(broker.connections(), 1);
}

#[test]
fn dropping_an_exchange_cancels_its_request_and_frees_its_place() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client_with(
        "app",
        Limits {
            max_in_flight: 1,
            ..Limits::default()
        },
    );
    let held = request(&client, "hold");
    broker.wait_for("the request", |b| b.frames("send").len() == 1);
    // The one place is taken: another request is refused without being sent.
    let mut refused = request(&client, "hold");
    let updates = drain(refused.as_mut());
    assert_eq!(failure_reason(&updates), Some(("QUEUE_FULL", true)));
    assert_eq!(broker.frames("send").len(), 1);

    drop(held);
    broker.wait_for("the cancel", |b| b.frames("cancel").len() == 1);
    assert_eq!(
        broker.frames("cancel")[0]["target"],
        id_of(&broker.frames("send")[0]).as_str()
    );
    let mut next = request(&client, "quick");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
}

#[test]
fn a_finished_exchange_its_consumer_keeps_does_not_hold_a_place_in_flight() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client_with(
        "app",
        Limits {
            max_in_flight: 1,
            ..Limits::default()
        },
    );
    let mut first = request(&client, "quick");
    assert_eq!(drain(first.as_mut()).last(), Some(&Update::Completed));
    // `first` is still held, and nothing is in flight: the next is admitted.
    let mut second = request(&client, "quick");
    assert_eq!(drain(second.as_mut()).last(), Some(&Update::Completed));
    drop((first, second));

    // However many finished exchanges are kept, the default limit is never
    // used up by them.
    let client = broker.client("app");
    let kept: Vec<_> = (0..Limits::default().max_in_flight + 6)
        .map(|_| {
            let mut exchange = request(&client, "quick");
            assert_eq!(drain(exchange.as_mut()).last(), Some(&Update::Completed));
            exchange
        })
        .collect();
    assert_eq!(kept.len(), Limits::default().max_in_flight + 6);
}

#[test]
fn a_slow_consumer_is_stopped_alone_with_what_it_was_given_intact() {
    const BOUND: usize = 32 * 1024;
    let handler: Handler = Arc::new(|ctx, frame| {
        let (id, label) = (id_of(frame), label_of(frame));
        match (frame["method"].as_str(), label.as_str()) {
            (Some("cancel"), _) => {
                let _ = ctx
                    .out
                    .send(stopped(frame["target"].as_str().unwrap_or("")));
            }
            (_, "flood") => {
                // Numbered pieces of 1 KiB, far more than the bound, with no end.
                for n in 0..3_000_u32 {
                    let _ = ctx
                        .out
                        .send(delta(&id, &format!("{n:08}{}", "x".repeat(1016))));
                }
            }
            _ => {
                let _ = ctx.out.send(delta(&id, "ok"));
                let _ = ctx.out.send(completed(&id));
            }
        }
    });
    let broker = FakeBroker::start(&["app"], handler);
    let client = broker.client_with(
        "app",
        Limits {
            max_unread_bytes: BOUND,
            ..Limits::default()
        },
    );
    // Nothing reads the flooded request while the broker floods it.
    let mut flooded = request(&client, "flood");
    broker.wait_for("the cancel for the flooded request", |b| {
        b.frames("cancel").len() == 1
    });
    assert_eq!(
        broker.frames("cancel")[0]["target"],
        id_of(&broker.frames("send")[0]).as_str()
    );

    // The other requests on the same connection are not held up by it.
    let mut other = request(&client, "quick");
    assert_eq!(text(&drain(other.as_mut())), "ok");

    // What the flooded consumer was given is the start of the answer, whole,
    // in order, within the bound; then one failure says the rest was not kept.
    let updates = drain(flooded.as_mut());
    let numbers: Vec<u32> = updates
        .iter()
        .filter_map(|update| match update {
            Update::Delta(text) => Some(text[..8].parse().unwrap()),
            _ => None,
        })
        .collect();
    assert!(!numbers.is_empty());
    assert_eq!(numbers, (0..numbers.len() as u32).collect::<Vec<_>>());
    assert!(
        text(&updates).len() <= BOUND + 1024,
        "{} bytes queued behind a bound of {BOUND}",
        text(&updates).len()
    );
    assert_eq!(
        failure_reason(&updates),
        Some(("CONSUMER_TOO_SLOW", false)),
        "{:?}",
        updates.last()
    );
    // The connection was never given up on, for either.
    let mut after = request(&client, "quick");
    assert_eq!(drain(after.as_mut()).last(), Some(&Update::Completed));
    assert_eq!(broker.connections(), 1);
}

#[test]
fn a_lost_connection_ends_what_was_in_flight_once_and_the_next_request_reconnects() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client("app");
    let mut lost = request(&client, "hold");
    broker.wait_for("the request", |b| b.frames("send").len() == 1);

    broker.hang_up(1);
    let updates = drain(lost.as_mut());
    assert_eq!(
        failure_reason(&updates),
        Some(("COMPANION_DISCONNECTED", true))
    );

    // The next request connects again and completes...
    let mut next = request(&client, "quick");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
    assert_eq!(broker.connections(), 2);
    // ...and the one that was lost was never sent a second time, by anything.
    let held: Vec<_> = broker
        .frames("send")
        .into_iter()
        .filter(|frame| label_of(frame) == "hold")
        .collect();
    assert_eq!(held.len(), 1, "a lost request was replayed");
}

#[test]
fn a_broker_that_says_something_unreadable_costs_the_connection_not_the_client() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client("app");
    let mut broken = request(&client, "hold");
    broker.wait_for("the request", |b| b.frames("send").len() == 1);
    broker.say(
        1,
        event(
            &id_of(&broker.frames("send")[0]),
            json!({"type": "nonsense"}),
        ),
    );
    assert_eq!(
        failure_reason(&drain(broken.as_mut())),
        Some(("COMPANION_DISCONNECTED", true))
    );
    let mut next = request(&client, "quick");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
    assert_eq!(broker.connections(), 2);
}

#[test]
fn a_rotated_grant_costs_only_the_requests_in_flight_and_the_next_connection_uses_the_new_one() {
    let broker = FakeBroker::start(&["app", "other"], by_label());
    let app = broker.client("app");
    let other = broker.client("other");
    let mut working = request(&app, "quick");
    assert_eq!(drain(working.as_mut()).last(), Some(&Update::Completed));
    let mut held = request(&app, "hold");
    broker.wait_for("the held request", |b| b.frames("send").len() == 2);

    // The grant changes. The broker closes the app's connection at its next
    // frame, which ends what is in flight on it, and serves nothing from it.
    grant(&broker.root, "app");
    let mut caught = request(&app, "quick");
    for exchange in [&mut held, &mut caught] {
        assert_eq!(
            failure_reason(&drain(exchange.as_mut())),
            Some(("COMPANION_DISCONNECTED", true))
        );
    }
    // Another app's connection, and its requests, are untouched.
    let mut elsewhere = request(&other, "quick");
    assert_eq!(drain(elsewhere.as_mut()).last(), Some(&Update::Completed));

    // The next request reads the new grant and goes through, on a new connection.
    let before = broker.connections();
    let mut next = request(&app, "quick");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
    assert_eq!(broker.connections(), before + 1);
}

#[test]
fn a_revoked_grant_ends_the_connection_and_then_refuses_new_requests_for_that_app_alone() {
    let broker = FakeBroker::start(&["app", "other"], by_label());
    let app = broker.client("app");
    let other = broker.client("other");
    let mut working = request(&app, "quick");
    assert_eq!(drain(working.as_mut()).last(), Some(&Update::Completed));
    let mut held = request(&other, "hold");
    broker.wait_for("the held request", |b| b.frames("send").len() == 2);

    std::fs::remove_file(config::app_path(&broker.root, "app").unwrap()).unwrap();
    // The first request after the revocation finds the connection closed...
    let mut caught = request(&app, "quick");
    assert_eq!(
        failure_reason(&drain(caught.as_mut())),
        Some(("COMPANION_DISCONNECTED", true))
    );
    // ...and the ones after it are refused, without a connection.
    let before = broker.connections();
    let mut refused = request(&app, "quick");
    assert_eq!(
        failure_reason(&drain(refused.as_mut())),
        Some(("APP_NOT_AUTHORIZED", false))
    );
    assert_eq!(broker.connections(), before);

    // The other app's request, held throughout, is still its own to finish.
    broker.say(2, completed(&id_of(&broker.frames("send")[1])));
    assert_eq!(drain(held.as_mut()).last(), Some(&Update::Completed));
}

#[test]
fn an_app_with_no_grant_is_refused_without_reaching_the_broker() {
    let broker = FakeBroker::start(&[], by_label());
    let client = broker.client("nobody");
    let mut refused = request(&client, "quick");
    assert_eq!(
        failure_reason(&drain(refused.as_mut())),
        Some(("APP_NOT_AUTHORIZED", false))
    );
    assert_eq!(broker.connections(), 0);
}

#[test]
fn a_broker_with_no_room_is_reported_as_busy_and_asked_again_by_the_next_request() {
    let broker = FakeBroker::start(&["app"], by_label());
    broker.busy.store(true, Ordering::SeqCst);
    let client = broker.client("app");
    let mut turned_away = request(&client, "quick");
    assert_eq!(
        failure_reason(&drain(turned_away.as_mut())),
        Some(("QUEUE_FULL", true))
    );
    broker.busy.store(false, Ordering::SeqCst);
    let mut next = request(&client, "quick");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
}

#[test]
fn a_request_too_big_for_a_frame_is_refused_alone() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client("app");
    let mut working = request(&client, "quick");
    assert_eq!(drain(working.as_mut()).last(), Some(&Update::Completed));
    let mut huge = client.request(
        "send",
        "codex",
        json!({"label": "quick", "text": "x".repeat(wire::MAX_FRAME)}),
    );
    assert_eq!(
        failure_reason(&drain(huge.as_mut())),
        Some(("INVALID_REQUEST", false))
    );
    // The connection was not touched.
    let mut next = request(&client, "quick");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
    assert_eq!(broker.connections(), 1);
    assert_eq!(broker.frames("send").len(), 2);
}

#[test]
fn closing_the_client_stops_what_is_in_flight_and_refuses_what_comes_after() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client("app");
    let mut held = request(&client, "hold");
    broker.wait_for("the request", |b| b.frames("send").len() == 1);

    client.close();
    assert_eq!(drain(held.as_mut()), [Update::Stopped]);
    broker.wait_for("the connection to close", |b| {
        b.closed.load(Ordering::SeqCst) == 1
    });
    let mut after = request(&client, "quick");
    assert_eq!(
        failure_reason(&drain(after.as_mut())),
        Some(("COMPANION_DISCONNECTED", true))
    );
}

#[test]
fn dropping_every_handle_closes_the_connection() {
    let broker = FakeBroker::start(&["app"], by_label());
    let client = broker.client("app");
    let clone = client.clone();
    let mut working = request(&clone, "quick");
    assert_eq!(drain(working.as_mut()).last(), Some(&Update::Completed));
    drop(client);
    // A clone and an exchange still hold the client, and so the connection.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(broker.closed.load(Ordering::SeqCst), 0);
    drop(working);
    drop(clone);
    broker.wait_for("the connection to close", |b| {
        b.closed.load(Ordering::SeqCst) == 1
    });
}

#[test]
fn the_compatibility_adapter_shares_a_clients_connection_or_opens_its_own() {
    let broker = FakeBroker::start(&["app"], by_label());
    let metadata = Codex::new(SearchPath::new([]), broker.root.join("work"));

    // With a shared client, a status, a turn and a cleanup are one connection.
    let shared = RemoteProvider::with_client("app", broker.client("app"), &metadata);
    let mut status = shared.status();
    assert_eq!(drain(status.as_mut()).last(), Some(&Update::Completed));
    (shared.cleanup_group("group-1").work)().unwrap();
    assert_eq!(broker.connections(), 1);
    let methods: Vec<String> = broker
        .frames("status")
        .iter()
        .chain(broker.frames("cleanup").iter())
        .map(|frame| frame["provider"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(methods, ["codex", "codex"]);
    assert!(
        broker
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(_, frame)| frame["id"] != "request"),
        "a shared client's requests have IDs of their own"
    );
}

#[test]
fn scheduling_hints_and_queue_events_route_through_the_shared_client_and_adapter() {
    use seatline_companion::scheduling::Hints;
    let broker = FakeBroker::start(
        &["app"],
        Arc::new(|ctx, frame| {
            let id = id_of(frame);
            if frame["scheduling"]["events"] == true {
                let _ = ctx.out.send(event(
                    &id,
                    json!({"type":"queued","ahead":2,"timeout_ms":1000}),
                ));
                let _ = ctx.out.send(event(&id, json!({"type":"admitted"})));
            }
            let _ = ctx.out.send(completed(&id));
        }),
    );
    let client = broker.client("app");
    let hints = Hints {
        interactive: true,
        queue_timeout_ms: Some(1000),
        events: true,
    };
    let expected = [
        Update::Queued {
            ahead: 2,
            timeout_ms: 1000,
        },
        Update::Admitted,
        Update::Completed,
    ];
    let mut direct = client.request_with_scheduling("status", "codex", Value::Null, hints);
    assert_eq!(drain(direct.as_mut()), expected);
    let metadata = Codex::new(SearchPath::new([]), broker.root.join("work"));
    let provider =
        RemoteProvider::with_client("app", client.clone(), &metadata).with_scheduling(hints);
    let mut status = provider.status();
    assert_eq!(drain(status.as_mut()), expected);
    (provider.cleanup_group("group-1").work)().unwrap();
    let mut legacy = client.status("codex");
    assert_eq!(drain(legacy.as_mut()), [Update::Completed]);
    assert_eq!(broker.connections(), 1);
    let status = broker.frames("status");
    assert_eq!(status.len(), 3);
    assert_eq!(status[0]["scheduling"], json!(hints));
    assert_eq!(status[1]["scheduling"], json!(hints));
    assert_eq!(broker.frames("cleanup")[0]["scheduling"], json!(hints));
    assert_eq!(status[2]["scheduling"]["events"], false);
}

#[test]
fn requests_that_wait_for_the_connection_reach_the_broker_in_the_order_they_were_made() {
    let broker = FakeBroker::start(&["app"], by_label());
    // The handshake takes long enough that every request is made before it ends.
    broker.delay.store(100, Ordering::SeqCst);
    let client = broker.client("app");
    let mut exchanges: Vec<_> = (0..20).map(|_| request(&client, "quick")).collect();
    for exchange in &mut exchanges {
        assert_eq!(drain(exchange.as_mut()).last(), Some(&Update::Completed));
    }
    let ids: Vec<String> = broker.frames("send").iter().map(id_of).collect();
    let in_order: Vec<String> = (1..=20).map(|n| format!("r{n}")).collect();
    assert_eq!(ids, in_order);
    assert_eq!(broker.connections(), 1);
}

#[test]
fn a_request_cancelled_before_the_connection_is_up_is_never_sent() {
    let broker = FakeBroker::start(&["app"], by_label());
    broker.delay.store(200, Ordering::SeqCst);
    let client = broker.client("app");
    let mut early = request(&client, "quick");
    let mut kept = request(&client, "quick");
    early.cancel(Duration::ZERO);
    assert_eq!(drain(early.as_mut()), [Update::Stopped]);
    // The one that was not cancelled goes through on the connection that came up.
    assert_eq!(drain(kept.as_mut()).last(), Some(&Update::Completed));
    assert_eq!(broker.frames("send").len(), 1);
    assert!(broker.frames("cancel").is_empty());
}
