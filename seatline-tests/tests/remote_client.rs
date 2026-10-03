//! The reusable remote client against the real broker (D-01): several
//! requests share one connection and are served concurrently, a cancel stops
//! one of them and reaps only its provider process, and a broker that goes
//! away ends what was in flight without replaying it, while the next request
//! reconnects to the broker that replaced it.
//!
//! The provider is the fake Codex. Nothing here asserts a time threshold.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use seatline_companion::client::socket_name;
use seatline_companion::remote::RemoteClient;
use seatline_companion::telemetry;
use seatline_core::exchange::{Exchange, Update};
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use serde_json::Value;
use support::{FIXTURES, FakeCodex};

const FAKE: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const APP: &str = "test_app";

/// The companion, built into the same directory as the fake provider. A plain
/// `cargo test --workspace` builds it for the companion's own tests; a run of
/// this package alone does not, so build it then.
#[allow(clippy::disallowed_methods)] // Builds the companion these tests run; never provider execution.
fn companion() -> PathBuf {
    let name = if cfg!(windows) {
        "seatline-companion.exe"
    } else {
        "seatline-companion"
    };
    let path = Path::new(FAKE).with_file_name(name);
    if !path.is_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut build = Command::new(env!("CARGO"));
        build
            .current_dir(root)
            .args(["build", "--locked", "-p", "seatline-companion"]);
        if !cfg!(debug_assertions) {
            build.arg("--release");
        }
        assert!(
            build.status().unwrap().success(),
            "could not build the companion"
        );
    }
    path
}

struct Broker(Child);

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One data directory with the app authorized, the fake Codex as the only
/// provider, and a broker that can be started, killed and started again.
struct World {
    root: PathBuf,
    home: PathBuf,
    fake: FakeCodex,
    telemetry: PathBuf,
    broker: Option<Broker>,
}

impl World {
    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn new(exec: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // Short: a socket path has a small limit.
        let base = Path::new(SCRATCH).join(format!(
            "rc{}-{}",
            std::process::id() % 100_000,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("d");
        let authorized = Command::new(companion())
            .env("SEATLINE_DATA_DIR", &root)
            .args(["authorize", APP, "codex"])
            .output()
            .unwrap();
        assert!(authorized.status.success());
        let mut world = Self {
            home: base.join("h"),
            telemetry: base.join("t.jsonl"),
            root,
            fake: FakeCodex::install(FIXTURES, exec, "signed-in"),
            broker: None,
        };
        world.start_broker();
        world
    }

    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn start_broker(&mut self) {
        let layout = seatline_platform::layout::Layout::new(
            seatline_core::turn::Namespace::fixed(APP).unwrap(),
        );
        let child = Command::new(companion())
            .arg("serve")
            .env("SEATLINE_DATA_DIR", &self.root)
            .env("SEATLINE_BROKER_IDLE_SECS", "120")
            .env(telemetry::FILE_VARIABLE, &self.telemetry)
            .env("HOME", &self.home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env(layout.search_path_variable(), &self.fake.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.broker = Some(Broker(child));
        // Listening means a client can connect.
        let give_up = Instant::now() + Duration::from_secs(15);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        loop {
            let name = socket_name(&self.root).unwrap();
            let connected = runtime.block_on(async {
                use interprocess::local_socket::tokio::{Stream, prelude::*};
                Stream::connect(name).await.is_ok()
            });
            if connected {
                return;
            }
            assert!(Instant::now() < give_up, "the broker never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_broker(&mut self) {
        drop(self.broker.take());
    }

    fn client(&self) -> RemoteClient {
        RemoteClient::with_root(APP, self.root.clone())
    }

    /// The broker's telemetry records of one kind.
    fn records(&self, kind: &str) -> Vec<Value> {
        std::fs::read_to_string(&self.telemetry)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["kind"] == kind)
            .collect()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        drop(self.broker.take());
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

fn ask(text: &str) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: None,
        tools: ToolPolicy::None,
        session: SessionPolicy::Ephemeral,
        continuation: None,
        cleanup_group: None,
        check_sign_in: false,
    }
}

/// Reads `exchange` to its end.
fn drain(exchange: &mut dyn Exchange) -> Vec<Update> {
    let give_up = Instant::now() + Duration::from_secs(30);
    let mut updates = Vec::new();
    while Instant::now() < give_up {
        if let Some(update) = exchange.next(Instant::now() + Duration::from_millis(100)) {
            let terminal = update.is_terminal();
            updates.push(update);
            if terminal {
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

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let give_up = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < give_up, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn concurrent_turns_share_one_connection_and_each_gets_its_own_answer() {
    let world = World::new("by-prompt");
    let client = world.client();
    // Each thread makes its own turn, all at once, on the one client.
    let start = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|index| {
            let (client, start) = (client.clone(), start.clone());
            std::thread::spawn(move || {
                let question = format!("answers {index}");
                start.wait();
                let mut exchange = client.send("codex", &ask(&question));
                (question, drain(exchange.as_mut()))
            })
        })
        .collect();
    for thread in threads {
        let (question, updates) = thread.join().unwrap();
        assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
        assert_eq!(text(&updates), format!("You asked: {question}"));
    }
    // One connection was made, authenticated once, and carried four requests.
    wait_until("the broker's records", || {
        world.records("request").len() == 4
    });
    assert_eq!(world.records("connection").len(), 1);
    let ids: std::collections::BTreeSet<_> = world
        .records("request")
        .iter()
        .map(|record| record["request"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 4);
    // And the fake saw four turns.
    assert_eq!(
        world
            .fake
            .invocations()
            .iter()
            .filter(|line| line.starts_with("exec "))
            .count(),
        4
    );
}

#[test]
fn a_cancel_stops_one_turn_and_reaps_only_its_process() {
    let world = World::new("goes-quiet");
    let client = world.client();
    let mut first = client.send("codex", &ask("one"));
    let mut second = client.send("codex", &ask("two"));
    wait_until("both providers to start", || world.fake.pids().len() == 2);

    first.cancel(Duration::ZERO);
    let updates = drain(first.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped), "{updates:?}");
    wait_until("the cancelled turn's process to be reaped", || {
        world.fake.still_running().len() == 1
    });
    // The other turn is untouched, and goes the same way when it is asked to.
    while let Some(update) = second.next(Instant::now() + Duration::from_millis(200)) {
        assert!(!update.is_terminal(), "the other turn ended: {update:?}");
    }
    second.cancel(Duration::ZERO);
    assert_eq!(drain(second.as_mut()).last(), Some(&Update::Stopped));
    wait_until("both processes to be reaped", || {
        world.fake.still_running().is_empty()
    });
    assert_eq!(world.records("connection").len(), 1);
}

#[test]
fn a_broker_that_goes_away_ends_the_turn_without_a_replay_and_the_next_request_reconnects() {
    let mut world = World::new("goes-quiet");
    let client = world.client();
    let mut lost = client.send("codex", &ask("never finishes"));
    wait_until("the provider to start", || world.fake.pids().len() == 1);

    world.kill_broker();
    let updates = drain(lost.as_mut());
    assert!(
        matches!(updates.last(), Some(Update::Failed(failure))
            if failure.reason == "COMPANION_DISCONNECTED" && failure.retryable),
        "{updates:?}"
    );

    // A broker takes its place, and the next request finds it.
    world.start_broker();
    let mut next = client.status("codex");
    let updates = drain(next.as_mut());
    assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    // The turn that was lost was started once: nothing replayed it.
    assert_eq!(
        world
            .fake
            .invocations()
            .iter()
            .filter(|line| line.starts_with("exec "))
            .count(),
        1
    );
}
