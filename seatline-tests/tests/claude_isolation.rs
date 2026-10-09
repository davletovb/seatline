//! Claude launch isolation (I-06): an owner who asks for it has the turns that
//! give Claude no tools started with `--safe-mode`, so the user's hooks,
//! plugins, skills and `CLAUDE.md` are not loaded at every start. A Claude that
//! does not know the option is asked again without it, once, and is not offered
//! it again until it changes. Against the fake Claude, which records every
//! command line it is given.
//!
//! The fake is a hard link of one binary, so another test's install changes its
//! identity, which this logic rightly reads as a replaced executable. Every test
//! here holds one lock and has a copy of its own. Nothing asserts a time.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use seatline_companion::client::socket_name;
use seatline_companion::remote::RemoteClient;
use seatline_companion::telemetry;
use seatline_core::protocol::ErrorCode;
use seatline_core::turn::{Message, ReasoningEffort, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::claude::Claude;
use seatline_providers::{Provider, Update};
use serde_json::Value;
use support::{FIXTURES, FakeClaude, PROMPT_STOP_GRACE, answer_text, failure, run_to_end};

const FAKE: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const APP: &str = "test_app";

static FIXTURE_LIFETIME: Mutex<()> = Mutex::new(());

/// Gives `dir/name` an executable of its own: a copy, where the fixture had a
/// link to the one fake provider binary every test shares.
///
/// Windows will not delete an executable that is running, and may hold it a
/// moment after its process has gone, so a refusal is tried again for a while.
fn independent_executable(dir: &Path, name: &str) {
    let executable = dir.join(name);
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_file(&executable) {
            Ok(()) => break,
            Err(error)
                if error.kind() != std::io::ErrorKind::NotFound && Instant::now() < give_up =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot replace {}: {error}", executable.display()),
        }
    }
    std::fs::copy(FIXTURES.provider(), executable).unwrap();
}

/// A fake Claude with an executable of its own, held for the test's whole life.
struct Fixture {
    claude: FakeClaude,
    /// Last, so the fake is gone before the next test starts.
    _lifetime: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(print: &str) -> Self {
        let lifetime = FIXTURE_LIFETIME
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let claude = FakeClaude::install(FIXTURES, print, "signed-in");
        independent_executable(&claude.dir, FakeClaude::file_name());
        Self {
            claude,
            _lifetime: lifetime,
        }
    }

    /// The adapter these tests run, which waits for each process to leave before
    /// its turn ends. A turn that keeps no session otherwise ends at Claude's
    /// result and leaves the process to exit in the background (I-05), and
    /// these tests replace the executable between turns, as an upgrade does:
    /// Windows will not replace one that is still running.
    fn adapter(&self) -> Claude {
        self.claude.adapter().with_background_exits(0)
    }

    /// The `claude -p` runs so far, one command line each.
    fn runs(&self) -> Vec<String> {
        self.claude
            .invocations()
            .into_iter()
            .filter(|line| line.starts_with("-p "))
            .collect()
    }

    /// For each run so far: whether it carried `--safe-mode`.
    fn safe_mode_runs(&self) -> Vec<bool> {
        self.runs()
            .iter()
            .map(|run| run.split(' ').any(|arg| arg == "--safe-mode"))
            .collect()
    }

    /// A Claude that was replaced, as an upgrade does: once none is running.
    fn replace_executable(&self) {
        self.claude.assert_nothing_left_running();
        independent_executable(&self.claude.dir, FakeClaude::file_name());
    }
}

