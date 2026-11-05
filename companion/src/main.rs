use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Stream, prelude::*},
};
use seatline_companion::{
    PROTOCOL_VERSION, client,
    config::{self, Grant, NativeAdapter},
    control::{self, Outcome},
    hub, telemetry, wire,
};
use seatline_core::telemetry::Sink;
use serde_json::{Value, json};

fn main() {
    if let Err(error) = run() {
        eprintln!("Seatline: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::disallowed_methods)] // Launches only native adapters registered by the local administrator, with inherited stdio.
fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = config::data_dir()?;
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!(
                "Seatline companion {} (protocol {PROTOCOL_VERSION})",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some("serve") if args.len() == 1 => {
            let _lock = match config::lock(&root, "broker.lock") {
                Ok(lock) => lock,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            #[cfg(unix)]
            if root.join("broker.sock").exists() {
                std::fs::remove_file(root.join("broker.sock"))?;
            }
            let telemetry = telemetry::from_env();
            let (hub, hub_thread) = hub::start_joinable(root.clone(), telemetry.clone())?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(serve(root, hub, telemetry));
            // Dropping the runtime drops the connections and with them the last senders; the hub then
            // ends whatever is still running (providers are stopped and reaped) before this process exits.
            drop(runtime);
            let _ = hub_thread.join();
            result
        }
        Some("pair") if args.len() == 4 || (args.len() == 5 && args[4] == "--open") => {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(seatline_companion::web::pair(
                    &root,
                    &args[1],
                    &args[2],
                    &args[3],
                    args.len() == 5,
                ))
        }
        Some("web") if args.len() == 1 || (args.len() == 2 && args[1] == "--detach") => {
            // Started at login by Windows, which gives it a console: start again without one.
            #[cfg(windows)]
            if args.len() == 2 {
                return seatline_companion::autostart::detach();
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(seatline_companion::links::serve(root))
        }
        Some("link") if args.len() >= 2 => {
            let (mut origin, mut relay, mut open) = (None, None, false);
            for arg in &args[2..] {
                if let Some(value) = arg.strip_prefix("--relay=") {
                    relay = Some(value);
                } else if arg == "--open" {
                    open = true;
                } else if arg.starts_with("https://") && origin.is_none() {
                    origin = Some(arg.as_str());
                } else {
                    return Err(io::Error::other(format!("unrecognized argument `{arg}`")));
                }
            }
            let link = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(seatline_companion::links::link(
                    &root, &args[1], origin, relay,
                ))?;
            println!(
                "Linked a browser to {}. Open this private link in the browser you use it in (or paste it into the site):\n{}",
                link.origin,
                link.url()
            );
            if !seatline_companion::links::service_running(&root) {
                eprintln!(
                    "Seatline: the web service is not running, so the website cannot reach Seatline yet. Start it with `seatline-companion web`, or `seatline-companion autostart on` to start it at login."
                );
            }
            if open {
                // The browser reads the one-shot page at once; it is removed soon after.
                let page = seatline_companion::web::open_privately(&root, &link.url())?;
                std::thread::sleep(std::time::Duration::from_secs(5));
                let _ = std::fs::remove_file(page);
            }
            Ok(())
        }
        Some("unlink") if args.len() == 2 => {
            let ended = seatline_companion::links::unlink(&root, &args[1])?;
            println!(
                "Ended {ended} link(s) of {}; those browsers must be linked again.",
                args[1]
            );
            Ok(())
        }
        Some("autostart") if args.len() == 2 && matches!(args[1].as_str(), "on" | "off") => {
            if args[1] == "off" {
                if seatline_companion::autostart::disable()? {
                    println!("The Seatline web service no longer starts at login.");
                } else {
                    println!("The Seatline web service was not set to start at login.");
                }
                return Ok(());
            }
            let executable = seatline_companion::install::executable(&root)?
                .ok_or_else(|| io::Error::other("run `seatline-companion install` first"))?;
            let registration = seatline_companion::autostart::enable(&executable, false)?;
            println!(
                "The Seatline web service starts at login ({}).",
                registration.display()
            );
            Ok(())
        }
        Some("connect") if args.len() == 2 => {
            let grant = config::load_grant(&root, &args[1])?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(bridge(root, grant))
        }
        Some("authorize") if args.len() >= 3 => {
            let _lock = config::lock(&root, "registry.lock")?;
            // Explicit local authorization rotates credentials, cancelling old connections.
            let grant = config::grant_from_args(&args[1], &args[2], &args[3..])?;
            config::write_private(
                &config::app_path(&root, &grant.app)?,
                &serde_json::to_vec_pretty(&grant)?,
            )?;
            if seatline_companion::install::executable(&root)?.is_some() {
                seatline_companion::install::register(&root)?;
            }
            println!(
                "Authorized {} for {}: {} extension origin(s), {} website origin(s), {} relay origin(s); provider-default tools {}.",
                grant.app,
                grant.providers.join(","),
                grant.extension_origins.len(),
                grant.web_origins.len(),
                grant.web_relays.len(),
                if grant.allow_provider_default {
                    "allowed"
                } else {
                    "denied"
                },
            );
            report_autostart(seatline_companion::autostart::sync(&root, false));
            Ok(())
        }
        Some("register-native") if args.len() >= 3 => {
            let _lock = config::lock(&root, "registry.lock")?;
            let mut grant = config::load_grant(&root, &args[1])?;
            let executable = std::fs::canonicalize(&args[2])?;
            if !executable.is_file() {
                return Err(io::Error::other("native adapter executable missing"));
            }
            grant.native_adapter = Some(NativeAdapter {
                executable,
                args: args[3..].to_vec(),
            });
            config::write_private(
                &config::app_path(&root, &grant.app)?,
                &serde_json::to_vec_pretty(&grant)?,
            )?;
            if seatline_companion::install::executable(&root)?.is_some() {
                seatline_companion::install::register(&root)?;
            }
            Ok(())
        }
        Some("revoke") if args.len() == 2 => {
            let _lock = config::lock(&root, "registry.lock")?;
            std::fs::remove_file(config::app_path(&root, &args[1])?)?;
            if seatline_companion::install::executable(&root)?.is_some() {
                seatline_companion::install::register(&root)?;
            }
            report_autostart(seatline_companion::autostart::sync(&root, false));
            Ok(())
        }
        Some("install") if args.len() == 1 => {
            println!(
                "Seatline installed at {}",
                seatline_companion::install::install(&root)?.display()
            );
            // A broker that is already running is an older copy and would keep serving it for as long
            // as any app stays connected: end it, so that the next use starts the copy just installed.
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(control::stop(&root))
            {
                Ok(Outcome::NotRunning) => {}
                Ok(Outcome::Stopped) => {
                    println!(
                        "Stopped the Seatline that was running; the next use starts this copy."
                    );
                }
                Ok(Outcome::Unanswered) => eprintln!("Seatline: {}", control::UNANSWERED),
                Ok(Outcome::Lingering) => eprintln!(
                    "Seatline: the Seatline that was running has not finished stopping; run `seatline-companion stop` to wait for it."
                ),
                Err(error) => eprintln!("Seatline: could not stop the running Seatline: {error}"),
            }
            // The web service that was running is an older copy too: start this one in its place.
            report_autostart(seatline_companion::autostart::sync(&root, true));
            Ok(())
        }
        Some("stop") if args.len() == 1 => {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(control::stop(&root))?
            {
                Outcome::NotRunning => println!("Seatline is not running."),
                Outcome::Stopped => {
                    println!("Seatline stopped. The next use starts the installed copy.")
                }
                Outcome::Unanswered => return Err(io::Error::other(control::UNANSWERED)),
                Outcome::Lingering => {
                    return Err(io::Error::other(
                        "Seatline was asked to stop but has not finished; run `seatline-companion stop` again to wait for it",
                    ));
                }
            }
            Ok(())
        }
        Some("manifest") if args.len() == 1 => {
            println!(
                "{}",
                seatline_companion::install::manifest(&root, &std::env::current_exe()?)?
            );
            Ok(())
        }
        Some(origin) if config::valid_extension_origin(origin) && args.len() <= 2 => {
            let mut matches = Vec::new();
            for entry in std::fs::read_dir(root.join("apps"))?.flatten() {
                if let Some(app) = entry.path().file_stem().and_then(|name| name.to_str()) {
                    if let Ok(grant) = config::load_grant(&root, app) {
                        if grant
                            .extension_origins
                            .iter()
                            .any(|allowed| allowed == origin)
                        {
                            matches.push(grant);
                        }
                    }
                }
            }
            if matches.len() != 1 {
                return Err(io::Error::other("extension has no unique authorized app"));
            }
            let grant = matches.remove(0);
            let Some(adapter) = grant.native_adapter.clone() else {
                return tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(bridge(root, grant));
            };
            let status = Command::new(adapter.executable)
                .args(adapter.args)
                .args(&args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("native adapter failed"))
            }
        }
        _ => Err(io::Error::other(
            "usage: seatline-companion install | stop | serve | web | link APP [SITE_ORIGIN] [--relay=RELAY_ORIGIN] [--open] | unlink APP | autostart on|off | connect APP | pair APP RELAY SITE [--open] | authorize APP PROVIDERS [EXTENSION_ORIGIN...] [SITE_ORIGIN...] [--relay=RELAY_ORIGIN] [--cache-title=TITLE] [--allow-provider-default] | register-native APP EXECUTABLE [ARGS...] | revoke APP | manifest",
        )),
    }
}

/// Says what changed about starting the web service at login. It never fails
/// the command that asked: authorizing or installing has already happened.
fn report_autostart(result: io::Result<Option<String>>) {
    match result {
        Ok(Some(message)) => println!("{message}"),
        Ok(None) => {}
        Err(error) => {
            eprintln!("Seatline: could not set the web service to start at login: {error}")
        }
    }
}

const MAX_CONNECTIONS: usize = 32;
/// How long the broker stays up with no connections at all. Exiting when idle
/// means the next start runs the installed binary, but a broker that apps keep
/// connected never gets there: `install` and `stop` end it (see `control`).
/// `0` disables the exit.
const DEFAULT_IDLE_SECONDS: u64 = 600;

fn idle_limit() -> Option<Duration> {
    let seconds = std::env::var("SEATLINE_BROKER_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_IDLE_SECONDS);
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

async fn serve(
    root: PathBuf,
    hub: std::sync::mpsc::SyncSender<hub::Command>,
    telemetry: Option<Arc<dyn Sink>>,
) -> io::Result<()> {
    if let Some(sink) = &telemetry {
        let mut limits = hub::limits_for(&root)?;
        limits.insert("listener_max_connections", MAX_CONNECTIONS as u64);
        limits.insert(
            "idle_exit_ms",
            idle_limit().map_or(0, |idle| idle.as_millis() as u64),
        );
        sink.record(telemetry::broker_record(limits));
    }
    let listener = ListenerOptions::new()
        .name(client::socket_name(&root)?)
        .create_tokio()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("broker.sock"),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    // Published while this broker lives, so `seatline-companion stop` (and `install`) can end it.
    let stopper = Arc::new(Stopper {
        token: control::publish(&root)?,
        requested: tokio::sync::Notify::new(),
    });
    let _published = Published(root.clone());
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let ids = AtomicU64::new(1);
    // Milliseconds since `started` at which a connection last opened or closed.
    let started = Instant::now();
    let last_activity = Arc::new(AtomicU64::new(0));
    let idle = idle_limit();
    let check_every = idle
        .map(|idle| (idle / 4).clamp(Duration::from_millis(50), Duration::from_secs(30)))
        .unwrap_or(Duration::from_secs(3600));
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => accepted?,
            () = stopper.requested.notified() => break,
            () = tokio::time::sleep(check_every) => {
                let quiet = (started.elapsed().as_millis() as u64)
                    .saturating_sub(last_activity.load(Ordering::Relaxed));
                if idle.is_some_and(|idle| {
                    permits.available_permits() == MAX_CONNECTIONS && quiet >= idle.as_millis() as u64
                }) {
                    #[cfg(unix)]
                    let _ = std::fs::remove_file(root.join("broker.sock"));
                    return Ok(());
                }
                continue;
            }
        };
        last_activity.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            // Say why instead of just hanging up, so the client can back off and retry. A request to stop
            // the broker is the one thing that must still get through: a broker whose slots are all taken
            // is as much in need of stopping as any other. It sends its frame at once, so wait only
            // briefly for one before saying busy.
            let stopper = stopper.clone();
            tokio::spawn(async move {
                let mut stream = stream;
                let first =
                    tokio::time::timeout(Duration::from_millis(250), wire::read_frame(&mut stream))
                        .await;
                if let Ok(Ok(frame)) = first {
                    if control::is_stop(&frame, &stopper.token) {
                        let said = tokio::time::timeout(
                            Duration::from_secs(1),
                            wire::write_frame(&mut stream, &json!({"type":"stopping"})),
                        )
                        .await;
                        if said.is_ok_and(|written| written.is_ok()) {
                            stopper.requested.notify_one();
                        }
                        return;
                    }
                }
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    wire::write_frame(&mut stream, &json!({"type":"busy"})),
                )
                .await;
            });
            continue;
        };
        let (root, hub, connection, last_activity, telemetry, stopper) = (
            root.clone(),
            hub.clone(),
            ids.fetch_add(1, Ordering::Relaxed),
            last_activity.clone(),
            telemetry.clone(),
            stopper.clone(),
        );
        tokio::spawn(async move {
            let _permit = permit;
            let _ =
                connection_loop(stream, root, hub.clone(), connection, telemetry, stopper).await;
            // Close must eventually be delivered even when the bounded inbox is full.
            while let Err(std::sync::mpsc::TrySendError::Full(_)) =
                hub.try_send(hub::Command::Close(connection))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            last_activity.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        });
    }
    retire(listener, &root, &hub).await;
    Ok(())
}

/// What a stop request needs: the token that authorizes it, and the way to tell `serve` to leave.
struct Stopper {
    token: String,
    requested: tokio::sync::Notify,
}

/// Withdraws the published control token when the broker leaves, however it leaves.
struct Published(PathBuf);
impl Drop for Published {
    fn drop(&mut self) {
        control::withdraw(&self.0);
    }
}

/// Leaves on request: stops taking connections, then waits until the hub has nothing queued or running,
/// or the grace period is over. What is still running then is ended when the broker exits.
async fn retire(
    listener: interprocess::local_socket::tokio::Listener,
    root: &std::path::Path,
    hub: &std::sync::mpsc::SyncSender<hub::Command>,
) {
    drop(listener);
    #[cfg(unix)]
    let _ = std::fs::remove_file(root.join("broker.sock"));
    #[cfg(not(unix))]
    let _ = root;
    let until = tokio::time::Instant::now() + control::grace();
    while !quiet(hub).await && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Whether the hub has no work. A hub that is gone has none.
async fn quiet(hub: &std::sync::mpsc::SyncSender<hub::Command>) -> bool {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let mut command = hub::Command::Quiet(reply);
    for _ in 0..200 {
        match hub.try_send(command) {
            Ok(()) => {
                return tokio::time::timeout(Duration::from_secs(1), answer)
                    .await
                    .is_ok_and(|answered| answered.unwrap_or(true));
            }
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                command = back;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return true,
        }
    }
    false
}

async fn connection_loop(
    mut stream: Stream,
    root: PathBuf,
    hub: std::sync::mpsc::SyncSender<hub::Command>,
    connection: u64,
    telemetry: Option<Arc<dyn Sink>>,
    stopper: Arc<Stopper>,
) -> io::Result<()> {
    let auth =
        tokio::time::timeout(Duration::from_secs(5), wire::read_frame(&mut stream)).await??;
    // A request to stop the broker takes the place of the app authentication frame.
    if !auth["control"].is_null() {
        if !control::is_stop(&auth, &stopper.token) {
            return Err(io::Error::other("control refused"));
        }
        wire::write_frame(&mut stream, &json!({"type":"stopping"})).await?;
        stopper.requested.notify_one();
        return Ok(());
    }
    // The handshake runs from the authentication frame to `ready` on the wire.
    let handshake_began = telemetry.is_some().then(Instant::now);
    if auth["version"] != PROTOCOL_VERSION {
        return Err(io::Error::other("unsupported protocol"));
    }
    let grant = config::load_grant(
        &root,
        auth["app"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing app"))?,
    )?;
    if !config::same_token(&grant.token, auth["token"].as_str().unwrap_or("")) {
        return Err(io::Error::other("authorization refused"));
    }
    let handshake_app = handshake_began.map(|_| grant.app.clone());
    let (output, mut events) = tokio::sync::mpsc::channel(64);
    hub.try_send(hub::Command::Open {
        connection,
        grant: Box::new(grant),
        output,
    })
    .map_err(|_| io::Error::other("broker busy"))?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let read = async {
        loop {
            let value = wire::read_frame(&mut reader).await?;
            hub.try_send(hub::Command::Request { connection, value })
                .map_err(|_| io::Error::other("broker busy"))?;
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let write = async {
        // The first thing the hub says to a connection is `ready`.
        let mut handshake = telemetry.zip(handshake_began).zip(handshake_app);
        while let Some(event) = events.recv().await {
            // Not a short limit: what waits for a slow client is bounded by size in
            // the hub, and a client such as the web helper takes nothing while its
            // network stalls (see `wire::CLIENT_STALL`).
            wire::write_frame_within(&mut writer, &event, wire::CLIENT_STALL).await?;
            if let Some(((sink, began), app)) = handshake.take() {
                sink.record(telemetry::handshake_record(
                    connection,
                    app,
                    began.elapsed(),
                ));
            }
        }
        Ok::<(), io::Error>(())
    };
    // Cancelling read ends the connection; it is never resumed mid-frame.
    tokio::select! { result = read => result, result = write => result }
}

/// How many of the broker's frames the bridge holds for an extension that has
/// not taken them yet. The broker holds the rest, within its own bounds.
const BRIDGE_QUEUE: usize = 64;

/// Passes the broker's frames on to the extension's queue, in order, waiting
/// while it is full. A turn can print many deltas at once, faster than an
/// extension reads them: that must wait, not end the bridge and with it every
/// request the extension has running. How much may wait for a slow extension
/// the broker bounds (the hub's limit on what waits for a connection). Ends
/// when the broker closes the connection, when the extension is gone, or when
/// it has taken nothing for `stall`: while the bridge waits it cannot see the
/// broker give up on it, so it gives up on the same terms
/// (`wire::CLIENT_STALL`) rather than wait with no end.
async fn pass_to_extension<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    output: &tokio::sync::mpsc::Sender<Value>,
    stall: Duration,
) -> io::Result<()> {
    loop {
        let frame = wire::read_frame(reader).await?;
        tokio::time::timeout(stall, output.send(frame))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the extension stopped reading"))?
            .map_err(|_| io::Error::other("the extension is gone"))?;
    }
}

/// Writes queued frames to the extension, each as its four-byte length and its
/// JSON, until the queue ends or the extension stops reading.
fn write_to_extension(mut frames: tokio::sync::mpsc::Receiver<Value>, out: &mut impl Write) {
    while let Some(value) = frames.blocking_recv() {
        let Ok(bytes) = serde_json::to_vec(&value) else {
            break;
        };
        if out
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .and_then(|()| out.write_all(&bytes))
            .and_then(|()| out.flush())
            .is_err()
        {
            break;
        }
    }
}

async fn bridge(root: PathBuf, grant: Grant) -> io::Result<()> {
    let mut stream = client::connect(&root).await?;
    wire::write_frame(
        &mut stream,
        &json!({"version":PROTOCOL_VERSION,"app":grant.app,"token":grant.token}),
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (send, mut input) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut size = [0; 4];
            if stdin.read_exact(&mut size).is_err() {
                break;
            }
            let size = u32::from_le_bytes(size) as usize;
            if size == 0 || size > wire::MAX_FRAME {
                break;
            }
            let mut bytes = vec![0; size];
            if stdin.read_exact(&mut bytes).is_err() {
                break;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                break;
            };
            if send.blocking_send(value).is_err() {
                break;
            }
        }
    });
    let (output, frames) = tokio::sync::mpsc::channel::<Value>(BRIDGE_QUEUE);
    std::thread::spawn(move || write_to_extension(frames, &mut io::stdout().lock()));
    let read = pass_to_extension(&mut reader, &output, wire::CLIENT_STALL);
    let write = async {
        while let Some(value) = input.recv().await {
            wire::write_frame(&mut writer, &value).await?;
        }
        Ok::<(), io::Error>(())
    };
    tokio::select! { result = read => result, result = write => result }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An extension that takes what it is sent slowly.
    struct SlowExtension(Arc<Mutex<Vec<u8>>>);
    impl Write for SlowExtension {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_micros(200));
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frames_in(bytes: &[u8]) -> Vec<Value> {
        let mut frames = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let length = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            frames.push(serde_json::from_slice(&bytes[at + 4..at + 4 + length]).unwrap());
            at += 4 + length;
        }
        frames
    }

    #[tokio::test]
    async fn a_burst_from_the_broker_waits_for_a_slower_extension_instead_of_ending_the_bridge() {
        // Far more frames than the bridge's queue holds, sent at once.
        const BURST: usize = 1_000;
        let (mut broker, mut from_broker) = tokio::io::duplex(64 * 1024);
        let sender = tokio::spawn(async move {
            for n in 0..BURST {
                let frame = json!({"id":"a","event":{"type":"delta","text":format!("{n} ")}});
                wire::write_frame(&mut broker, &frame).await.unwrap();
            }
        });
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut extension = SlowExtension(written.clone());
        let (output, frames) = tokio::sync::mpsc::channel(BRIDGE_QUEUE);
        let writer = std::thread::spawn(move || write_to_extension(frames, &mut extension));
        // The bridge ends only because the broker closed: every frame was passed on.
        let ended = pass_to_extension(&mut from_broker, &output, wire::CLIENT_STALL)
            .await
            .unwrap_err();
        assert_eq!(ended.kind(), io::ErrorKind::UnexpectedEof, "{ended}");
        sender.await.unwrap();
        drop(output);
        writer.join().unwrap();
        let delivered = frames_in(&written.lock().unwrap());
        assert_eq!(delivered.len(), BURST);
        for (n, frame) in delivered.iter().enumerate() {
            assert_eq!(frame["event"]["text"], format!("{n} "));
        }
    }

    #[tokio::test]
    async fn the_bridge_ends_once_the_extension_is_gone() {
        let (mut broker, mut from_broker) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            while wire::write_frame(&mut broker, &json!({"id":"a"}))
                .await
                .is_ok()
            {}
        });
        let (output, frames) = tokio::sync::mpsc::channel(BRIDGE_QUEUE);
        drop(frames);
        let ended = pass_to_extension(&mut from_broker, &output, wire::CLIENT_STALL)
            .await
            .unwrap_err();
        assert!(ended.to_string().contains("is gone"), "{ended}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_bridge_gives_up_on_an_extension_that_takes_nothing_as_the_broker_does() {
        // The extension keeps its pipes open and reads nothing: the bridge must
        // not wait for it for ever, any more than the broker waits for the bridge.
        let (mut broker, mut from_broker) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            while wire::write_frame(&mut broker, &json!({"id":"a"}))
                .await
                .is_ok()
            {}
        });
        let (output, _unread) = tokio::sync::mpsc::channel(BRIDGE_QUEUE);
        let began = tokio::time::Instant::now();
        let ended = pass_to_extension(&mut from_broker, &output, wire::CLIENT_STALL)
            .await
            .unwrap_err();
        assert_eq!(ended.kind(), io::ErrorKind::TimedOut, "{ended}");
        assert_eq!(began.elapsed(), wire::CLIENT_STALL);
    }
}
