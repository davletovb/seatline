//! The isolated environment a scenario runs in: a scratch directory, the fake
//! provider installed under a real CLI's name, a broker data directory per
//! broker, and the processes that use them.
//!
//! Everything a broker or an application needs to find its way is passed in the
//! environment of the process the harness starts; the harness never changes
//! its own.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::{Stream, prelude::*};
use seatline_companion::{client, config, telemetry};
use seatline_core::turn::Namespace;
use seatline_platform::layout::Layout;
use serde_json::Value;

use crate::workload::{Sample, Spec};

/// What the harness was told about where things are.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The `seatline-companion` executable.
    pub companion: PathBuf,
    /// The fake provider executable, for the deterministic mode.
    pub fake_provider: PathBuf,
    /// This executable: simulated applications are run as itself.
    pub harness: PathBuf,
    /// The live provider to measure, or `None` for the fake one.
    pub live: Option<String>,
    /// Where scratch directories go. Providers refuse to run in a directory
    /// that anyone else could change or replace, and `/tmp` is such a place,
    /// so the default is under the user's cache directory.
    pub scratch: PathBuf,
    /// Keep the scratch directories, for looking at what a run left.
    pub keep: bool,
    /// The owner's `scheduling.json`, written into every scratch broker's data
    /// directory, or `None` for the broker's defaults. How a run is made with
    /// a setting on and then off.
    pub policy: Option<Vec<u8>>,
}

/// The variables that make the harness stand in for the companion when a
/// client starts it: the real companion to run, and the log to count in.
pub const SHIM_COMPANION: &str = "SEATLINE_BENCH_REAL_COMPANION";
pub const SHIM_LOG: &str = "SEATLINE_BENCH_SPAWN_LOG";

/// How long a broker the harness starts for a warm scenario may sit idle before
/// it leaves. It is never idle that long while a scenario runs, and a broker
/// whose harness was killed does not outlive it for long.
pub const WARM_IDLE_SECS: u64 = 120;

/// The default place for scratch directories: private to the user.
pub fn default_scratch() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".cache"))
                    .filter(|path| path.is_absolute())
            })
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("seatline-bench")
}

impl Settings {
    /// The provider the applications ask for.
    pub fn provider(&self) -> &str {
        self.live.as_deref().unwrap_or("codex")
    }
}

/// A broker's data directory, with its own grants, sessions and telemetry.
#[derive(Debug, Clone)]
pub struct Instance {
    pub root: PathBuf,
    pub telemetry: PathBuf,
}

pub struct Lab {
    pub settings: Settings,
    pub scratch: PathBuf,
    /// Where the fake provider CLIs are installed.
    pub providers: PathBuf,
    home: PathBuf,
    instances: usize,
    /// What a broker said it ran with, from the first telemetry that had it.
    pub broker_record: Option<Value>,
    /// Where the starts of the companion are logged, when the applications
    /// start it through the harness's own shim to be counted.
    spawn_log: Option<PathBuf>,
}

impl Drop for Lab {
    fn drop(&mut self) {
        if !self.settings.keep {
            let _ = std::fs::remove_dir_all(&self.scratch);
        }
    }
}

impl Lab {
    pub fn new(settings: &Settings) -> io::Result<Self> {
        // Short: a socket's path has a length limit.
        let scratch = settings.scratch.join(format!(
            "{}-{}",
            std::process::id(),
            &config::random_token()?[..6]
        ));
        // A Unix socket's path has a length limit (104 bytes on macOS), and the
        // broker's socket is in a data directory below this one. Say so now
        // rather than let the broker fail to bind.
        let longest = scratch.join("data-999").join("broker.sock");
        if cfg!(unix) && longest.as_os_str().len() > 100 {
            return Err(io::Error::other(format!(
                "the scratch directory's path is too long for a Unix socket ({} bytes, at most 100 allowed); pass a shorter one with --scratch",
                longest.as_os_str().len()
            )));
        }
        let providers = scratch.join("providers");
        let home = scratch.join("home");
        for dir in [&providers, &home.join(".cache")] {
            std::fs::create_dir_all(dir)?;
        }
        let lab = Self {
            settings: settings.clone(),
            scratch,
            providers,
            home,
            instances: 0,
            broker_record: None,
            spawn_log: None,
        };
        if settings.live.is_none() {
            lab.install_fake_codex()?;
        }
        Ok(lab)
    }

