//! `seatline-companion stop`, and `install` ending the broker it replaces: the running broker leaves on
//! request even while apps stay connected, it answers only the control token it published, and a
//! broker that cannot be asked is reported as such instead of being waited for.
use interprocess::local_socket::tokio::{Stream, prelude::*};
use seatline_companion::{client, config, control, wire};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const EXE: &str = env!("CARGO_BIN_EXE_seatline-companion");
const APP: &str = "test_app";

fn root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "seatline-stop-{}",
        &config::random_token().unwrap()[..12]
    ))
}

#[allow(clippy::disallowed_methods)] // Exercise the built binary, never provider execution.
fn companion(root: &Path) -> Command {
    let mut command = Command::new(EXE);
    command.env("SEATLINE_DATA_DIR", root);
    command
}

fn authorize(root: &Path) {
    assert!(
        companion(root)
            .args(["authorize", APP, "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

fn start_broker(root: &Path) -> Broker {
    let broker = Broker(
        companion(root)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // Listening, and the control token published.
    let until = Instant::now() + Duration::from_secs(15);
    while !(root.join("broker.sock").exists() || cfg!(windows))
        || !root.join(control::TOKEN_FILE).exists()
    {
        assert!(Instant::now() < until, "the broker never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    broker
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn connect(root: &Path) -> Stream {
    for _ in 0..200 {
        if let Ok(stream) = Stream::connect(client::socket_name(root).unwrap()).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the broker was not listening");
}

/// Connects as the app and completes the handshake, as an app that stays connected does.
async fn connected_app(root: &Path) -> Stream {
    let mut stream = connect(root).await;
    let grant = config::load_grant(root, APP).unwrap();
    wire::write_frame(
        &mut stream,
        &json!({"version":1,"app":APP,"token":grant.token}),
    )
    .await
    .unwrap();
    assert_eq!(
        wire::read_frame(&mut stream).await.unwrap()["type"],
        "ready"
    );
    stream
}

fn exits(broker: &mut Broker) -> bool {
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        if broker.0.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn stop_ends_the_broker_even_while_an_app_stays_connected() {
    let root = root();
    authorize(&root);
    let mut broker = start_broker(&root);
    runtime().block_on(async {
        let mut app = connected_app(&root).await;

        let stopped = companion(&root).arg("stop").output().unwrap();
        assert!(stopped.status.success(), "{stopped:?}");
        assert!(
            String::from_utf8_lossy(&stopped.stdout).contains("Seatline stopped"),
            "{stopped:?}"
        );
        // The app sees its connection end; it would reconnect by itself.
        assert!(wire::read_frame(&mut app).await.is_err());
    });
    assert!(exits(&mut broker), "the broker is still running");
    assert!(
        !root.join(control::TOKEN_FILE).exists(),
        "the control token was left behind"
    );

    // Nothing is running now: a second stop says so, and starts nothing.
    let again = companion(&root).arg("stop").output().unwrap();
    assert!(again.status.success(), "{again:?}");
    assert!(String::from_utf8_lossy(&again.stdout).contains("not running"));
    assert!(!root.join("broker.sock").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn only_the_published_control_token_stops_the_broker() {
    let root = root();
    authorize(&root);
    let mut broker = start_broker(&root);
    let published = std::fs::read_to_string(root.join(control::TOKEN_FILE)).unwrap();
    assert_eq!(published.len(), 64);
    runtime().block_on(async {
        let app_token = config::load_grant(&root, APP).unwrap().token;
        for frame in [
            // An app's own token is not a way to stop the shared broker.
            json!({"version":1,"control":"stop","token":app_token}),
            json!({"version":1,"control":"stop","token":"0".repeat(64)}),
            json!({"version":1,"control":"stop"}),
            json!({"version":2,"control":"stop","token":published.trim()}),
            json!({"version":1,"control":"restart","token":published.trim()}),
        ] {
            let mut stream = connect(&root).await;
            wire::write_frame(&mut stream, &frame).await.unwrap();
            assert!(
                wire::read_frame(&mut stream).await.is_err(),
                "{frame} was answered"
            );
        }
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        broker.0.try_wait().unwrap().is_none(),
        "a refused request stopped the broker"
    );
    // And the broker still serves its apps.
    runtime().block_on(async { connected_app(&root).await });

    let stopped = companion(&root).arg("stop").output().unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(exits(&mut broker));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stop_with_no_broker_starts_none() {
    let root = root();
    authorize(&root);
    let stopped = companion(&root).arg("stop").output().unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("not running"));
    assert!(!root.join("broker.sock").exists());
    assert!(!root.join(control::TOKEN_FILE).exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// A broker from a Seatline without `stop` holds the lock and publishes no token, and drops a
/// connection that does not authenticate as an app. `stop` must say so rather than wait for it.
#[test]
fn a_broker_that_cannot_be_asked_is_reported_not_waited_for() {
    let root = root();
    authorize(&root);
    let _older_broker = config::lock(&root, "broker.lock").unwrap();
    let started = Instant::now();
    let stopped = companion(&root).arg("stop").output().unwrap();
    assert!(!stopped.status.success(), "{stopped:?}");
    assert!(
        String::from_utf8_lossy(&stopped.stderr).contains("cannot be asked to stop"),
        "{stopped:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "stop waited for a broker it could not ask"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `install` registers the copy it installs with Chrome, which on Unix is a file under the home
/// directory: this test gives it one of its own. (Windows registers in the registry.)
#[cfg(unix)]
#[test]
fn install_ends_the_broker_it_replaces() {
    let root = root();
    let home = root.with_extension("home");
    authorize(&root);
    let mut broker = start_broker(&root);
    let installed = companion(&root)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .arg("install")
        .output()
        .unwrap();
    assert!(installed.status.success(), "{installed:?}");
    let said = String::from_utf8_lossy(&installed.stdout);
    assert!(said.contains("Seatline installed at"), "{said}");
    assert!(
        said.contains("Stopped the Seatline that was running"),
        "{said}"
    );
    assert!(exits(&mut broker), "the replaced broker is still running");

    // With nothing running, install says nothing about stopping.
    let again = companion(&root)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .arg("install")
        .output()
        .unwrap();
    assert!(again.status.success(), "{again:?}");
    assert!(!String::from_utf8_lossy(&again.stdout).contains("Stopped"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
}
