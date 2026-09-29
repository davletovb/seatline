//! Test-only fake provider, and a harness for testing provider adapters
//! against it. See README.md for the mode contract.
//!
//! [`run`] is the fake CLI's `main`: each repository that tests against it
//! builds a one-line binary around it, so the tests can start the binary
//! through `CARGO_BIN_EXE_*`. It acts as a fake Codex, Claude, Antigravity or
//! Grok when its file name is that CLI's, and as the mode-driven fake provider
//! otherwise. [`harness`] installs it under those names in a scratch
//! directory, and builds adapters that run it.

pub mod harness;
pub mod resources;

mod claude;
mod codex;
mod gemini;
mod grok;

use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const EXIT_FAILURE: u8 = 1;
const EXIT_NONZERO_MODE: u8 = 42;
const EXIT_USAGE: u8 = 64;

const LARGE_OUTPUT_SIZE: usize = 2 * 1024 * 1024;
const SLOW_DELAY: Duration = Duration::from_millis(700);

const NORMAL_LINES: [&str; 3] = [
    r#"{"type":"delta","text":"alpha"}"#,
    r#"{"type":"delta","text":" beta"}"#,
    r#"{"type":"completed"}"#,
];
const READY_LINE: &str = r#"{"type":"ready"}"#;
/// The start of a line that never ends.
const PARTIAL_LINE: &str = r#"{"type":"delta","text":"unfinish"#;

#[derive(Debug, Clone, Copy)]
enum Mode {
    Normal,
    Slow,
    Stderr,
    ExitNonzero,
    Hang,
    IgnoreCancel,
    Malformed,
    Large,
    Crash,
    Echo,
    Tree,
    Orphan,
    Escape,
    Detached,
    Partial,
    Env,
    Args,
}

impl Mode {
    fn parse(name: &OsStr) -> Option<Self> {
        Some(match name.to_str()? {
            "normal" => Self::Normal,
            "slow" => Self::Slow,
            "stderr" => Self::Stderr,
            "exit-nonzero" => Self::ExitNonzero,
            "hang" => Self::Hang,
            "ignore-cancel" => Self::IgnoreCancel,
            "malformed" => Self::Malformed,
            "large" => Self::Large,
            "crash" => Self::Crash,
            "echo" => Self::Echo,
            "tree" => Self::Tree,
            "orphan" => Self::Orphan,
            "escape" => Self::Escape,
            "detached" => Self::Detached,
            "partial" => Self::Partial,
            "env" => Self::Env,
            "args" => Self::Args,
            _ => return None,
        })
    }
}

/// The fake provider's `main`.
/// Appends `text` to `<cli>-<what>` beside the executable, for a test to read
/// back what the adapter sent.
fn record(cli: &str, what: &str, text: &str) {
    use std::io::Write as _;

    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{cli}-{what}")))
    {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Records that this process ran a turn, as one line in `<cli>-pids`, for a
/// test to check that it was reaped.
fn record_launch(cli: &str) {
    record(cli, "pids", &format!("{}\n", std::process::id()));
}

pub fn run() -> ExitCode {
    let mut args = std::env::args_os();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("fake-provider"));
    let arguments: Vec<OsString> = args.collect();
    if codex::is_codex(&program) {
        return codex::main(arguments);
    }
    if claude::is_claude(&program) {
        return claude::main(arguments);
    }
    if gemini::is_gemini(&program) {
        return gemini::main(arguments);
    }
    if grok::is_grok(&program) {
        return grok::main(arguments);
    }

    let mode = match arguments.as_slice() {
        [flag, mode] if flag == OsStr::new("--mode") => Mode::parse(mode),
        // Only `args` takes arguments of its own.
        [flag, mode, ..] if flag == OsStr::new("--mode") && mode == OsStr::new("args") => {
            Some(Mode::Args)
        }
        _ => None,
    };
    let Some(mode) = mode else {
        let _ = writeln!(
            io::stderr(),
            "usage: {} --mode <normal|slow|stderr|exit-nonzero|hang|ignore-cancel|malformed|large|crash|echo|tree|orphan|escape|detached|partial|env|args [argument...]>",
            Path::new(&program).display()
        );
        return ExitCode::from(EXIT_USAGE);
    };

    match run_mode(mode, arguments.get(2..).unwrap_or_default()) {
        Ok(status) => ExitCode::from(status),
        Err(_) => ExitCode::from(EXIT_FAILURE),
    }
}

fn run_mode(mode: Mode, rest: &[OsString]) -> io::Result<u8> {
    match mode {
        Mode::Normal => stream_lines(false)?,
        Mode::Slow => stream_lines(true)?,
        Mode::Stderr => {
            write_line(
                &mut io::stderr(),
                "fake-provider: deterministic stderr message",
            )?;
            stream_lines(false)?;
        }
        Mode::ExitNonzero => {
            write_line(&mut io::stderr(), "fake-provider: exiting with status 42")?;
            return Ok(EXIT_NONZERO_MODE);
        }
        Mode::Hang => {
            write_line(&mut io::stdout(), READY_LINE)?;
            hang_forever();
        }
        Mode::IgnoreCancel => {
            // Ignore SIGTERM before announcing readiness so a supervisor that
            // signals right after the ready line always hits the ignored state.
            ignore_termination_signal()?;
            write_line(&mut io::stdout(), READY_LINE)?;
            if read_command().starts_with(b"cancel") {
                write_line(&mut io::stderr(), "fake-provider: cancellation ignored")?;
            }
            hang_forever();
        }
        Mode::Malformed => write_line(&mut io::stdout(), "{not-json")?,
        Mode::Large => write_large_output()?,
        Mode::Crash => {
            disable_core_dumps();
            std::process::abort();
        }
        Mode::Echo => {
            io::copy(&mut io::stdin().lock(), &mut io::stdout().lock())?;
            io::stdout().flush()?;
        }
        Mode::Tree => {
            spawn_descendant("hang")?;
            write_line(&mut io::stdout(), READY_LINE)?;
            hang_forever();
        }
        Mode::Orphan => spawn_descendant("hang")?,
        Mode::Escape => spawn_descendant("detached")?,
        Mode::Partial => {
            let mut stdout = io::stdout();
            stdout.write_all(PARTIAL_LINE.as_bytes())?;
            stdout.flush()?;
            hang_forever();
        }
        Mode::Env => {
            let environment: serde_json::Map<String, serde_json::Value> = std::env::vars_os()
                .map(|(name, value)| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned().into(),
                    )
                })
                .collect();
            let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
            let launch = serde_json::json!({"cwd": cwd, "env": environment});
            write_line(&mut io::stdout(), &launch.to_string())?;
        }
        Mode::Args => {
            let args: Vec<String> = rest
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            write_line(
                &mut io::stdout(),
                &serde_json::Value::from(args).to_string(),
            )?;
        }
        Mode::Detached => {
            leave_process_group()?;
            let line = format!(r#"{{"type":"detached","pid":{}}}"#, std::process::id());
            write_line(&mut io::stdout(), &line)?;
            hang_forever();
        }
    }
    Ok(0)
}

