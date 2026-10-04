//! Client adapters. Each exchange of a [`RemoteProvider`] made with
//! [`RemoteProvider::new`] has one authenticated IPC connection of its own;
//! disconnecting it cancels only that exchange, never another app's work. One
//! made with [`RemoteProvider::with_client`] shares its app's
//! [`RemoteClient`]: one runtime and one connection for all of its requests.
use std::io;
use std::path::Path;
use std::sync::{
    Arc,
    mpsc::{self, Receiver},
};
use std::time::{Duration, Instant};

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced,
    tokio::{Stream, prelude::*},
};
use seatline_core::exchange::{Exchange, Scripted, Timeouts, Update};
use seatline_core::protocol::{Capabilities, ErrorCode};
use seatline_core::turn::Turn;
use seatline_providers::{Cleanup, Provider};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::remote::RemoteClient;
use crate::startup::{self, Startup};
use crate::{PROTOCOL_VERSION, config, wire};

pub fn socket_name(root: &Path) -> io::Result<interprocess::local_socket::Name<'static>> {
    let endpoint = config::endpoint(root)?;
    if cfg!(windows) {
        endpoint.to_ns_name::<GenericNamespaced>()
    } else {
        endpoint.to_fs_name::<GenericFilePath>()
    }
}

/// Connects to the broker, starting the companion if none is running and no
/// other client is already starting one; see [`crate::startup`].
pub async fn connect(root: &Path) -> io::Result<Stream> {
    connect_with(root, &Startup::default()).await
}

/// [`connect`] with the startup settings given.
pub async fn connect_with(root: &Path, startup: &Startup) -> io::Result<Stream> {
    startup::start_or_wait(
        startup,
        || async { Stream::connect(socket_name(root)?).await },
        || startup::claim(root),
        || startup::start_companion(root, startup),
        startup::exited,
    )
    .await
}

pub struct RemoteProvider {
    app: String,
    id: String,
    capabilities: Capabilities,
    timeouts: Timeouts,
    persistent: bool,
    /// The app's shared connection, when it has one; without it each exchange
    /// opens a connection of its own.
    client: Option<RemoteClient>,
    preparation: bool,
}

impl RemoteProvider {
    /// A provider whose every exchange connects and authenticates for itself.
    /// This is the compatibility path; an app that makes many requests wants
    /// [`RemoteProvider::with_client`].
    pub fn new(app: &str, metadata: &dyn Provider) -> Self {
        Self {
            app: app.to_owned(),
            id: metadata.id().to_owned(),
            capabilities: metadata.capabilities(),
            timeouts: metadata.timeouts(),
            persistent: metadata.supports_persistent_session(),
            client: None,
            preparation: metadata.supports_preparation(),
        }
    }

    /// A provider whose exchanges are requests on `client`, the app's shared
    /// connection, which belongs to the same app the grant names.
    pub fn with_client(app: &str, client: RemoteClient, metadata: &dyn Provider) -> Self {
        Self {
            client: Some(client),
            ..Self::new(app, metadata)
        }
    }

    fn requester(&self) -> Requester {
        Requester {
            app: self.app.clone(),
            provider: self.id.clone(),
            client: self.client.clone(),
        }
    }

    fn request(&self, method: &str, params: Value) -> Box<dyn Exchange> {
        self.requester().request(method, params)
    }
}

