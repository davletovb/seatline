//! Client adapters. Each exchange has one authenticated IPC connection;
//! disconnecting it cancels only that exchange, never another app's work.
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use interprocess::local_socket::{GenericFilePath, GenericNamespaced, tokio::{prelude::*, Stream}};
use seatline_core::exchange::{Exchange, Scripted, Timeouts, Update};
use seatline_core::protocol::{Capabilities, ErrorCode};
use seatline_core::turn::Turn;
use seatline_providers::{Cleanup, Provider};
use serde_json::{Value, json};

use crate::{PROTOCOL_VERSION, config, wire};

pub fn socket_name(root: &Path) -> io::Result<interprocess::local_socket::Name<'static>> {
    let endpoint = config::endpoint(root)?;
    if cfg!(windows) { endpoint.to_ns_name::<GenericNamespaced>() }
    else { endpoint.to_fs_name::<GenericFilePath>() }
}

pub async fn connect(root: &Path) -> io::Result<Stream> {
    let name = socket_name(root)?;
    if let Ok(stream) = Stream::connect(name).await { return Ok(stream); }
    let executable = std::env::var_os("SEATLINE_COMPANION_BIN").map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?.with_file_name(if cfg!(windows) {
            "seatline-companion.exe"
        } else { "seatline-companion" }));
    Command::new(executable).arg("serve").stdin(Stdio::null())
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(stream) = Stream::connect(socket_name(root)?).await { return Ok(stream); }
    }
    Err(io::Error::other("Seatline companion did not start"))
}

pub struct RemoteProvider {
    app: String,
    id: String,
    capabilities: Capabilities,
    timeouts: Timeouts,
    persistent: bool,
}

impl RemoteProvider {
    pub fn new(app: &str, metadata: &dyn Provider) -> Self {
        Self { app: app.to_owned(), id: metadata.id().to_owned(),
            capabilities: metadata.capabilities(), timeouts: metadata.timeouts(),
            persistent: metadata.supports_persistent_session() }
    }
    fn request(&self, method: &str, params: Value) -> Box<dyn Exchange> {
        RemoteExchange::start(&self.app, json!({"id":"request", "method":method,
            "provider":self.id,"params":params}))
    }
}

impl Provider for RemoteProvider {
    fn id(&self) -> &str { &self.id }
    fn timeouts(&self) -> Timeouts { self.timeouts }
    fn capabilities(&self) -> Capabilities { self.capabilities }
    fn supports_persistent_session(&self) -> bool { self.persistent }
    fn status(&self) -> Box<dyn Exchange> { self.request("status", Value::Null) }
    fn send(&self, turn: Turn) -> Box<dyn Exchange> { self.request("send", json!(turn)) }
    fn cleanup_sessions(&self, sessions: &[String]) -> Cleanup {
        let (app, provider, sessions) = (self.app.clone(), self.id.clone(), sessions.to_vec());
        Cleanup::new(move || {
            let mut exchange = RemoteExchange::start(&app, json!({"id":"request",
                "method":"forget","provider":provider,"params":{"sessions":sessions}}));
            let deadline = Instant::now() + Duration::from_secs(30);
            loop { match exchange.next(deadline) {
                Some(Update::Completed) => return Ok(()),
                Some(Update::Failed(_)) | Some(Update::Stopped) | None => return Err(io::Error::other("Seatline cleanup failed")),
                _ => {}
            }}
        }, || {})
    }
    fn cleanup_group(&self, group: &str) -> Cleanup {
        let (app, provider, group) = (self.app.clone(), self.id.clone(), group.to_owned());
        Cleanup::new(move || {
            let mut exchange = RemoteExchange::start(&app, json!({"id":"request",
                "method":"cleanup","provider":provider,"params":{"group":group}}));
            let deadline = Instant::now() + Duration::from_secs(30);
            loop { match exchange.next(deadline) {
                Some(Update::Completed) => return Ok(()),
                Some(Update::Failed(_)) | Some(Update::Stopped) | None => return Err(io::Error::other("Seatline cleanup failed")),
                _ => {}
            }}
        }, || {})
    }
}

struct RemoteExchange {
    events: Receiver<Update>,
    cancel: tokio::sync::mpsc::Sender<()>,
    ended: bool,
}

impl RemoteExchange {
    fn start(app: &str, request: Value) -> Box<dyn Exchange> {
        let setup = (|| {
            let root = config::data_dir()?;
            let grant = config::load_grant(&root, app)?;
            Ok::<_, io::Error>((root, grant))
        })();
        let Ok((root, grant)) = setup else {
            return Box::new(Scripted::new([Update::Failed(wire::failure(
                ErrorCode::InvalidRequest, "APP_NOT_AUTHORIZED", false))]));
        };
        let (send, events) = mpsc::sync_channel(64);
        let (cancel, mut cancellations) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(async {
                    let mut stream = connect(&root).await?;
                    wire::write_frame(&mut stream, &json!({"version":PROTOCOL_VERSION,
                        "app":grant.app,"token":grant.token})).await?;
                    let hello = wire::read_frame(&mut stream).await?;
                    if hello["type"] != "ready" { return Err(io::Error::other("authorization refused")); }
                    wire::write_frame(&mut stream, &request).await?;
                    let (mut reader, mut writer) = tokio::io::split(stream);
                    let cancellation = tokio::spawn(async move {
                        if cancellations.recv().await.is_some() {
                            let _ = wire::write_frame(&mut writer, &json!({"id":"cancel", "method":"cancel", "target":"request"})).await;
                        }
                    });
                    // The reader is never cancelled mid-frame by a select.
                    let result = async {
                        loop {
                            let event = wire::read_frame(&mut reader).await?;
                            if event["id"] != "request" { continue; }
                            let update = wire::decode_update(event["event"].clone())?;
                            let terminal = update.is_terminal();
                            send.send(update).map_err(|_| io::Error::other("client closed"))?;
                            if terminal { return Ok::<_, io::Error>(()); }
                        }
                    }.await;
                    cancellation.abort();
                    result
                })
            })();
            if result.is_err() { let _ = send.send(Update::Failed(wire::failure(
                ErrorCode::ProviderFailed, "COMPANION_DISCONNECTED", true))); }
        });
        Box::new(Self { events, cancel, ended:false })
    }
}

impl Exchange for RemoteExchange {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if self.ended { return None; }
        match self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(update) => { self.ended = update.is_terminal(); Some(update) },
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => { self.ended = true;
                Some(Update::Failed(wire::failure(ErrorCode::ProviderFailed,"COMPANION_DISCONNECTED",true))) }
        }
    }
    fn cancel(&mut self, _: Duration) { let _ = self.cancel.try_send(()); }
}

impl Drop for RemoteExchange {
    fn drop(&mut self) { if !self.ended { let _ = self.cancel.try_send(()); } }
}
