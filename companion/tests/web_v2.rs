//! The hosted-app transport against a real broker, with a scripted relay
//! standing in for the browser. It checks what the relay is able to do to a
//! frame: drop, repeat or replay it. None of that may be processed, and none of
//! it may leave the pairing stuck.
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::{Aes256Gcm, aead::KeyInit};
use futures_util::{SinkExt, StreamExt};
use seatline_companion::secure::{self, Epoch};
use seatline_companion::web::{Timing, run_channel};
use seatline_companion::{client, config};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

type Socket = WebSocketStream<TcpStream>;
type Log = Arc<Mutex<Vec<String>>>;

fn key() -> Aes256Gcm {
    Aes256Gcm::new_from_slice(&[5; 32]).unwrap()
}

fn note(log: &Log, entry: impl Into<String>) {
    log.lock().unwrap().push(entry.into());
}

enum Next {
    Frame(Value),
    /// The helper ended the connection.
    Closed,
    /// Nothing arrived, and the connection is still open.
    Silent,
}

/// The next frame from the helper. Pings are answered so the link stays alive.
async fn poll(socket: &mut Socket, patience: Duration) -> Next {
    loop {
        let Ok(message) = tokio::time::timeout(patience, socket.next()).await else {
            return Next::Silent;
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "ping" {
                    let _ = socket
                        .send(Message::Text(json!({"type":"pong"}).to_string().into()))
                        .await;
                    continue;
                }
                return Next::Frame(value);
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Next::Closed,
            Some(Ok(_)) => continue,
        }
    }
}

/// A frame that must arrive; anything else fails the test.
async fn next(socket: &mut Socket) -> Option<Value> {
    match poll(socket, Duration::from_secs(5)).await {
        Next::Frame(value) => Some(value),
        Next::Closed | Next::Silent => None,
    }
}

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// The browser's half of the handshake; returns the epoch it agreed with the helper.
async fn handshake(socket: &mut Socket) -> Epoch {
    let browser = secure::fresh_nonce().unwrap();
    send(
        socket,
        secure::seal_hello(&key(), "browser", &browser, None).unwrap(),
    )
    .await;
    let reply = next(socket).await.expect("the helper must answer a hello");
    let (helper, echo) = secure::open_hello(&key(), "helper", &reply).unwrap();
    assert_eq!(echo, Some(browser), "the helper must echo our nonce");
    Epoch { helper, browser }
}

fn status_request(id: &str) -> Value {
    json!({"id":id,"provider":"codex","method":"status","params":null})
}

/// Reads the helper's events for one request, checking that it numbers its
/// own frames from 1 without gaps, until the request ends.
async fn read_to_end(socket: &mut Socket, epoch: &Epoch, id: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut expected = 1;
    loop {
        let frame = next(socket)
            .await
            .expect("the helper went away mid-request");
        assert_eq!(secure::envelope_sequence(&frame).unwrap(), expected);
        expected += 1;
        let packet = secure::open_frame(&key(), "helper", epoch, &frame).unwrap();
        if packet["id"] != id {
            continue;
        }
        let ended = ["completed", "failed", "stopped"]
            .contains(&packet["event"]["type"].as_str().unwrap_or_default());
        events.push(packet);
        if ended {
            return events;
        }
    }
}

/// True only if the helper actually ended the connection. A helper that
/// ignored the frame and stayed connected is not "dropped", nor is silence.
async fn dropped(socket: &mut Socket) -> bool {
    matches!(poll(socket, Duration::from_secs(5)).await, Next::Closed)
}

