//! Generic outbound connection for an authorized hosted app. App orchestration
//! and relay hosting live outside Seatline. Only neutral provider frames pass.
use crate::{PROTOCOL_VERSION, client, config, secure, wire};
use aes_gcm::{Aes256Gcm, aead::KeyInit};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::{self, Message};

fn error(message: &str) -> io::Error {
    io::Error::other(message)
}
/// How long the helper waits, retries and probes the relay.
#[derive(Clone, Copy)]
pub struct Timing {
    pub connect: Duration,
    /// How often an authenticated connection is probed with a `ping` frame.
    pub ping_every: Duration,
    /// A connection that has produced no frame at all for this long is dead.
    pub dead_after: Duration,
    pub backoff_start: Duration,
    pub backoff_max: Duration,
    /// A connection that lasted this long resets the backoff.
    pub stable_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            ping_every: Duration::from_secs(25),
            dead_after: Duration::from_secs(75),
            backoff_start: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(30),
        }
    }
}

/// Why one relay connection ended. Only a refusal that repeating cannot fix
/// stops the helper.
enum End {
    Retry(io::Error),
    Fatal(io::Error),
}

impl From<io::Error> for End {
    fn from(value: io::Error) -> Self {
        Self::Retry(value)
    }
}
impl From<serde_json::Error> for End {
    fn from(value: serde_json::Error) -> Self {
        Self::Retry(io::Error::other(value))
    }
}
impl End {
    fn retry(message: impl AsRef<str>) -> Self {
        Self::Retry(error(message.as_ref()))
    }
    fn fatal(message: impl AsRef<str>) -> Self {
        Self::Fatal(error(message.as_ref()))
    }
}

