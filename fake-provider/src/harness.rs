//! A scratch directory holding the fake provider under a real CLI's name, and
//! the adapter that runs it.
//!
//! The fake is installed as a hard link to the binary a test package builds
//! around [`crate::run`]: a copy would be open for writing while it is made,
//! and a process another test thread starts at that moment would keep it busy
//! (`ETXTBSY`) when the test runs it. A test that changes an installed CLI
//! must replace it, never write through it, which would change the fake
//! provider itself.
//!
//! A file named `<cli>-scenario` in the directory chooses how the fake
//! behaves, and the fake records what it saw in `<cli>-invocations`,
//! `<cli>-prompts` and `<cli>-pids` (see the README).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use seatline_core::discovery::SearchPath;
use seatline_core::exchange::Timeouts;
use seatline_core::turn::Namespace;
use seatline_providers::claude::{self, Claude};
use seatline_providers::codex::{self, Codex};
use seatline_providers::gemini::Gemini;
use seatline_providers::grok::Grok;

/// Where a test package keeps the fake provider binary it built, and where it
/// may put scratch directories. Both come from the package's own `env!`
/// values, which no library can read for it.
#[derive(Debug, Clone, Copy)]
pub struct Fixtures {
    provider: &'static str,
    scratch: &'static str,
    namespace: &'static str,
}

impl Fixtures {
    /// `provider` is `env!("CARGO_BIN_EXE_<the binary>")` and `scratch` is
    /// `env!("CARGO_TARGET_TMPDIR")`, on the same file system as the binary.
    /// `namespace` names the application the adapters run for.
    pub const fn new(
        provider: &'static str,
        scratch: &'static str,
        namespace: &'static str,
    ) -> Self {
        Self {
            provider,
            scratch,
            namespace,
        }
    }

    /// The namespace of the application the adapters run for.
    pub fn namespace(&self) -> Namespace {
        Namespace::fixed(self.namespace).expect("the fixtures name a valid namespace")
    }

    /// The fake provider binary.
    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// A new, empty directory of its own under the scratch directory.
    fn directory(&self, name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(self.scratch).join(format!(
            "fake-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a fake provider directory");
        dir
    }

    /// Installs the fake provider in `dir` as `file_name`.
    fn install(&self, dir: &std::path::Path, file_name: &str) {
        let path = dir.join(file_name);
        if std::fs::hard_link(self.provider, &path).is_err() {
            std::fs::copy(self.provider, &path).expect("install the fake provider");
        }
    }
}

/// The grace period of a cancel that should stop a process promptly. On POSIX
/// the stop request, SIGTERM, ends the process well inside a long grace period.
/// Windows has no stop request that reaches a process not reading its input,
/// so there a short grace period ends in a kill.
pub const PROMPT_STOP_GRACE: Duration = if cfg!(unix) {
    Duration::from_secs(5)
} else {
    Duration::from_millis(300)
};

/// Short Codex adapter limits, so failures show up quickly.
pub const TEST_LIMITS: codex::Limits = codex::Limits {
    timeouts: Timeouts {
        start: Duration::from_secs(10),
        idle: Duration::from_secs(10),
        max_turn: Duration::from_secs(30),
        stop_grace: Duration::from_millis(300),
    },
    probe: Duration::from_secs(5),
    finish: Duration::from_millis(300),
};

/// Short Claude adapter limits for integration tests.
pub const CLAUDE_TEST_LIMITS: claude::Limits = claude::Limits {
    timeouts: Timeouts {
        start: Duration::from_secs(10),
        idle: Duration::from_secs(10),
        max_turn: Duration::from_secs(30),
        stop_grace: Duration::from_millis(300),
    },
    probe: Duration::from_secs(5),
    finish: Duration::from_millis(300),
};

/// What the fake CLIs record about the launches they ran: the process of each.
macro_rules! launched {
    ($cli:literal) => {
        /// The process ID of each launch that ran a turn.
        pub fn pids(&self) -> Vec<u32> {
            std::fs::read_to_string(self.dir.join(concat!($cli, "-pids")))
                .unwrap_or_default()
                .lines()
                .map(|pid| pid.parse().expect("a pid"))
                .collect()
        }

        /// The launches this directory saw that have not exited and been
        /// reaped yet.
        pub fn still_running(&self) -> Vec<u32> {
            #[cfg(unix)]
            {
                use nix::errno::Errno;
                use nix::sys::signal::kill;
                use nix::unistd::Pid;

                self.pids()
                    .into_iter()
                    .filter(|pid| {
                        let pid = Pid::from_raw(i32::try_from(*pid).expect("pid fits in pid_t"));
                        kill(pid, None) != Err(Errno::ESRCH)
                    })
                    .collect()
            }
            #[cfg(not(unix))]
            Vec::new()
        }

        /// Every launch this directory saw has exited and been reaped.
        pub fn assert_nothing_left_running(&self) {
            assert_eq!(self.still_running(), Vec::<u32>::new(), "still around");
        }
    };
}

/// What the fake CLIs record, read back from the directory that holds them.
macro_rules! recorded {
    ($cli:literal) => {
        pub fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
        }

        /// One line per launch: its command line, and what it ran with.
        pub fn invocations(&self) -> Vec<String> {
            self.read(concat!($cli, "-invocations"))
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// What each launch read from its standard input.
        pub fn prompts(&self) -> Vec<String> {
            self.read(concat!($cli, "-prompts"))
                .split('\0')
                .filter(|prompt| !prompt.is_empty())
                .map(str::to_owned)
                .collect()
        }

        launched!($cli);
    };
}

/// A directory holding a fake `codex` and the scenario it follows.
pub struct FakeCodex {
    pub dir: PathBuf,
}

impl FakeCodex {
    pub fn install(fixtures: Fixtures, exec: &str, login: &str) -> Self {
        let dir = fixtures.directory("codex");
        fixtures.install(&dir, Self::file_name());
        let codex = Self { dir };
        codex.set(exec, login);
        codex
    }