impl Provider for RemoteProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn timeouts(&self) -> Timeouts {
        self.timeouts
    }
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    fn supports_persistent_session(&self) -> bool {
        self.persistent
    }
    fn status(&self) -> Box<dyn Exchange> {
        self.request("status", Value::Null)
    }
    fn supports_preparation(&self) -> bool {
        self.preparation
    }
    fn readiness(&self, freshness: seatline_core::readiness::Freshness) -> Box<dyn Exchange> {
        self.request("readiness", json!(freshness))
    }
    fn prepare(&self, freshness: seatline_core::readiness::Freshness) -> Box<dyn Exchange> {
        self.request("prepare", json!(freshness))
    }
    fn send_with_readiness(
        &self,
        turn: Turn,
        freshness: seatline_core::readiness::Freshness,
    ) -> Box<dyn Exchange> {
        self.request("send_ready", json!({"turn":turn,"freshness":freshness}))
    }
    fn send(&self, turn: Turn) -> Box<dyn Exchange> {
        self.request("send", json!(turn))
    }
    fn cleanup_sessions(&self, sessions: &[String]) -> Cleanup {
        let (requester, sessions) = (self.requester(), sessions.to_vec());
        Cleanup::new(
            move || run_cleanup(&requester, "forget", json!({"sessions":sessions})),
            || {},
        )
    }
    fn cleanup_group(&self, group: &str) -> Cleanup {
        let (requester, group) = (self.requester(), group.to_owned());
        Cleanup::new(
            move || run_cleanup(&requester, "cleanup", json!({"group":group})),
            || {},
        )
    }
}

/// What makes one provider's requests, for work that must own what it needs.
struct Requester {
    app: String,
    provider: String,
    client: Option<RemoteClient>,
}

impl Requester {
    fn request(&self, method: &str, params: Value) -> Box<dyn Exchange> {
        match &self.client {
            Some(client) => client.request(method, &self.provider, params),
            None => RemoteExchange::start(
                &self.app,
                json!({"id":"request", "method":method,
            "provider":self.provider,"params":params}),
            ),
        }
    }
}

/// Runs a cleanup request to its end: done once it completed, failed otherwise.
fn run_cleanup(requester: &Requester, method: &str, params: Value) -> io::Result<()> {
    let mut exchange = requester.request(method, params);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match exchange.next(deadline) {
            Some(Update::Completed) => return Ok(()),
            Some(Update::Failed(_)) | Some(Update::Stopped) | None => {
                return Err(io::Error::other("Seatline cleanup failed"));
            }
            _ => {}
        }
    }
}

struct RemoteExchange {
    events: Receiver<BufferedUpdate>,
    cancel: tokio::sync::mpsc::Sender<()>,
    ended: bool,
}

// Each nonterminal event owns one buffer slot until the synchronous consumer
// takes it. Waiting for a slot is asynchronous; std's unbounded send itself
// never blocks the runtime. One terminal event can use a reserved extra slot.
type BufferedUpdate = (Update, Option<OwnedSemaphorePermit>);
const EVENT_CAPACITY: usize = 64;

async fn forward_updates<S>(
    stream: S,
    send: &mpsc::Sender<BufferedUpdate>,
    capacity: Arc<Semaphore>,
    mut cancellations: tokio::sync::mpsc::Receiver<()>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let cancellation = tokio::spawn(async move {
        if cancellations.recv().await.is_some() {
            let _ = wire::write_frame(
                &mut writer,
                &json!({"id":"cancel", "method":"cancel", "target":"request"}),
            )
            .await;
        }
    });
    // The reader is never cancelled mid-frame by a select.
    let result = async {
        loop {
            let event = wire::read_frame(&mut reader).await?;
            if event["id"] != "request" {
                continue;
            }
            let update = wire::decode_update(event["event"].clone())?;
            let terminal = update.is_terminal();
            let slot = if terminal {
                None
            } else {
                Some(
                    capacity
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(io::Error::other)?,
                )
            };
            send.send((update, slot))
                .map_err(|_| io::Error::other("client closed"))?;
            if terminal {
                return Ok::<_, io::Error>(());
            }
        }
    }
    .await;
    cancellation.abort();
    result
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
                ErrorCode::InvalidRequest,
                "APP_NOT_AUTHORIZED",
                false,
            ))]));
        };
        let (send, events) = mpsc::channel();
        let capacity = Arc::new(Semaphore::new(EVENT_CAPACITY));
        let (cancel, cancellations) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    let mut stream = connect(&root).await?;
                    wire::write_frame(
                        &mut stream,
                        &json!({"version":PROTOCOL_VERSION,
                        "app":grant.app,"token":grant.token}),
                    )
                    .await?;
                    let hello = wire::read_frame(&mut stream).await?;
                    if hello["type"] == "busy" {
                        let _ = send.send((
                            Update::Failed(wire::failure(
                                ErrorCode::ProviderFailed,
                                wire::reason::QUEUE_FULL,
                                true,
                            )),
                            None,
                        ));
                        return Ok(());
                    }
                    if hello["type"] != "ready" {
                        return Err(io::Error::other("authorization refused"));
                    }
                    wire::write_frame(&mut stream, &request).await?;
                    forward_updates(stream, &send, capacity, cancellations).await
                })
            })();
            if result.is_err() {
                let _ = send.send((
                    Update::Failed(wire::failure(
                        ErrorCode::ProviderFailed,
                        "COMPANION_DISCONNECTED",
                        true,
                    )),
                    None,
                ));
            }
        });
        Box::new(Self {
            events,
            cancel,
            ended: false,
        })
    }
}