    /// The fake `codex`: a copy of the fake provider binary under that name,
    /// whose behavior is chosen by the first word of each question.
    ///
    /// A copy, not a link: a link shares the binary's identity with every other
    /// run that links it, so one run's install changes the change time the
    /// provider's readiness evidence is tied to, and a `prepare` would find its
    /// evidence expired by a stranger. The copy is finished and closed before
    /// anything is started, so nothing can be told the file is busy.
    fn install_fake_codex(&self) -> io::Result<()> {
        let name = if cfg!(windows) { "codex.exe" } else { "codex" };
        let target = self.providers.join(name);
        std::fs::copy(&self.settings.fake_provider, &target)?;
        std::fs::write(
            self.providers.join("codex-scenario"),
            "exec=by-prompt\nlogin=signed-in\n",
        )
    }

    /// What the fake provider was asked to do so far: how many sign-in
    /// probes (`codex login status`) and how many turns (`codex exec`).
    pub fn fake_invocations(&self) -> (u64, u64) {
        let text =
            std::fs::read_to_string(self.providers.join("codex-invocations")).unwrap_or_default();
        let count = |prefix: &str| text.lines().filter(|l| l.starts_with(prefix)).count() as u64;
        (count("login status"), count("exec "))
    }

    /// From here on, applications that start the companion do it through the
    /// harness's own shim, which logs each start before running the real one.
    /// Every start costs one more process hop, so only a scenario that counts
    /// starts asks for it.
    pub fn count_companion_starts(&mut self) {
        self.spawn_log = Some(self.scratch.join("companion-starts.log"));
    }

    /// How many times the companion was started through the shim: each is one
    /// `serve` the applications' clients asked for, whether it went on to
    /// serve or found another broker already did.
    pub fn companion_starts(&self) -> Option<u64> {
        let log = self.spawn_log.as_ref()?;
        let text = std::fs::read_to_string(log).unwrap_or_default();
        Some(text.lines().count() as u64)
    }

    /// Keeps the broker's own record of its configuration, once.
    pub fn remember_broker(&mut self, records: &[Value]) {
        if self.broker_record.is_none() {
            self.broker_record = records.iter().find(|r| r["kind"] == "broker").cloned();
        }
    }

    /// A new broker data directory with `apps` authorized for the provider.
    #[allow(clippy::disallowed_methods)] // The harness runs the companion it was pointed at, never a request-supplied program.
    pub fn instance(&mut self, apps: &[&str]) -> io::Result<Instance> {
        self.instances += 1;
        let root = self.scratch.join(format!("data-{}", self.instances));
        for app in apps {
            let status = Command::new(&self.settings.companion)
                .env("SEATLINE_DATA_DIR", &root)
                .args(["authorize", app, self.settings.provider()])
                .stdout(Stdio::null())
                .status()?;
            if !status.success() {
                return Err(io::Error::other(format!("could not authorize {app}")));
            }
        }
        if let Some(policy) = &self.settings.policy {
            std::fs::write(root.join("scheduling.json"), policy)?;
        }
        Ok(Instance {
            telemetry: root.with_extension("telemetry.jsonl"),
            root,
        })
    }

    /// The environment for a broker, or for an application that may start one.
    /// `idle_secs` is how long a broker stays up with nobody connected.
    pub fn environment(
        &self,
        instance: &Instance,
        apps: &[&str],
        idle_secs: u64,
    ) -> io::Result<Vec<(OsString, OsString)>> {
        let mut env: Vec<(OsString, OsString)> = vec![
            ("SEATLINE_DATA_DIR".into(), instance.root.clone().into()),
            (
                "SEATLINE_COMPANION_BIN".into(),
                match &self.spawn_log {
                    Some(_) => self.settings.harness.clone().into(),
                    None => self.settings.companion.clone().into(),
                },
            ),
            (
                telemetry::FILE_VARIABLE.into(),
                instance.telemetry.clone().into(),
            ),
            (
                "SEATLINE_BROKER_IDLE_SECS".into(),
                idle_secs.to_string().into(),
            ),
        ];
        if let Some(log) = &self.spawn_log {
            env.push((
                SHIM_COMPANION.into(),
                self.settings.companion.clone().into(),
            ));
            env.push((SHIM_LOG.into(), log.clone().into()));
        }
        if self.settings.live.is_none() {
            // The fake provider is the only one the apps can find, and
            // everything they write goes under the scratch directory.
            env.push(("HOME".into(), self.home.clone().into()));
            env.push(("XDG_CACHE_HOME".into(), self.home.join(".cache").into()));
            for app in apps {
                let namespace = Namespace::fixed(*app)
                    .map_err(|_| io::Error::other("not a valid app identifier"))?;
                env.push((
                    Layout::new(namespace).search_path_variable().into(),
                    self.providers.clone().into(),
                ));
            }
        }
        Ok(env)
    }