    pub fn file_name() -> &'static str {
        if cfg!(windows) { "codex.exe" } else { "codex" }
    }

    /// Chooses how the fake answers `codex exec` and `codex login status`.
    pub fn set(&self, exec: &str, login: &str) {
        std::fs::write(
            self.dir.join("codex-scenario"),
            format!("exec={exec}\nlogin={login}\n"),
        )
        .expect("write the scenario");
    }

    /// The Codex adapter, pointed at this directory, with short limits.
    pub fn adapter(&self) -> Codex {
        self.adapter_with(TEST_LIMITS)
    }

    pub fn adapter_with(&self, limits: codex::Limits) -> Codex {
        Codex::new(SearchPath::new([self.dir.clone()]), self.dir.join("work")).with_limits(limits)
    }

    /// [`FakeCodex::adapter`] with the environment Codex gets from `host`.
    pub fn adapter_with_env<I>(&self, host: I) -> Codex
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        self.adapter().with_environment(host)
    }

    recorded!("codex");
}

impl Drop for FakeCodex {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A directory holding a fake `claude` CLI and its scenario.
pub struct FakeClaude {
    pub dir: PathBuf,
}

impl FakeClaude {
    pub fn install(fixtures: Fixtures, print: &str, auth: &str) -> Self {
        let dir = fixtures.directory("claude");
        fixtures.install(&dir, Self::file_name());
        let fake = Self { dir };
        fake.set(print, auth);
        fake
    }

    pub fn file_name() -> &'static str {
        if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        }
    }

    /// Chooses how the fake answers `claude -p` and `claude auth status`.
    pub fn set(&self, print: &str, auth: &str) {
        std::fs::write(
            self.dir.join("claude-scenario"),
            format!("print={print}\nauth={auth}\n"),
        )
        .expect("write Claude scenario");
    }

    /// The Claude adapter, pointed at this directory, with short limits.
    pub fn adapter(&self) -> Claude {
        self.adapter_with(CLAUDE_TEST_LIMITS)
    }

    pub fn adapter_with(&self, limits: claude::Limits) -> Claude {
        Claude::new(
            SearchPath::new([self.dir.clone()]),
            self.dir.join("claude-work"),
        )
        .with_limits(limits)
    }

    /// [`FakeClaude::adapter`] with the environment Claude gets from `host`.
    pub fn adapter_with_env<I>(&self, host: I) -> Claude
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        self.adapter().with_environment(host)
    }

    recorded!("claude");
}