impl Exchange for RemoteExchange {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if self.ended {
            return None;
        }
        match self
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok((update, _slot)) => {
                self.ended = update.is_terminal();
                Some(update)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.ended = true;
                Some(Update::Failed(wire::failure(
                    ErrorCode::ProviderFailed,
                    "COMPANION_DISCONNECTED",
                    true,
                )))
            }
        }
    }
    fn cancel(&mut self, _: Duration) {
        let _ = self.cancel.try_send(());
    }
}

impl Drop for RemoteExchange {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.cancel.try_send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn a_full_event_buffer_still_allows_cancellation_and_receiver_drop() {
        let (stream, mut broker) = tokio::io::duplex(16 * 1024);
        let (send, events) = mpsc::channel();
        let capacity = Arc::new(Semaphore::new(EVENT_CAPACITY));
        let worker_capacity = capacity.clone();
        let (cancel, cancellations) = tokio::sync::mpsc::channel(1);
        let mut exchange = RemoteExchange {
            events,
            cancel,
            ended: false,
        };
        let worker = tokio::spawn(async move {
            forward_updates(stream, &send, worker_capacity, cancellations).await
        });
        for _ in 0..=EVENT_CAPACITY {
            wire::write_frame(
                &mut broker,
                &json!({
                    "id":"request", "event":{"type":"delta", "text":"x"}
                }),
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while capacity.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the event buffer never filled");
        exchange.cancel(Duration::ZERO);
        let frame = tokio::time::timeout(Duration::from_secs(2), wire::read_frame(&mut broker))
            .await
            .expect("backpressure blocked cancellation")
            .unwrap();
        assert_eq!(frame["method"], "cancel");
        assert_eq!(frame["target"], "request");
        assert_eq!(
            capacity.available_permits(),
            0,
            "consumer has not drained output"
        );
        drop(exchange);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .expect("receiver drop stranded the IPC worker")
                .unwrap()
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2), wire::read_frame(&mut broker))
                .await
                .unwrap()
                .is_err(),
            "dropping the exchange must close its connection"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_terminal_event_preserves_order_with_a_full_buffer() {
        let (stream, mut broker) = tokio::io::duplex(16 * 1024);
        let (send, events) = mpsc::channel();
        let capacity = Arc::new(Semaphore::new(EVENT_CAPACITY));
        let (_cancel, cancellations) = tokio::sync::mpsc::channel(1);
        let worker =
            tokio::spawn(
                async move { forward_updates(stream, &send, capacity, cancellations).await },
            );
        for index in 0..EVENT_CAPACITY {
            wire::write_frame(
                &mut broker,
                &json!({
                    "id":"request", "event":{"type":"delta", "text":index.to_string()}
                }),
            )
            .await
            .unwrap();
        }
        wire::write_frame(
            &mut broker,
            &json!({
                "id":"request", "event":{"type":"completed"}
            }),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("terminal delivery required consumer capacity")
            .unwrap()
            .unwrap();
        for index in 0..EVENT_CAPACITY {
            assert_eq!(
                events.try_recv().unwrap().0,
                Update::Delta(index.to_string())
            );
        }
        assert_eq!(events.try_recv().unwrap().0, Update::Completed);
        assert!(events.try_recv().is_err());
    }
}
