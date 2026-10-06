//! The reusable remote client against the real broker (D-01): several
//! requests share one connection and are served concurrently, a cancel stops
//! one of them and reaps only its provider process, and a broker that goes
//! away ends what was in flight without replaying it, while the next request
//! reconnects to the broker that replaced it. Preparation, readiness and an
//! explicit checked send go through the same shared client, as the readiness
//! slice (C-01 to C-04) and this one were agreed to be tested together.
//!
//! The provider is the fake Codex. Nothing here asserts a time threshold.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use seatline_companion::client::{RemoteProvider, socket_name};
use seatline_companion::control;
use seatline_companion::remote::RemoteClient;
use seatline_companion::telemetry;
use seatline_core::exchange::{Exchange, Update};
use seatline_core::readiness::{Freshness, SignInPolicy, Source};
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::Provider;
use serde_json::Value;
use support::{FIXTURES, FakeCodex};

const FAKE: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const APP: &str = "test_app";

/// Held for a world's whole life. The fake Codex is a hard link of one binary,
/// so another test's install changes its change time, which readiness caching
/// rightly takes for a replaced executable: tests that share the file run one
/// at a time, and each has an executable of its own.
static FIXTURE_LIFETIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn independent_executable(dir: &Path, name: &str) {
    let executable = dir.join(name);
    std::fs::remove_file(&executable).unwrap();
    std::fs::copy(FIXTURES.provider(), executable).unwrap();
}

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
    /// How long the current broker waits for running work when it is asked to stop.
    grace: Duration,
    /// Last, so the broker is gone before the next world starts.
    _lifetime: std::sync::MutexGuard<'static, ()>,
}

impl World {
    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn new(exec: &str) -> Self {
        let lifetime = FIXTURE_LIFETIME
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        let fake = FakeCodex::install(FIXTURES, exec, "signed-in");
        independent_executable(&fake.dir, FakeCodex::file_name());
        let mut world = Self {
            home: base.join("h"),
            telemetry: base.join("t.jsonl"),
            root,
            fake,
            broker: None,
            grace: Duration::ZERO,
            _lifetime: lifetime,
        };
        world.start_broker();
        world
    }