impl Drop for FakeClaude {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A directory holding a fake `agy` (Antigravity) and a home for it.
pub struct FakeGemini {
    pub dir: PathBuf,
    pub home: PathBuf,
    namespace: Namespace,
}

impl FakeGemini {
    recorded!("agy");

    pub fn install(fixtures: Fixtures) -> Self {
        let dir = fixtures.directory("agy");
        fixtures.install(&dir, if cfg!(windows) { "agy.exe" } else { "agy" });
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self {
            dir,
            home,
            namespace: fixtures.namespace(),
        }
    }

    /// The environment the adapter gets: this home, and this `PATH`.
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        let home_name = if cfg!(unix) { "HOME" } else { "USERPROFILE" };
        vec![
            (
                OsString::from(home_name),
                self.home.as_os_str().to_os_string(),
            ),
            (OsString::from("PATH"), self.dir.as_os_str().to_os_string()),
        ]
    }

    /// The Gemini adapter, pointed at this directory.
    pub fn adapter(&self) -> Gemini {
        Gemini::new(
            &self.namespace,
            SearchPath::new([self.dir.clone()]),
            self.dir.join("workspace"),
        )
        .with_environment(self.environment())
    }

    /// Where Antigravity keeps the transcripts of its conversations.
    pub fn brain(&self) -> PathBuf {
        self.home.join(".gemini/antigravity-cli/brain")
    }

    /// Where Antigravity keeps the databases of its conversations, prompts
    /// included.
    pub fn conversations(&self) -> PathBuf {
        self.home.join(".gemini/antigravity-cli/conversations")
    }

    /// How many things Antigravity keeps of its turns: transcripts and
    /// conversation databases.
    pub fn kept(&self) -> usize {
        [self.brain(), self.conversations()]
            .iter()
            .map(|dir| std::fs::read_dir(dir).map_or(0, |entries| entries.count()))
            .sum()
    }
}

impl Drop for FakeGemini {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A directory holding a fake `grok` and a home with a cached sign-in.
pub struct FakeGrok {
    pub dir: PathBuf,
    pub home: PathBuf,
    namespace: Namespace,
}

impl FakeGrok {
    recorded!("grok");

    pub fn install(fixtures: Fixtures) -> Self {
        let dir = fixtures.directory("grok");
        fixtures.install(&dir, if cfg!(windows) { "grok.exe" } else { "grok" });
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(home.join(".grok/auth.json"), "{}").unwrap();
        Self {
            dir,
            home,
            namespace: fixtures.namespace(),
        }
    }

    /// The environment the adapter gets: this home, this `PATH`, and an API
    /// key that must never reach Grok.
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        let home_name = if cfg!(unix) { "HOME" } else { "USERPROFILE" };
        vec![
            (
                OsString::from(home_name),
                self.home.as_os_str().to_os_string(),
            ),
            (OsString::from("PATH"), self.dir.as_os_str().to_os_string()),
            (
                OsString::from("XAI_API_KEY"),
                OsString::from("must-not-leak"),
            ),
        ]
    }

    /// The Grok adapter, pointed at this directory.
    pub fn adapter(&self) -> Grok {
        self.adapter_with_environment(&[])
    }

    /// The Grok adapter with more variables in the environment it gets.
    pub fn adapter_with_environment(&self, extra: &[(&str, PathBuf)]) -> Grok {
        let mut environment = self.environment();
        environment.extend(
            extra
                .iter()
                .map(|(name, value)| (OsString::from(name), value.as_os_str().to_os_string())),
        );
        Grok::new(
            &self.namespace,
            SearchPath::new([self.dir.clone()]),
            self.dir.join("workspace"),
        )
        .with_environment(environment)
    }

    /// The per-turn workspaces Grok has now, live or left behind.
    pub fn turn_dirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.dir.join("workspace"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("turn-"))
            })
            .collect()
    }
}

impl Drop for FakeGrok {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
