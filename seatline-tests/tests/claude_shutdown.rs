//! A broker asked to stop while a finished Claude turn's process is still
//! leaving (I-05).
//!
//! A Claude turn that keeps no session ends at Claude's result, and its process
//! is left to exit on its own under a bounded reaper whose helper threads end
//! with the broker process. A broker that went while one was still waiting
//! would leave that process running with nobody to stop it, and `stop` (or
//! `install`) would take the broker for quiet while a provider process of its
//! was alive. Against the real broker and the fake Claude: `stop` waits for a
//! process that is leaving on its own, and one that will not leave is stopped
//! before the broker goes.
//!
//! Nothing here asserts a time.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use seatline_companion::client::socket_name;
use seatline_companion::control;
use seatline_companion::remote::RemoteClient;
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::Update;
use support::{FIXTURES, FakeClaude, run_to_end};

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

/// A data directory with the app authorized, the fake Claude as the only
/// provider, and a broker listening.
struct World {
    claude: FakeClaude,
    root: PathBuf,
    _broker: Broker,
}

impl World {
    /// A broker that, when asked to stop, waits `grace` for running work.
    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn new(print: &str, grace: Duration) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // Short: a socket path has a small limit.
        let base = Path::new(SCRATCH).join(format!(
            "cs{}-{}",
            std::process::id() % 100_000,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let (root, home) = (base.join("d"), base.join("h"));
        let authorized = Command::new(companion())
            .env("SEATLINE_DATA_DIR", &root)
            .args(["authorize", APP, "claude"])
            .output()
            .unwrap();
        assert!(authorized.status.success());
        let claude = FakeClaude::install(FIXTURES, print, "signed-in");
        let layout = seatline_platform::layout::Layout::new(
            seatline_core::turn::Namespace::fixed(APP).unwrap(),
        );
        let broker = Broker(
            Command::new(companion())
                .arg("serve")
                .env("SEATLINE_DATA_DIR", &root)
                .env("SEATLINE_BROKER_IDLE_SECS", "120")
                .env(control::GRACE_VARIABLE, grace.as_millis().to_string())
                .env("HOME", &home)
                .env("XDG_CACHE_HOME", home.join(".cache"))
                .env(layout.search_path_variable(), &claude.dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let world = Self {
            claude,
            root,
            _broker: broker,
        };
        world.wait_until_listening();
        world
    }

    fn wait_until_listening(&self) {
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

    /// One turn that keeps no session, run to its end.
    fn ask(&self) -> Vec<Update> {
        let turn = Turn {
            system: None,
            messages: vec![Message {
                role: Role::User,
                text: "hello".to_owned(),
            }],
            model: None,
            reasoning_effort: None,
            service_tier: None,
            tools: ToolPolicy::None,
            session: SessionPolicy::Ephemeral,
            continuation: None,
            cleanup_group: None,
            check_sign_in: false,
        };
        run_to_end(
            RemoteClient::with_root(APP, self.root.clone())
                .send("claude", &turn)
                .as_mut(),
        )
    }

    /// `seatline-companion stop`, which returns once the broker has left.
    #[allow(clippy::disallowed_methods)] // Runs the companion this package builds; never a request-supplied program.
    fn stop(&self) -> Output {
        Command::new(companion())
            .arg("stop")
            .env("SEATLINE_DATA_DIR", &self.root)
            .output()
            .unwrap()
    }

    /// How many launches have left on their own.
    fn exits(&self) -> usize {
        self.claude.read("claude-exits").lines().count()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // A test that failed must not leave its Claude running.
        #[cfg(unix)]
        for pid in self.claude.still_running() {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            if let Ok(pid) = i32::try_from(pid) {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

#[test]
fn a_broker_asked_to_stop_lets_a_finished_turns_process_leave_on_its_own() {
    // The fake takes 1.5 s to leave after its result.
    let world = World::new("exits-after-1500", Duration::from_secs(30));
    let updates = world.ask();
    assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    // The turn is over, and the process is still on its way out.
    assert_eq!(world.exits(), 0, "the process had already left");

    let stopped = world.stop();
    assert!(stopped.status.success(), "{stopped:?}");

    // The broker waited for it: it left by itself, which a signal or a kill
    // would have prevented, and nothing of the fake's is running.
    assert_eq!(world.exits(), 1, "it was stopped before it could leave");
    world.claude.assert_nothing_left_running();
}

#[cfg(unix)]
#[test]
fn a_broker_asked_to_stop_ends_a_finished_turns_process_that_will_not_leave() {
    // The fake never leaves after its result, and the broker waits only half a
    // second for work that is still going when it is asked to stop.
    let world = World::new("lingers", Duration::from_millis(500));
    let updates = world.ask();
    assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    assert!(
        !world.claude.still_running().is_empty(),
        "the process had already gone"
    );

    let stopped = world.stop();
    assert!(stopped.status.success(), "{stopped:?}");

    // It was stopped before the broker went, not left behind by it.
    world.claude.assert_nothing_left_running();
    assert_eq!(world.exits(), 0, "the fake does not leave by itself");
}
