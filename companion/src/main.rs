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
    hub, wire,
};
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
            let hub = hub::start(root.clone())?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(serve(root, hub))
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
            Ok(())
        }
        Some("install") if args.len() == 1 => {
            println!(
                "Seatline installed at {}",
                seatline_companion::install::install(&root)?.display()
            );
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
            "usage: seatline-companion install | serve | connect APP | pair APP RELAY SITE [--open] | authorize APP PROVIDERS [EXTENSION_ORIGIN...] [SITE_ORIGIN...] [--relay=RELAY_ORIGIN] [--cache-title=TITLE] [--allow-provider-default] | register-native APP EXECUTABLE [ARGS...] | revoke APP | manifest",
        )),
    }
}

const MAX_CONNECTIONS: usize = 32;
/// How long the broker stays up with no connections at all. Exiting when idle
/// means the next start runs the installed binary, so an upgrade takes effect
/// without anyone having to stop the old broker. `0` disables the exit.
const DEFAULT_IDLE_SECONDS: u64 = 600;

fn idle_limit() -> Option<Duration> {
    let seconds = std::env::var("SEATLINE_BROKER_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_IDLE_SECONDS);
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

async fn serve(root: PathBuf, hub: std::sync::mpsc::SyncSender<hub::Command>) -> io::Result<()> {
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
            continue;
        };
        let (root, hub, connection, last_activity) = (
            root.clone(),
            hub.clone(),
            ids.fetch_add(1, Ordering::Relaxed),
            last_activity.clone(),
        );
        tokio::spawn(async move {
            let _permit = permit;
            let _ = connection_loop(stream, root, hub.clone(), connection).await;
            // Close must eventually be delivered even when the bounded inbox is full.
            while let Err(std::sync::mpsc::TrySendError::Full(_)) =
                hub.try_send(hub::Command::Close(connection))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            last_activity.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        });
    }
}

async fn connection_loop(
    mut stream: Stream,
    root: PathBuf,
    hub: std::sync::mpsc::SyncSender<hub::Command>,
    connection: u64,
) -> io::Result<()> {
    let auth =
        tokio::time::timeout(Duration::from_secs(5), wire::read_frame(&mut stream)).await??;
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
        while let Some(event) = events.recv().await {
            tokio::time::timeout(
                Duration::from_secs(5),
                wire::write_frame(&mut writer, &event),
            )
            .await??;
        }
        Ok::<(), io::Error>(())
    };
    // Cancelling read ends the connection; it is never resumed mid-frame.
    tokio::select! { result = read => result, result = write => result }
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
    let (output, events) = std::sync::mpsc::sync_channel::<Value>(64);
    std::thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        for value in events {
            let Ok(bytes) = serde_json::to_vec(&value) else {
                break;
            };
            if stdout
                .write_all(&(bytes.len() as u32).to_le_bytes())
                .and_then(|()| stdout.write_all(&bytes))
                .and_then(|()| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    });
    let read = async {
        loop {
            output
                .try_send(wire::read_frame(&mut reader).await?)
                .map_err(|_| io::Error::other("bridge output full"))?;
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let write = async {
        while let Some(value) = input.recv().await {
            wire::write_frame(&mut writer, &value).await?;
        }
        Ok::<(), io::Error>(())
    };
    tokio::select! { result = read => result, result = write => result }
}