/// A turn that gives Claude no tools and keeps no session.
fn plain(text: &str) -> Turn {
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

#[test]
fn only_a_turn_without_tools_for_an_owner_who_asked_starts_in_safe_mode() {
    let fixture = Fixture::new("answers");

    // Not asked for: no turn gets the option.
    let off = fixture.adapter();
    run_to_end(off.send(plain("a")).as_mut());
    run_to_end(
        off.send(Turn {
            tools: ToolPolicy::NativeWebSearch,
            ..plain("b")
        })
        .as_mut(),
    );
    assert_eq!(fixture.safe_mode_runs(), [false, false]);

    // Asked for: a turn without tools and a search turn get it. A turn that left
    // the provider's own configuration in charge does not: it asked for the
    // user's setup.
    let on = fixture.adapter().with_isolated_launch(true);
    for (tools, text) in [
        (ToolPolicy::None, "c"),
        (ToolPolicy::NativeWebSearch, "d"),
        (ToolPolicy::ProviderDefault, "e"),
    ] {
        let updates = run_to_end(
            on.send(Turn {
                tools,
                ..plain(text)
            })
            .as_mut(),
        );
        assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    }
    assert_eq!(fixture.safe_mode_runs(), [false, false, true, true, false]);

    // The option is the only difference: everything else Claude is told is the
    // same with it and without it.
    let runs = fixture.runs();
    assert_eq!(runs[2].replace(" --safe-mode", ""), runs[0], "{runs:?}");
}

#[test]
fn a_claude_from_before_safe_mode_is_asked_again_without_it_once_and_then_left_alone() {
    let fixture = Fixture::new("no-safe-mode-option");
    let adapter = fixture.adapter().with_isolated_launch(true);

    // That Claude rejected the first run before it started anything; the same
    // question, without the option, is answered. The application hears of one
    // launch, though two processes were started.
    let first = run_to_end(adapter.send(plain("first")).as_mut());
    assert_eq!(first.last(), Some(&Update::Completed), "{first:?}");
    assert_eq!(answer_text(&first), "You asked: first");
    assert_eq!(
        first
            .iter()
            .filter(|update| **update == Update::Launched)
            .count(),
        1,
        "{first:?}"
    );
    assert_eq!(fixture.safe_mode_runs(), [true, false]);

    // The next turn is not offered the option: that executable does not know it.
    let second = run_to_end(adapter.send(plain("second")).as_mut());
    assert_eq!(second.last(), Some(&Update::Completed), "{second:?}");
    assert_eq!(fixture.safe_mode_runs(), [true, false, false]);

    // A replaced executable may: it is offered the option again.
    fixture.replace_executable();
    run_to_end(adapter.send(plain("third")).as_mut());
    assert_eq!(fixture.safe_mode_runs(), [true, false, false, true, false]);
}

#[test]
fn what_was_learned_about_an_executable_is_forgotten_with_the_readiness_evidence() {
    let fixture = Fixture::new("no-safe-mode-option");
    let adapter = fixture.adapter().with_isolated_launch(true);
    run_to_end(adapter.send(plain("one")).as_mut());
    run_to_end(adapter.send(plain("two")).as_mut());
    assert_eq!(fixture.safe_mode_runs(), [true, false, false]);

    // The same events that drop cached readiness (a revoked grant, an
    // authentication failure) drop this too: the next turn tries again.
    adapter.invalidate_readiness();
    run_to_end(adapter.send(plain("three")).as_mut());
    assert_eq!(fixture.safe_mode_runs(), [true, false, false, true, false]);
}

#[test]
fn only_an_unknown_option_error_asks_for_another_run() {
    // A Claude that dies before it starts for some other reason: one run, an
    // ordinary failure, and nothing learned about the option.
    let fixture = Fixture::new("resume-crashes");
    let adapter = fixture.adapter().with_isolated_launch(true);
    let resume = || Turn {
        session: SessionPolicy::Persistent,
        continuation: Some("session-1".to_owned()),
        ..plain("x")
    };
    let updates = run_to_end(adapter.send(resume()).as_mut());
    assert_eq!(
        failure(&updates),
        (ErrorCode::ProviderFailed, "PROCESS_EXITED")
    );
    assert_eq!(fixture.safe_mode_runs(), [true]);
    run_to_end(adapter.send(resume()).as_mut());
    assert_eq!(fixture.safe_mode_runs(), [true, true]);
}

#[test]
fn a_claude_that_knows_neither_newer_option_fails_on_the_effort_after_two_runs() {
    let fixture = Fixture::new("no-newer-options");
    let adapter = fixture.adapter().with_isolated_launch(true);
    let updates = run_to_end(
        adapter
            .send(Turn {
                reasoning_effort: Some(ReasoningEffort::Low),
                ..plain("x")
            })
            .as_mut(),
    );
    // The first run names `--safe-mode`; the run without it names `--effort`,
    // which is the choice the turn cannot do without.
    assert_eq!(
        failure(&updates),
        (ErrorCode::InvalidRequest, "REASONING_EFFORT_UNSUPPORTED")
    );
    assert!(matches!(updates.last(), Some(Update::Failed(error)) if !error.retryable));
    assert_eq!(fixture.safe_mode_runs(), [true, false]);
    assert!(
        fixture.runs()[1].contains("--effort=low"),
        "{:?}",
        fixture.runs()
    );
}

#[test]
fn cancelling_a_run_that_is_about_to_be_rejected_stops_the_turn_and_asks_nothing_again() {
    let fixture = Fixture::new("slow-no-safe-mode-option");
    let adapter = fixture.adapter().with_isolated_launch(true);
    let mut exchange = adapter.send(plain("x"));
    // The run has started; the fake has not yet said it does not know the option.
    let first = exchange.next(Instant::now() + Duration::from_secs(5));
    assert_eq!(first, Some(Update::Launched));
    // Wait until the fake has recorded the run, or there is nothing to count:
    // it is then in the pause before it says so.
    let give_up = Instant::now() + Duration::from_secs(5);
    while fixture.runs().is_empty() {
        assert!(Instant::now() < give_up, "the fake never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    exchange.cancel(PROMPT_STOP_GRACE);
    let updates = run_to_end(exchange.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped), "{updates:?}");
    assert_eq!(fixture.safe_mode_runs(), [true]);
}

// ---- through the real broker -------------------------------------------------

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

/// The real broker with `policy` as the owner's `scheduling.json` (or none) and
/// the fake Claude as the only provider; every turn goes through its hub.
#[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
fn through_the_broker(policy: Option<&str>) -> (Vec<bool>, Value) {
    let fixture = Fixture::new("answers");
    let base = Path::new(SCRATCH).join(format!("ci{}", std::process::id() % 100_000));
    let (root, home, log) = (base.join("d"), base.join("h"), base.join("t.jsonl"));
    let authorized = Command::new(companion())
        .env("SEATLINE_DATA_DIR", &root)
        .args(["authorize", APP, "claude", "--allow-provider-default"])
        .output()
        .unwrap();
    assert!(authorized.status.success());
    if let Some(policy) = policy {
        std::fs::write(root.join("scheduling.json"), policy).unwrap();
    }
    let layout =
        seatline_platform::layout::Layout::new(seatline_core::turn::Namespace::fixed(APP).unwrap());
    let _broker = Broker(
        Command::new(companion())
            .arg("serve")
            .env("SEATLINE_DATA_DIR", &root)
            .env("SEATLINE_BROKER_IDLE_SECS", "120")
            .env(telemetry::FILE_VARIABLE, &log)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env(layout.search_path_variable(), &fixture.claude.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let give_up = Instant::now() + Duration::from_secs(15);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    loop {
        let name = socket_name(&root).unwrap();
        let connected = runtime.block_on(async {
            use interprocess::local_socket::tokio::{Stream, prelude::*};
            Stream::connect(name).await.is_ok()
        });
        if connected {
            break;
        }
        assert!(Instant::now() < give_up, "the broker never listened");
        std::thread::sleep(Duration::from_millis(20));
    }

    let client = RemoteClient::with_root(APP, root.clone());
    for (tools, text) in [
        (ToolPolicy::None, "plain"),
        (ToolPolicy::ProviderDefault, "provider default"),
    ] {
        let turn = Turn {
            tools,
            ..plain(text)
        };
        let updates = run_to_end(client.send("claude", &turn).as_mut());
        assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    }
    let broker_record = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["kind"] == "broker")
        .expect("the broker wrote its configuration");
    let runs = fixture.safe_mode_runs();
    let _ = std::fs::remove_dir_all(&base);
    (runs, broker_record)
}

#[test]
fn the_owners_setting_reaches_claude_through_the_real_broker() {
    // Set in the owner's file, it reaches the adapter: the turn without tools
    // starts in safe mode and the provider-default turn does not.
    let (runs, broker) = through_the_broker(Some(r#"{"claude_isolation": true}"#));
    assert_eq!(runs, [true, false]);
    assert_eq!(broker["limits"]["claude_isolation"], 1, "{broker}");

    // Not set, or the file says nothing about it: nothing changes.
    for policy in [None, Some(r#"{"max_running": 4}"#)] {
        let (runs, broker) = through_the_broker(policy);
        assert_eq!(runs, [false, false], "{policy:?}");
        assert_eq!(broker["limits"]["claude_isolation"], 0, "{broker}");
    }
}