/// Starts this executable in `mode` as a child that shares this process's
/// stdout and stderr, and leaves it running.
#[allow(
    clippy::disallowed_methods,
    reason = "the fake provider spawns a descendant to test process-tree cleanup"
)]
fn spawn_descendant(mode: &str) -> io::Result<()> {
    Command::new(std::env::current_exe()?)
        .args(["--mode", mode])
        .stdin(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Writes one line and flushes it so supervisors observe each line promptly.
fn write_line<W: Write>(stream: &mut W, line: &str) -> io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn stream_lines(slow: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    for (index, line) in NORMAL_LINES.iter().enumerate() {
        write_line(&mut stdout, line)?;
        if slow && index + 1 < NORMAL_LINES.len() {
            thread::sleep(SLOW_DELAY);
        }
    }
    Ok(())
}

/// Emits exactly [`LARGE_OUTPUT_SIZE`] bytes with no newline.
fn write_large_output() -> io::Result<()> {
    let block = [b'x'; 4096];
    let mut stdout = io::stdout().lock();
    let mut remaining = LARGE_OUTPUT_SIZE;
    while remaining > 0 {
        let count = remaining.min(block.len());
        stdout.write_all(&block[..count])?;
        remaining -= count;
    }
    stdout.flush()
}

/// Reads one command line from stdin, up to 63 bytes.
fn read_command() -> Vec<u8> {
    let mut command = Vec::new();
    // End of input or a read error both mean "no command".
    let _ = io::stdin().lock().take(63).read_until(b'\n', &mut command);
    command
}

fn hang_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

/// Keeps SIGTERM from terminating the process; only SIGKILL stops it.
#[cfg(unix)]
fn ignore_termination_signal() -> io::Result<()> {
    use nix::sys::signal::{SigSet, Signal};

    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    signals.thread_block().map_err(io::Error::from)
}

/// Windows has no catchable termination signal; `TerminateProcess` always wins.
#[cfg(not(unix))]
fn ignore_termination_signal() -> io::Result<()> {
    Ok(())
}

/// Keeps `crash` from leaving a core file behind.
#[cfg(unix)]
fn disable_core_dumps() {
    use nix::sys::resource::{Resource, setrlimit};

    let _ = setrlimit(Resource::RLIMIT_CORE, 0, 0);
}

#[cfg(not(unix))]
fn disable_core_dumps() {}

/// Moves this process into a new session, out of its parent's process group,
/// so signals sent to that group no longer reach it.
#[cfg(unix)]
fn leave_process_group() -> io::Result<()> {
    nix::unistd::setsid().map(drop).map_err(io::Error::from)
}

#[cfg(not(unix))]
fn leave_process_group() -> io::Result<()> {
    Ok(())
}