    /// Starts a broker that stays up, and waits until it accepts connections.
    #[allow(clippy::disallowed_methods)] // The harness runs the companion it was pointed at, never a request-supplied program.
    pub fn start_broker(&self, instance: &Instance, apps: &[&str]) -> io::Result<Broker> {
        let child = Command::new(&self.settings.companion)
            .arg("serve")
            .envs(self.environment(instance, apps, WARM_IDLE_SECS)?)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let broker = Broker(child);
        wait_until_listening(&instance.root)?;
        Ok(broker)
    }

    /// Starts one simulated application and waits until it is ready.
    #[allow(clippy::disallowed_methods)] // The harness runs itself as an application, never a request-supplied program.
    pub fn spawn_app(
        &self,
        instance: &Instance,
        apps: &[&str],
        idle_secs: u64,
        spec: &Spec,
    ) -> io::Result<App> {
        let path = self
            .scratch
            .join(format!("spec-{}-{}.json", spec.app, self.instances));
        std::fs::write(&path, serde_json::to_vec(spec)?)?;
        let mut child = Command::new(&self.settings.harness)
            .arg("app")
            .arg(&path)
            .envs(self.environment(instance, apps, idle_secs)?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let mut reader = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("no pipe"))?,
        );
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if serde_json::from_str::<Value>(&line).map_or(true, |value| value["ready"] != true) {
            let _ = child.kill();
            return Err(io::Error::other("the simulated application did not start"));
        }
        Ok(App {
            child,
            stdin,
            reader: Some(reader),
            thread: None,
        })
    }
}

/// A broker the harness started, stopped when it goes out of scope.
pub struct Broker(Child);

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

fn wait_until_listening(root: &Path) -> io::Result<()> {
    let give_up = Instant::now() + Duration::from_secs(15);
    runtime()?.block_on(async {
        loop {
            if Stream::connect(client::socket_name(root)?).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= give_up {
                return Err(io::Error::other("the broker did not start listening"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
}

/// Waits until no broker holds `root`, which is how a cold start finishes
/// cleanly: the broker a client started exits by itself once idle.
pub fn wait_until_stopped(root: &Path) -> io::Result<()> {
    let give_up = Instant::now() + Duration::from_secs(30);
    loop {
        match config::lock(root, "broker.lock") {
            Ok(lock) => {
                drop(lock);
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= give_up {
            return Err(io::Error::other("the broker did not stop"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A simulated application the harness is running.
pub struct App {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: Option<BufReader<ChildStdout>>,
    thread: Option<JoinHandle<io::Result<Vec<Sample>>>>,
}

impl App {
    /// Lets it start making its requests.
    pub fn go(&mut self) -> io::Result<()> {
        let mut stdin = self
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("already started"))?;
        stdin.write_all(b"go\n")?;
        drop(stdin);
        let reader = self
            .reader
            .take()
            .ok_or_else(|| io::Error::other("already started"))?;
        self.thread = Some(std::thread::spawn(move || read_samples(reader)));
        Ok(())
    }

    /// Waits for it to finish and returns what it measured.
    pub fn finish(mut self) -> io::Result<Vec<Sample>> {
        let samples = self
            .thread
            .take()
            .ok_or_else(|| io::Error::other("not started"))?
            .join()
            .map_err(|_| io::Error::other("the reader panicked"))??;
        let status = self.child.wait()?;
        if !status.success() {
            return Err(io::Error::other("the simulated application failed"));
        }
        Ok(samples)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_samples(reader: BufReader<ChildStdout>) -> io::Result<Vec<Sample>> {
    let mut samples = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let value: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
        if value["done"] == true {
            return Ok(samples);
        }
        samples.push(serde_json::from_value(value).map_err(io::Error::other)?);
    }
    Err(io::Error::other(
        "the simulated application ended before it finished",
    ))
}

/// The records a broker wrote, one JSON value each. A file that was never
/// written is no records.
pub fn read_telemetry(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The harness run as `serve`, which is how a client starts the companion:
/// log that it was asked to, then become the real companion, so the broker
/// that results is the one the client would have had.
#[allow(clippy::disallowed_methods)] // Runs the companion this harness was pointed at, never a request-supplied program.
pub fn shim_serve() -> io::Result<()> {
    let real = std::env::var_os(SHIM_COMPANION)
        .ok_or_else(|| io::Error::other("not started as the companion's stand-in"))?;
    if let Some(log) = std::env::var_os(SHIM_LOG) {
        // One short line per start, in a single write, so that starts that
        // happen together do not mix their lines.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        file.write_all(b"serve\n")?;
    }
    let mut command = Command::new(real);
    command.arg("serve");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("the companion failed"))
        }
    }
}