pub async fn pair(root: &Path, app: &str, relay: &str, site: &str, launch: bool) -> io::Result<()> {
    let grant = config::load_grant(root, app)?;
    let relay = reqwest::Url::parse(relay).map_err(io::Error::other)?;
    let mut site = reqwest::Url::parse(site).map_err(io::Error::other)?;
    if relay.scheme() != "https"
        || site.scheme() != "https"
        || !relay.username().is_empty()
        || !site.username().is_empty()
        || relay.password().is_some()
        || site.password().is_some()
        || relay.query().is_some()
        || relay.fragment().is_some()
        || !grant
            .web_origins
            .contains(&site.origin().ascii_serialization())
        || !grant
            .web_relays
            .contains(&relay.origin().ascii_serialization())
    {
        return Err(error(
            "website and relay must be locally authorized HTTPS origins",
        ));
    }
    let _lock = config::lock(root, &format!("{app}-web.lock"))?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let response = http
        .post(relay.join("/pair").map_err(io::Error::other)?)
        .json(&json!({"app":app,"origin":site.origin().ascii_serialization()}))
        .send()
        .await
        .map_err(io::Error::other)?;
    if !response.status().is_success() {
        return Err(error("relay refused pairing"));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if bytes.len() + chunk.len() > 4096 {
            return Err(error("invalid pairing response"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() > 4096 {
        return Err(error("invalid pairing response"));
    }
    let pair: Value = serde_json::from_slice(&bytes)?;
    for name in ["id", "helper", "browser"] {
        if !pair[name]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(error("invalid pairing response"));
        }
    }
    let mut key_bytes = [0_u8; 32];
    getrandom::fill(&mut key_bytes).map_err(|_| error("randomness unavailable"))?;
    let key = Aes256Gcm::new_from_slice(&key_bytes).map_err(|_| error("invalid key"))?;
    site.set_fragment(Some(&format!(
        "seatline={}:{}:{}",
        pair["id"].as_str().unwrap(),
        pair["browser"].as_str().unwrap(),
        URL_SAFE_NO_PAD.encode(key_bytes)
    )));
    println!("Open this private pairing link:\n{site}");
    if launch {
        // The link holds the browser credential and the encryption key, so it
        // never goes on a command line where other local users could read it.
        let page = prepare_launch(root, site.as_str())?;
        if let Err(failure) = open_website(&page) {
            let _ = std::fs::remove_file(&page);
            return Err(failure);
        }
        tokio::spawn(async move {
            tokio::time::sleep(LAUNCH_PAGE_LIFETIME).await;
            let _ = std::fs::remove_file(page);
        });
    }
    let mut endpoint = relay
        .join(&format!(
            "/channels/{}/helper",
            pair["id"].as_str().unwrap()
        ))
        .map_err(io::Error::other)?;
    endpoint
        .set_scheme("wss")
        .map_err(|_| error("invalid relay scheme"))?;
    run_channel(
        root,
        app,
        &grant.token,
        endpoint.as_str(),
        pair["helper"].as_str().unwrap(),
        &key,
        Timing::default(),
    )
    .await
}

/// Keeps the helper connected to the relay until the pairing can no longer
/// work: the app was revoked or reauthorized, or the relay says the pairing is
/// expired or unknown. Anything else is retried with growing delays.
pub async fn run_channel(
    root: &Path,
    app: &str,
    token: &str,
    endpoint: &str,
    helper: &str,
    key: &Aes256Gcm,
    timing: Timing,
) -> io::Result<()> {
    let mut delay = timing.backoff_start;
    loop {
        let started = Instant::now();
        let end = match connection(root, app, token, endpoint, helper, key, timing).await {
            Ok(never) => match never {},
            Err(end) => end,
        };
        match end {
            End::Fatal(failure) => return Err(failure),
            End::Retry(failure) => {
                if started.elapsed() >= timing.stable_after {
                    delay = timing.backoff_start;
                }
                eprintln!(
                    "Seatline: relay connection ended ({failure}); retrying in {:.1}s",
                    delay.as_secs_f32()
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(timing.backoff_max);
            }
        }
    }
}

/// `Some` when the app's grant no longer matches the one this helper started with.
fn grant_ended(root: &Path, app: &str, token: &str) -> Option<End> {
    match config::load_grant(root, app) {
        Ok(current) if config::same_token(&current.token, token) => None,
        Ok(_) => Some(End::fatal(
            "app authorization was replaced; run `pair` again to use the new credentials",
        )),
        Err(failure) if failure.kind() == io::ErrorKind::NotFound => {
            Some(End::fatal("app authorization revoked"))
        }
        // A grant being rewritten is not a revocation; the broker fails closed on its own.
        Err(_) => None,
    }
}

type BrokerWriter = tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>;

/// Drops the helper's connection to the broker, which cancels whatever the
/// browser had running, and discards events that were already on their way.
fn release_broker(
    writer: &mut Option<BrokerWriter>,
    reader: &mut Option<tokio::task::JoinHandle<()>>,
    events: &mut tokio::sync::mpsc::Receiver<Value>,
) {
    *writer = None;
    if let Some(reader) = reader.take() {
        reader.abort();
    }
    while events.try_recv().is_ok() {}
}

/// A WebSocket to the relay, over TCP with or without TLS.
type RelaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connects to the relay with Nagle's algorithm off.
///
/// Every event is sent as its own small encrypted frame. With Nagle on, a
/// frame written while the one before it is still unacknowledged waits for that
/// acknowledgement, which a relay may hold back for its delayed-ACK timer, so
/// streamed text reached the browser in lumps instead of as it arrived. The
/// option is set on the TCP socket before any TLS handshake.
async fn connect_relay(
    endpoint: &str,
) -> Result<(RelaySocket, tungstenite::handshake::client::Response), tungstenite::Error> {
    tokio_tungstenite::connect_async_with_config(endpoint, None, true).await
}

async fn connection(
    root: &Path,
    app: &str,
    token: &str,
    endpoint: &str,
    helper: &str,
    key: &Aes256Gcm,
    timing: Timing,
) -> Result<Infallible, End> {
    let connecting = tokio::time::timeout(timing.connect, connect_relay(endpoint));
    let (mut socket, _) = match connecting.await {
        Err(_) => return Err(End::retry("relay did not answer in time")),
        Ok(Err(tungstenite::Error::Http(response))) => {
            let status = response.status();
            return Err(if matches!(status.as_u16(), 401 | 403 | 404 | 410) {
                End::fatal(format!(
                    "relay refused this pairing (HTTP {status}); it has expired or is not valid, run `pair` again"
                ))
            } else {
                End::retry(format!("relay answered HTTP {status}"))
            });
        }
        Ok(Err(failure)) => return Err(End::Retry(io::Error::other(failure))),
        Ok(Ok(connected)) => connected,
    };
    socket
        .send(Message::Text(
            json!({"type":"auth","token":helper}).to_string().into(),
        ))
        .await
        .map_err(io::Error::other)?;
    let mut broker_writer: Option<BrokerWriter> = None;
    let mut broker_reader: Option<tokio::task::JoinHandle<()>> = None;
    let (output, mut events) = tokio::sync::mpsc::channel::<Value>(64);
    let mut check = tokio::time::interval(Duration::from_secs(1));
    let mut probe = tokio::time::interval(timing.ping_every);
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut authenticated, mut last_seen) = (false, Instant::now());
    // Set by the browser's hello. Sequence numbers count from 1 within it, so
    // nothing carries over from an earlier connection of either side.
    let mut epoch: Option<secure::Epoch> = None;
    let (mut sent, mut received) = (0_u64, 0_u64);
    let result: Result<Infallible, End> = async {
        loop {
            tokio::select! {
                _ = check.tick() => {
                    if let Some(end) = grant_ended(root, app, token) { return Err(end); }
                },
                _ = probe.tick() => {
                    // Any frame counts as life. A silent link (sleep, a changed
                    // network) would otherwise look connected for hours.
                    if last_seen.elapsed() > timing.dead_after {
                        return Err(End::retry("relay stopped responding"));
                    }
                    if authenticated {
                        socket.send(Message::Text(json!({"type":"ping"}).to_string().into())).await.map_err(io::Error::other)?;
                    }
                },
                event = events.recv(), if broker_writer.is_some() => {
                    let event = event.ok_or_else(|| error("broker disconnected"))?;
                    if event.is_null() { return Err(End::retry("broker disconnected")); }
                    // Events that outlived their epoch are dropped, never sent under another.
                    let Some(current) = epoch.as_ref() else { continue };
                    sent = sent.checked_add(1).ok_or_else(|| End::fatal("sequence exhausted"))?;
                    socket.send(Message::Text(secure::seal_frame(key,"helper",current,sent,&event)?.to_string().into())).await.map_err(io::Error::other)?;
                },
                message = socket.next() => {
                    let Some(message) = message else { return Err(End::retry("relay disconnected")); };
                    let message = message.map_err(io::Error::other)?;
                    last_seen = Instant::now();
                    let text = match message {
                        Message::Text(text) => text,
                        Message::Close(frame) => {
                            let (code, reason) = frame
                                .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                                .unwrap_or((1005, String::new()));
                            let description = format!("relay closed the connection ({code} {reason})");
                            // 1008 is the relay's policy refusal: expired pairing, refused credential.
                            return Err(if code == 1008 { End::fatal(description) } else { End::retry(description) });
                        },
                        _ => continue,
                    };
                    if text.len()>768*1024 { return Err(End::retry("relay frame too large")); }
                    let value: Value = serde_json::from_str(&text)?;
                    match value["type"].as_str() {
                        Some(kind @ ("ready"|"peer")) => {
                            if kind == "ready" { authenticated = true; }
                            let connected = value["peer"]==true || value["connected"]==true;
                            if !connected {
                                epoch = None;
                                release_broker(&mut broker_writer, &mut broker_reader, &mut events);
                            }
                            // A peer that has just connected opens the handshake itself.
                        },
                        Some("data") => {
                            let sequence = secure::envelope_sequence(&value)?;
                            if sequence == 0 {
                                // A new epoch: the browser's nonce plus a fresh one of ours. Whatever the
                                // previous epoch had running is cancelled, so nothing crosses over.
                                let (browser_nonce, echo) = secure::open_hello(key, "browser", &value)?;
                                if echo.is_some() { return Err(End::retry("the browser answered a hello instead of starting one")); }
                                release_broker(&mut broker_writer, &mut broker_reader, &mut events);
                                let current = secure::Epoch { helper: secure::fresh_nonce()?, browser: browser_nonce };
                                (epoch, sent, received) = (Some(current), 0, 0);
                                let mut stream = client::connect(root).await?;
                                wire::write_frame(&mut stream,&json!({"version":PROTOCOL_VERSION,"app":app,"token":token})).await?;
                                let hello = wire::read_frame(&mut stream).await?;
                                if hello["type"]=="busy" {return Err(End::retry("broker is at its connection limit"));}
                                if hello["type"]!="ready" {return Err(End::retry("broker refused app"));}
                                let (mut reader,writer) = tokio::io::split(stream);
                                broker_writer = Some(writer);
                                let output = output.clone();
                                broker_reader = Some(tokio::spawn(async move {
                                    loop {match wire::read_frame(&mut reader).await {
                                        Ok(event) => {if output.send(event).await.is_err() {break;}},
                                        Err(_) => {let _ = output.send(Value::Null).await;break;},
                                    }}
                                }));
                                let reply = secure::seal_hello(key, "helper", &current.helper, Some(&current.browser))?;
                                socket.send(Message::Text(reply.to_string().into())).await.map_err(io::Error::other)?;
                            } else {
                                let Some(current) = epoch.as_ref() else {
                                    return Err(End::retry("data arrived before a handshake; the peer may speak an older protocol"));
                                };
                                // Exactly the next frame. A gap is a lost frame, a repeat is a replay:
                                // either way the connection ends and the next handshake starts clean.
                                if sequence != received + 1 {
                                    return Err(End::retry(format!("frame {sequence} arrived after {received}: a frame was lost, repeated or replayed")));
                                }
                                let request = secure::open_frame(key, "browser", current, &value)?;
                                received = sequence;
                                if let Some(writer) = &mut broker_writer {wire::write_frame(writer,&request).await?;}
                            }
                        },
                        Some("pong") => {},
                        _ => return Err(End::retry("invalid relay frame")),
                    }
                },
            }
        }
    }.await;
    if let Some(reader) = broker_reader {
        reader.abort();
    }
    drop(broker_writer);
    result
}

/// How long the one-shot page that hands the pairing link to the browser lives.
const LAUNCH_PAGE_LIFETIME: Duration = Duration::from_secs(60);

fn attribute_escaped(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Writes a private one-shot page that forwards the browser to the pairing
/// link, and returns its path. The link itself stays out of every command line.
fn prepare_launch(root: &Path, url: &str) -> io::Result<PathBuf> {
    // Pages left by a helper that was killed before it could remove them.
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("pairing-") && name.ends_with(".html") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let page = root.join(format!("pairing-{}.html", &config::random_token()?[..16]));
    let url = attribute_escaped(url);
    config::write_private(
        &page,
        format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Seatline pairing</title>\
             <meta http-equiv=\"refresh\" content=\"0;url={url}\">\
             <p>Opening the pairing link\u{2026}</p>"
        )
        .as_bytes(),
    )?;
    Ok(page)
}

#[allow(clippy::disallowed_methods)] // Opens a private local page with a fixed OS launcher; no shell.
fn open_website(page: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let path =
            std::env::var_os("WINDIR").ok_or_else(|| error("Windows directory unavailable"))?;
        let mut command = std::process::Command::new(
            std::path::PathBuf::from(path).join("System32/rundll32.exe"),
        );
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("/usr/bin/open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("/usr/bin/xdg-open");
    command
        .arg(page)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;
    use tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

    fn quick() -> Timing {
        Timing {
            connect: Duration::from_secs(2),
            ping_every: Duration::from_millis(40),
            dead_after: Duration::from_millis(250),
            backoff_start: Duration::from_millis(50),
            backoff_max: Duration::from_millis(400),
            stable_after: Duration::from_secs(30),
        }
    }

    fn app_root() -> (PathBuf, String) {
        let root = std::env::temp_dir().join(format!(
            "seatline-web-{}",
            &config::random_token().unwrap()[..12]
        ));
        let grant = config::grant_from_args("test_app", "codex", &[]).unwrap();
        config::write_private(
            &config::app_path(&root, "test_app").unwrap(),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        (root, grant.token)
    }

    fn key() -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&[7; 32]).unwrap()
    }

    /// A relay that accepts WebSockets and lets `behavior` play one connection.
    async fn relay<F, Fut>(behavior: F) -> (String, Arc<AtomicUsize>)
    where
        F: Fn(WebSocketStream<TcpStream>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/channels/x/helper", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        let behavior = Arc::new(behavior);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let behavior = behavior.clone();
                tokio::spawn(async move {
                    if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                        behavior(socket).await;
                    }
                });
            }
        });
        (endpoint, connections)
    }

    async fn expect_auth(socket: &mut WebSocketStream<TcpStream>) {
        let Some(Ok(Message::Text(text))) = socket.next().await else {
            panic!("the helper must authenticate first");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["type"],
            "auth"
        );
    }

    async fn ready(socket: &mut WebSocketStream<TcpStream>) {
        expect_auth(socket).await;
        socket
            .send(Message::Text(
                json!({"type":"ready","peer":false}).to_string().into(),
            ))
            .await
            .unwrap();
    }

    async fn run(
        root: &Path,
        token: &str,
        endpoint: &str,
        limit: Duration,
    ) -> Option<io::Result<()>> {
        tokio::time::timeout(
            limit,
            run_channel(root, "test_app", token, endpoint, "h", &key(), quick()),
        )
        .await
        .ok()
    }

    #[tokio::test]
    async fn the_relay_connection_does_not_hold_small_frames_back_for_acknowledgements() {
        let (endpoint, _) = relay(|mut socket| async move {
            // Keep the connection open until the helper's side is dropped.
            while socket.next().await.is_some() {}
        })
        .await;
        let (socket, _) = connect_relay(&endpoint).await.unwrap();
        let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = socket.get_ref() else {
            panic!("a ws:// endpoint is plain TCP");
        };
        assert!(
            tcp.nodelay().unwrap(),
            "streamed events would wait for the relay's delayed acknowledgements"
        );
    }

    #[tokio::test]
    async fn an_expired_pairing_stops_the_helper_instead_of_retrying() {
        let (root, token) = app_root();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/channels/x/helper", listener.local_addr().unwrap());
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0; 2048];
                let _ = stream.read(&mut buffer).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 410 Gone\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let result = run(&root, &token, &endpoint, Duration::from_secs(3))
            .await
            .expect("the helper must stop, not keep retrying");
        assert!(result.unwrap_err().to_string().contains("expired"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_policy_close_from_the_relay_is_final() {
        let (root, token) = app_root();
        let (endpoint, connections) = relay(|mut socket| async move {
            expect_auth(&mut socket).await;
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "Pairing expired".into(),
                })))
                .await;
            while socket.next().await.is_some() {}
        })
        .await;
        let result = run(&root, &token, &endpoint, Duration::from_secs(3))
            .await
            .expect("the helper must stop");
        assert!(result.unwrap_err().to_string().contains("Pairing expired"));
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_silent_relay_is_detected_and_the_helper_reconnects() {
        let (root, token) = app_root();
        let pings = Arc::new(AtomicUsize::new(0));
        let seen = pings.clone();
        let (endpoint, connections) = relay(move |mut socket| {
            let seen = seen.clone();
            async move {
                ready(&mut socket).await;
                // Never answers: the link looks open but is dead.
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Text(text) = message {
                        if text.contains("ping") {
                            seen.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }
        })
        .await;
        assert!(
            run(&root, &token, &endpoint, Duration::from_millis(1800))
                .await
                .is_none(),
            "a silent relay is a reason to retry, not to give up"
        );
        assert!(pings.load(Ordering::SeqCst) >= 1, "no heartbeat was sent");
        assert!(connections.load(Ordering::SeqCst) >= 2, "no reconnect");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_relay_that_answers_keeps_its_connection() {
        let (root, token) = app_root();
        let (endpoint, connections) = relay(|mut socket| async move {
            ready(&mut socket).await;
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(text) = message {
                    if text.contains("ping") {
                        let _ = socket
                            .send(Message::Text(json!({"type":"pong"}).to_string().into()))
                            .await;
                    }
                }
            }
        })
        .await;
        assert!(
            run(&root, &token, &endpoint, Duration::from_millis(900))
                .await
                .is_none()
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reconnects_back_off_instead_of_hammering_the_relay() {
        let (root, token) = app_root();
        let (endpoint, connections) = relay(|mut socket| async move {
            ready(&mut socket).await;
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Error,
                    reason: "Connection failed".into(),
                })))
                .await;
            while socket.next().await.is_some() {}
        })
        .await;
        assert!(
            run(&root, &token, &endpoint, Duration::from_millis(1000))
                .await
                .is_none()
        );
        let attempts = connections.load(Ordering::SeqCst);
        // 50 ms between attempts would allow about 20; doubling gives a handful.
        assert!((2..=8).contains(&attempts), "{attempts} attempts in 1s");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn revoking_the_app_ends_the_helper_with_a_clear_message() {
        let (root, token) = app_root();
        let (endpoint, _) = relay(|mut socket| async move {
            ready(&mut socket).await;
            while socket.next().await.is_some() {}
        })
        .await;
        let path = config::app_path(&root, "test_app").unwrap();
        let revoke = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::fs::remove_file(path).unwrap();
        });
        let result = run(&root, &token, &endpoint, Duration::from_secs(4))
            .await
            .expect("revocation must end the helper");
        revoke.await.unwrap();
        assert_eq!(result.unwrap_err().to_string(), "app authorization revoked");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reauthorizing_the_app_tells_the_user_to_pair_again() {
        let (root, token) = app_root();
        let (endpoint, _) = relay(|mut socket| async move {
            ready(&mut socket).await;
            while socket.next().await.is_some() {}
        })
        .await;
        let replacement = config::grant_from_args("test_app", "codex", &[]).unwrap();
        config::write_private(
            &config::app_path(&root, "test_app").unwrap(),
            &serde_json::to_vec(&replacement).unwrap(),
        )
        .unwrap();
        let result = run(&root, &token, &endpoint, Duration::from_secs(4))
            .await
            .expect("a rotated credential must end the helper");
        assert!(result.unwrap_err().to_string().contains("run `pair` again"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_launch_page_carries_the_link_but_its_path_does_not() {
        let root = std::env::temp_dir().join(format!(
            "seatline-launch-{}",
            &config::random_token().unwrap()[..12]
        ));
        let stale = root.join("pairing-stale.html");
        config::write_private(&stale, b"old").unwrap();
        let url = "https://app.example.com/#seatline=id:browser:key";
        let page = prepare_launch(&root, url).unwrap();
        assert!(!stale.exists(), "leftover pages are swept");
        assert!(!page.to_string_lossy().contains("seatline="));
        assert!(!page.to_string_lossy().contains("browser"));
        let html = std::fs::read_to_string(&page).unwrap();
        assert!(html.contains(url));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&page).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_launch_page_cannot_be_broken_out_of() {
        assert_eq!(
            attribute_escaped("https://a/?x=\"><script>'&"),
            "https://a/?x=&quot;&gt;&lt;script&gt;&#39;&amp;"
        );
    }
}