async fn play(mut socket: Socket, index: usize, log: Log, captured: Arc<Mutex<Option<Value>>>) {
    // Authenticate as the relay would, and report the browser as present.
    let auth = next(&mut socket)
        .await
        .expect("the helper authenticates first");
    assert_eq!(auth["type"], "auth");
    send(&mut socket, json!({"type":"ready","peer":true})).await;

    match index {
        0 => {
            let epoch = handshake(&mut socket).await;
            note(&log, "1: handshake");
            let request =
                secure::seal_frame(&key(), "browser", &epoch, 1, &status_request("a")).unwrap();
            *captured.lock().unwrap() = Some(request.clone());
            send(&mut socket, request).await;
            let events = read_to_end(&mut socket, &epoch, "a").await;
            note(
                &log,
                format!(
                    "1: request ended with {}",
                    events.last().unwrap()["event"]["type"]
                ),
            );
            // Frame 2 is "lost": the next one claims to be frame 3.
            send(
                &mut socket,
                secure::seal_frame(&key(), "browser", &epoch, 3, &status_request("b")).unwrap(),
            )
            .await;
            note(
                &log,
                format!("1: gap dropped={}", dropped(&mut socket).await),
            );
        }
        1 => {
            let epoch = handshake(&mut socket).await;
            note(&log, "2: handshake resynced after the gap");
            // A frame the helper accepted before, presented in a new epoch.
            let old = captured.lock().unwrap().clone().unwrap();
            send(&mut socket, old).await;
            note(
                &log,
                format!("2: replayed frame dropped={}", dropped(&mut socket).await),
            );
            let _ = epoch;
        }
        2 => {
            let epoch = handshake(&mut socket).await;
            let request =
                secure::seal_frame(&key(), "browser", &epoch, 1, &status_request("c")).unwrap();
            send(&mut socket, request.clone()).await;
            let events = read_to_end(&mut socket, &epoch, "c").await;
            note(
                &log,
                format!(
                    "3: request ended with {}",
                    events.last().unwrap()["event"]["type"]
                ),
            );
            // The same frame again inside the same epoch.
            send(&mut socket, request).await;
            note(
                &log,
                format!("3: repeated frame dropped={}", dropped(&mut socket).await),
            );
        }
        3 => {
            // Data with no handshake at all, as an older (protocol 1) browser would send.
            let epoch = Epoch {
                helper: [1; 16],
                browser: [2; 16],
            };
            send(
                &mut socket,
                secure::seal_frame(&key(), "browser", &epoch, 1, &status_request("d")).unwrap(),
            )
            .await;
            note(
                &log,
                format!(
                    "4: data before a handshake dropped={}",
                    dropped(&mut socket).await
                ),
            );
        }
        _ => {
            // Keep later reconnects quiet.
            while next(&mut socket).await.is_some() {}
        }
    }
}

#[tokio::test]
#[allow(clippy::disallowed_methods)] // Runs the built broker, never a provider.
async fn a_lost_repeated_or_replayed_frame_ends_the_connection_and_the_next_handshake_recovers() {
    let root = std::env::temp_dir().join(format!(
        "seatline-web-v2-{}",
        &config::random_token().unwrap()[..12]
    ));
    let providers = root.join("no-providers");
    std::fs::create_dir_all(&providers).unwrap();
    let exe = env!("CARGO_BIN_EXE_seatline-companion");
    assert!(
        Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
            .args(["authorize", "test_app", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let token = config::load_grant(&root, "test_app").unwrap().token;
    let broker = Broker(
        Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
            .env("TEST_APP_PROVIDER_PATH", &providers)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..100 {
        if client::connect(&root).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/channels/x/helper", listener.local_addr().unwrap());
    let log: Log = Log::default();
    let captured = Arc::new(Mutex::new(None));
    let (finished, mut done) = tokio::sync::mpsc::unbounded_channel();
    {
        let (log, captured) = (log.clone(), captured.clone());
        tokio::spawn(async move {
            let mut index = 0;
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                let (log, captured, finished) = (log.clone(), captured.clone(), finished.clone());
                let current = index;
                index += 1;
                tokio::spawn(async move {
                    play(socket, current, log, captured).await;
                    let _ = finished.send(current);
                });
            }
        });
    }

    let timing = Timing {
        connect: Duration::from_secs(2),
        ping_every: Duration::from_millis(100),
        dead_after: Duration::from_secs(3),
        backoff_start: Duration::from_millis(30),
        backoff_max: Duration::from_millis(100),
        stable_after: Duration::from_secs(30),
    };
    let helper_root = root.clone();
    let helper = tokio::spawn(async move {
        run_channel(
            &helper_root,
            "test_app",
            &token,
            &endpoint,
            "h",
            &key(),
            timing,
        )
        .await
    });

    // Every scripted connection (0 to 3) must run to its end.
    let mut finished_connections = Vec::new();
    while finished_connections.len() < 4 {
        let index = tokio::time::timeout(Duration::from_secs(30), done.recv())
            .await
            .expect("the scripted connections did not all finish")
            .unwrap();
        finished_connections.push(index);
    }
    helper.abort();
    drop(broker);

    let log = log.lock().unwrap().clone();
    let ended = |needle: &str| log.iter().any(|entry| entry == needle);
    assert!(ended("1: handshake"), "{log:?}");
    assert!(ended("1: request ended with \"completed\""), "{log:?}");
    assert!(ended("1: gap dropped=true"), "{log:?}");
    assert!(ended("2: handshake resynced after the gap"), "{log:?}");
    assert!(ended("2: replayed frame dropped=true"), "{log:?}");
    assert!(ended("3: request ended with \"completed\""), "{log:?}");
    assert!(ended("3: repeated frame dropped=true"), "{log:?}");
    assert!(ended("4: data before a handshake dropped=true"), "{log:?}");
    let _ = std::fs::remove_dir_all(root);
}