    /// A broker that, when asked to stop, waits a long time for running work: a test that does not care
    /// leaves it, and one that does sets its own.
    fn start_broker(&mut self) {
        self.start_broker_with(Duration::from_secs(60));
    }

    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn start_broker_with(&mut self, grace: Duration) {
        self.grace = grace;
        let layout = seatline_platform::layout::Layout::new(
            seatline_core::turn::Namespace::fixed(APP).unwrap(),
        );
        let child = Command::new(companion())
            .arg("serve")
            .env("SEATLINE_DATA_DIR", &self.root)
            .env("SEATLINE_BROKER_IDLE_SECS", "120")
            .env(control::GRACE_VARIABLE, grace.as_millis().to_string())
            .env(telemetry::FILE_VARIABLE, &self.telemetry)
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
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

    /// `seatline-companion stop` against this world's broker.
    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn stop_command(&self) -> Command {
        let mut command = Command::new(companion());
        command
            .arg("stop")
            .env("SEATLINE_DATA_DIR", &self.root)
            .env(control::GRACE_VARIABLE, self.grace.as_millis().to_string());
        command
    }

    fn broker_has_exited(&mut self) -> bool {
        self.broker
            .as_mut()
            .is_none_or(|broker| broker.0.try_wait().unwrap().is_some())
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
        reasoning_effort: None,
        service_tier: None,
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
    // The harness can tell which launches are still alive only on Unix; on
    // Windows the cancellation itself is what is checked.
    #[cfg(unix)]
    wait_until("the cancelled turn's process to be reaped", || {
        world.fake.still_running().len() == 1
    });
    // The other turn is untouched, and goes the same way when it is asked to.
    while let Some(update) = second.next(Instant::now() + Duration::from_millis(200)) {
        assert!(!update.is_terminal(), "the other turn ended: {update:?}");
    }
    second.cancel(Duration::ZERO);
    assert_eq!(drain(second.as_mut()).last(), Some(&Update::Stopped));
    #[cfg(unix)]
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

#[test]
fn a_stop_waits_for_a_running_turn_up_to_the_grace_period_then_ends_it_and_reaps_its_provider() {
    let mut world = World::new("goes-quiet");
    world.kill_broker();
    world.start_broker_with(Duration::from_millis(1500));
    let client = world.client();
    let mut running = client.send("codex", &ask("never finishes"));
    wait_until("the provider to start", || world.fake.pids().len() == 1);

    let stopping = world
        .stop_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The turn never ends by itself and the grace period is longer than this wait: the broker is
    // still there, and has not ended the turn.
    let waiting = Instant::now() + Duration::from_millis(300);
    while let Some(update) = running.next(waiting) {
        assert!(!update.is_terminal(), "the turn ended early: {update:?}");
    }
    assert!(
        !world.broker_has_exited(),
        "the broker left with a turn running"
    );
    let output = stopping.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");

    // Past the grace period the turn is ended the way a closing broker ends it: the client is told the
    // connection was lost (nothing is replayed), and the provider process does not outlive the broker.
    let updates = drain(running.as_mut());
    assert!(
        matches!(updates.last(), Some(Update::Failed(failure))
            if failure.reason == "COMPANION_DISCONNECTED" && failure.retryable),
        "{updates:?}"
    );
    wait_until("the broker to exit", || world.broker_has_exited());
    #[cfg(unix)]
    wait_until("the provider to be reaped", || {
        world.fake.still_running().is_empty()
    });

    // The next use finds a broker again, as it does after any exit.
    world.start_broker();
    let mut next = client.status("codex");
    assert_eq!(drain(next.as_mut()).last(), Some(&Update::Completed));
}

#[test]
fn a_stop_does_not_wait_for_an_app_that_is_connected_but_has_nothing_running() {
    // The grace period is a minute, and the app keeps its connection: the broker still leaves as soon
    // as the hub has nothing to do, which is what lets `install` finish promptly.
    let mut world = World::new("answers");
    let client = world.client();
    let mut first = client.status("codex");
    assert_eq!(drain(first.as_mut()).last(), Some(&Update::Completed));

    let started = Instant::now();
    let output = world.stop_command().output().unwrap();
    assert!(output.status.success(), "{output:?}");
    // Half the grace period is far more than an idle hub needs, and far less than waiting it out.
    assert!(
        started.elapsed() < world.grace / 2,
        "stop waited for the grace period with nothing running"
    );
    wait_until("the broker to exit", || world.broker_has_exited());

    world.start_broker();
    let mut again = client.status("codex");
    assert_eq!(drain(again.as_mut()).last(), Some(&Update::Completed));
}

#[test]
fn prepare_readiness_and_checked_sends_travel_through_the_shared_client() {
    // Preparation, readiness and an explicit checked send, made through
    // `RemoteProvider::with_client`, are requests on the app's one connection,
    // and the broker's retained readiness serves them: one probe covers the
    // preparations, the readiness and the cached send, and a send that asks for
    // a fresh check adds exactly one.
    const CACHED: Freshness = Freshness::Cached { max_age_ms: 30_000 };
    let world = World::new("answers");
    world.fake.set("answers", "subscription");
    let provider = RemoteProvider::with_client(APP, world.client(), &world.fake.adapter());
    assert!(provider.supports_preparation());

    let source = |updates: &[Update]| {
        updates.iter().find_map(|update| match update {
            Update::Status { status, .. } => status.readiness.map(|readiness| readiness.source),
            _ => None,
        })
    };
    let not_a_generation = |updates: &[Update]| {
        !updates
            .iter()
            .any(|u| matches!(u, Update::Launched | Update::Delta(_) | Update::Session(_)))
    };

    let first = drain(provider.prepare(CACHED).as_mut());
    assert_eq!(first.last(), Some(&Update::Completed), "{first:?}");
    assert_eq!(source(&first), Some(Source::Fresh));
    assert!(first.iter().any(|update| matches!(update,
        Update::Status { status, .. } if status.sign_in == Some(seatline_core::turn::SignInClassification::Subscription)
    )), "{first:?}");
    assert!(not_a_generation(&first), "preparation ran a generation");
    let second = drain(provider.prepare(CACHED).as_mut());
    assert_eq!(second.last(), Some(&Update::Completed), "{second:?}");
    assert_eq!(source(&second), Some(Source::Cached));
    let readiness = drain(provider.readiness(CACHED).as_mut());
    assert_eq!(readiness.last(), Some(&Update::Completed), "{readiness:?}");
    assert_eq!(source(&readiness), Some(Source::Cached));

    // A checked send reuses the verified readiness: its status comes first,
    // and no second probe is run.
    let cached_send = drain(
        provider
            .send_with_readiness_policy(
                Turn {
                    reasoning_effort: Some(seatline_core::turn::ReasoningEffort::Low),
                    ..ask("Say hello")
                },
                CACHED,
                SignInPolicy::try_from(vec![
                    seatline_core::turn::SignInClassification::Subscription,
                ])
                .unwrap(),
            )
            .as_mut(),
    );
    assert_eq!(
        cached_send.last(),
        Some(&Update::Completed),
        "{cached_send:?}"
    );
    assert_eq!(text(&cached_send), "You asked: Say hello");
    assert_eq!(source(&cached_send), Some(Source::Cached));
    assert!(cached_send.iter().any(|update| matches!(update,
        Update::Status { status, .. } if status.capabilities.reasoning_effort == seatline_core::protocol::Capability::Supported
    )), "{cached_send:?}");
    let status_at = cached_send
        .iter()
        .position(|u| matches!(u, Update::Status { .. }))
        .unwrap();
    let launched_at = cached_send
        .iter()
        .position(|u| *u == Update::Launched)
        .unwrap();
    assert!(status_at < launched_at, "status must precede the launch");

    // One that requires a fresh sign-in check overrides the cache.
    let fresh = Turn {
        check_sign_in: true,
        ..ask("Say hello again")
    };
    let fresh_send = drain(provider.send_with_readiness(fresh, CACHED).as_mut());
    assert_eq!(
        fresh_send.last(),
        Some(&Update::Completed),
        "{fresh_send:?}"
    );
    assert_eq!(source(&fresh_send), Some(Source::Fresh));

    // What the fake saw: two probes (the first preparation and the fresh
    // send) and two turns, whatever the number of requests.
    let invocations = world.fake.invocations();
    let count = |prefix: &str| invocations.iter().filter(|l| l.starts_with(prefix)).count();
    assert_eq!(count("login "), 2, "{invocations:?}");
    assert_eq!(count("exec "), 2, "{invocations:?}");
    let generations: Vec<_> = invocations
        .iter()
        .filter(|line| line.starts_with("exec "))
        .collect();
    assert!(
        generations[0].contains("-c model_reasoning_effort=\"low\""),
        "{generations:?}"
    );
    assert!(
        !generations[1].contains("model_reasoning_effort"),
        "{generations:?}"
    );

    // And what the broker saw: five requests on one connection.
    wait_until("the broker's records", || {
        world.records("request").len() == 5
    });
    assert_eq!(world.records("connection").len(), 1);
    let methods: Vec<String> = world
        .records("request")
        .iter()
        .map(|record| record["method"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(
        methods,
        [
            "prepare",
            "prepare",
            "readiness",
            "send_ready_with_policy",
            "send_ready"
        ]
    );
}

#[test]
fn changed_sign_in_is_rejected_over_ipc_before_the_client_consumes_status() {
    const CACHED: Freshness = Freshness::Cached { max_age_ms: 30_000 };
    let world = World::new("answers");
    world.fake.set("answers", "subscription");
    let account = world.home.join(".codex");
    std::fs::create_dir_all(&account).unwrap();
    std::fs::write(account.join("auth.json"), br#"{"account":"subscription"}"#).unwrap();
    let provider = RemoteProvider::with_client(APP, world.client(), &world.fake.adapter());
    let approved = drain(provider.readiness(CACHED).as_mut());
    assert_eq!(approved.last(), Some(&Update::Completed), "{approved:?}");
    assert!(approved.iter().any(|update| matches!(update,
        Update::Status { status, .. } if status.sign_in == Some(seatline_core::turn::SignInClassification::Subscription)
    )), "{approved:?}");

    world.fake.set("answers", "signed-in"); // The fake reports API-key sign-in.
    std::fs::write(account.join("auth.json"), br#"{"account":"api-key"}"#).unwrap();
    let mut rejected = provider.send_with_readiness_policy(
        ask("This prompt must not launch"),
        CACHED,
        SignInPolicy::try_from(vec![
            seatline_core::turn::SignInClassification::Subscription,
        ])
        .unwrap(),
    );
    // The broker finishes before the application consumes any status event.
    // No client callback or cancel can be responsible for preventing launch.
    wait_until("policy refusal with client delivery delayed", || {
        world.records("request").len() == 2
    });
    assert_eq!(
        world
            .fake
            .invocations()
            .iter()
            .filter(|line| line.starts_with("exec "))
            .count(),
        0
    );
    let updates = drain(rejected.as_mut());
    assert!(
        matches!(updates.last(), Some(Update::Failed(f)) if f.reason == "SIGN_IN_POLICY_DENIED" && !f.retryable),
        "{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|u| matches!(u, Update::Launched | Update::Delta(_)))
    );
    assert!(updates.iter().any(|update| matches!(update,
        Update::Status { status, .. } if status.sign_in == Some(seatline_core::turn::SignInClassification::ApiKey)
    )), "{updates:?}");
}
