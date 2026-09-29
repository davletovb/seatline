//! A fake `codex` CLI for the Codex adapter's tests. This binary acts as it
//! when it runs under the name `codex` (a link to it, in a test directory).
//!
//! It answers `codex login status` and `codex exec --json [...] [resume <id>] -`
//! with the event shapes Codex CLI 0.156 prints. What it does comes from the
//! file `codex-scenario` next to it, whose lines read `login=<behavior>` and
//! `exec=<behavior>`. Each run appends its arguments to `codex-invocations`,
//! and its environment and working directory to `codex-environment`, and
//! each `exec` its prompt to `codex-prompts` and its pid to `codex-pids`, so
//! tests can check what the adapter sent.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

/// A fake credential, written to stderr: it must never reach an application's events
/// or logs.
const SECRET: &str = "sk-live-SECRET-9d2f";

/// How long the endless behaviors, and the hangs, keep going, so a stuck or
/// killed test still ends.
const ENDLESS: Duration = Duration::from_secs(60);

/// stderr written by `stderr-flood` while it answers.
const STDERR_FLOOD_BYTES: usize = 128 * 1024 * 1024;

/// Progress events `stdout-flood` reports before its answer.
const STDOUT_FLOOD_EVENTS: usize = 100_000;

/// Events a flood writes at once.
const FLOOD_BATCH: usize = 500;

/// Whether this process runs as the fake `codex`.
pub fn is_codex(program: &OsString) -> bool {
    Path::new(program)
        .file_stem()
        .is_some_and(|stem| stem == "codex")
}

pub fn main(args: Vec<OsString>) -> ExitCode {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let scenario = std::fs::read_to_string(dir.join("codex-scenario")).unwrap_or_default();
    let setting = |key: &str| {
        scenario
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
            .to_owned()
    };
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let first_path_entry = std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).next())
        .unwrap_or_default();
    append(
        &dir.join("codex-invocations"),
        &format!("{}\tPATH0={}\n", args.join(" "), first_path_entry.display()),
    );
    let environment: serde_json::Map<String, serde_json::Value> = std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                json!(value.to_string_lossy()),
            )
        })
        .collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    append(
        &dir.join("codex-environment"),
        &format!(
            "{}\n",
            json!({"command": args.first(), "cwd": cwd.to_string_lossy(), "env": environment})
        ),
    );

    match args.first().map(String::as_str) {
        Some("login") if args.get(1).map(String::as_str) == Some("status") => {
            login_status(&setting("login"))
        }
        Some("exec") => match exec(&dir, &args, &setting("exec")) {
            Ok(code) => code,
            Err(_) => ExitCode::from(1),
        },
        _ => {
            let _ = writeln!(io::stderr(), "fake codex: unsupported command");
            ExitCode::from(2)
        }
    }
}

fn login_status(behavior: &str) -> ExitCode {
    let mut stderr = io::stderr();
    match behavior {
        "signed-out" => {
            let _ = writeln!(stderr, "Not logged in");
            ExitCode::from(1)
        }
        "broken" => {
            let _ = writeln!(stderr, "Error loading configuration");
            ExitCode::from(3)
        }
        "hangs" => hang(),
        // Output without end, on both streams: the check must still give up.
        "floods" => {
            thread::spawn(|| flood_stderr(usize::MAX, ENDLESS));
            let _ = flood(
                &mut io::stdout().lock(),
                &json!({"account": "someone@example.com"}),
                usize::MAX,
            );
            ExitCode::SUCCESS
        }
        // Like Codex, it names a masked key: the adapter must never read it.
        _ => {
            let _ = writeln!(stderr, "Logged in using an API key - sk-fake***0000");
            ExitCode::SUCCESS
        }
    }
}

fn exec(dir: &Path, args: &[String], behavior: &str) -> io::Result<ExitCode> {
    append(
        &dir.join("codex-pids"),
        &format!("{}\n", std::process::id()),
    );
    let resume = args
        .iter()
        .position(|arg| arg == "resume")
        .and_then(|index| args.get(index + 1))
        .cloned();
    if !args.iter().any(|arg| arg == "--json") || args.last().map(String::as_str) != Some("-") {
        let _ = writeln!(
            io::stderr(),
            "fake codex: expected --json and a prompt on stdin"
        );
        return Ok(ExitCode::from(2));
    }

    // Like Codex, read the whole prompt before doing anything.
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt)?;
    append(&dir.join("codex-prompts"), &format!("{prompt}\u{0}"));

    // `by-prompt`: the question's first word names the behavior, so one host
    // can run different behaviors side by side.
    let behavior = match behavior {
        "by-prompt" => prompt.split_whitespace().next().unwrap_or_default(),
        behavior => behavior,
    };
    if matches!(behavior, "ignores-cancel" | "floods-and-ignores-cancel") {
        ignore_termination_signal();
    }
    // Secrets on stderr must never reach an application's events or logs.
    let _ = writeln!(io::stderr(), "fake codex: token {SECRET}");

    if behavior == "dribble" {
        return dribble(&prompt);
    }
    if behavior == "resume-fails" {
        if let Some(thread_id) = &resume {
            let _ = writeln!(
                io::stderr(),
                "Error: thread/resume: no rollout found for thread id {thread_id}"
            );
            return Ok(ExitCode::from(1));
        }
    }

    // Floods stderr while it answers: a host that kept stderr, or stopped
    // reading stdout while stderr flows, would grow or stall.
    let stderr_flood = (behavior == "stderr-flood")
        .then(|| thread::spawn(|| flood_stderr(STDERR_FLOOD_BYTES, ENDLESS)));

    let thread_id = resume.unwrap_or_else(|| format!("thread-{}", std::process::id()));
    let mut out = io::stdout().lock();
    emit(
        &mut out,
        &json!({"type": "thread.started", "thread_id": thread_id}),
    )?;
    match behavior {
        "never-starts" => hang(),
        "malformed" => {
            writeln!(out, "{{not json")?;
            out.flush()?;
            hang()
        }
        _ => {}
    }
    emit(
        &mut out,
        &json!({"type": "item.completed", "item": {"id": "item_0", "type": "error", "message": "Model metadata not found. Defaulting to fallback metadata."}}),
    )?;
    emit(&mut out, &json!({"type": "turn.started"}))?;
    let native_search = args
        .windows(2)
        .any(|pair| pair == ["-c", "web_search=\"live\""]);
    if native_search && behavior == "search-narrates" {
        // Codex sometimes says what it's about to do before it searches.
        emit(
            &mut out,
            &agent_message("item_narration", "I'll search the web for that."),
        )?;
    }
    if native_search {
        // Real Codex exec reports the search query/action here, not results.
        emit(
            &mut out,
            &json!({
                "type": "item.completed",
                "item": {
                    "id": "search_1",
                    "type": "web_search",
                    "query": prompt.trim(),
                    "action": {"type": "search", "query": prompt.trim()}
                }
            }),
        )?;
    }

    match behavior {
        "goes-quiet" | "ignores-cancel" => hang(),
        "crashes" => {
            super::disable_core_dumps();
            std::process::abort()
        }
        "exits-nonzero" => {
            let _ = writeln!(io::stderr(), "fake codex: giving up");
            return Ok(ExitCode::from(3));
        }
        "no-result" => return Ok(ExitCode::SUCCESS),
        // stderr is not progress, however much of it there is.
        "endless-stderr" => {
            flood_stderr(usize::MAX, ENDLESS);
            hang()
        }
        // Progress without end: only a cancel or closing the input ends it.
        "endless-flood" | "floods-and-ignores-cancel" => {
            flood(&mut out, &progress(0), usize::MAX)?;
            return Ok(ExitCode::SUCCESS);
        }
        // Events the adapter doesn't know, without end: not progress either.
        "unknown-flood" => {
            flood(
                &mut out,
                &json!({"type": "future.event", "detail": "x"}),
                usize::MAX,
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        "invalid-utf8" => {
            out.write_all(
                b"{\"type\":\"item.completed\",\"item\":{\"id\":\"item_2\",\"type\":\"agent_message\",\"text\":\"\xff\xfe\"}}\n",
            )?;
            out.flush()?;
            hang()
        }
        // One line that never ends.
        "endless-line" => {
            out.write_all(
                b"{\"type\":\"item.completed\",\"item\":{\"id\":\"item_2\",\"type\":\"agent_message\",\"text\":\"",
            )?;
            let block = [b'x'; 64 * 1024];
            let give_up = Instant::now() + ENDLESS;
            while Instant::now() < give_up {
                out.write_all(&block)?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        "fails-401" | "fails-429" | "fails-500" => {
            let message = match behavior {
                "fails-401" => {
                    "unexpected status 401 Unauthorized: Incorrect API key provided: sk-abc***xyz"
                }
                "fails-429" => "exceeded retry limit, last status: 429 Too Many Requests",
                _ => "We're currently experiencing high demand, which may cause temporary errors.",
            };
            emit(&mut out, &json!({"type": "error", "message": message}))?;
            emit(
                &mut out,
                &json!({"type": "turn.failed", "error": {"message": message}}),
            )?;
            return Ok(ExitCode::from(1));
        }
        "oversized" => {
            let text = "x".repeat(9 * 1024 * 1024);
            emit(&mut out, &agent_message("item_2", &text))?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    emit(
        &mut out,
        &json!({"type": "item.started", "item": {"id": "item_1", "type": "command_execution", "command": "ls", "aggregated_output": "", "exit_code": null, "status": "in_progress"}}),
    )?;
    emit(
        &mut out,
        &json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution", "command": "ls", "aggregated_output": "", "exit_code": 0, "status": "completed"}}),
    )?;
    match behavior {
        "stdout-flood" => {
            flood(&mut out, &progress(0), STDOUT_FLOOD_EVENTS)?;
            emit(&mut out, &agent_message("item_2", "Done flooding."))?;
        }
        "two-messages" => {
            emit(&mut out, &agent_message("item_2", "First."))?;
            emit(&mut out, &agent_message("item_3", "Second."))?;
        }
        "slow" => {
            emit(&mut out, &agent_message("item_2", "one"))?;
            thread::sleep(Duration::from_millis(300));
            emit(&mut out, &agent_message("item_3", "two"))?;
        }
        "huge" => {
            let text = "é✓😀 ".repeat(30_000);
            emit(&mut out, &agent_message("item_2", &text))?;
        }
        _ => {
            let answer = if native_search && behavior == "search-hostile" {
                // Cited links whose text tries to look like command-line
                // options, shell, or markup (SEC-05).
                format!(
                    "You asked: {prompt}\n\n[--config=evil $(touch pwned) <b>bold</b>](https://example.com/codex-hostile) [run](javascript:alert(1)) https://user@evil.example/ https://example.com/codex-hostile"
                )
            } else if native_search && behavior != "search-no-links" {
                format!(
                    "You asked: {prompt}\n\n[Codex search result](https://example.com/codex-search)"
                )
            } else {
                format!("You asked: {prompt}")
            };
            emit(&mut out, &agent_message("item_2", &answer))?;
        }
    }
    emit(
        &mut out,
        &json!({"type": "turn.completed", "usage": {"input_tokens": 12, "cached_input_tokens": 0, "cache_write_input_tokens": 0, "output_tokens": 7, "reasoning_output_tokens": 0}}),
    )?;
    drop(out);
    if let Some(flood) = stderr_flood {
        let _ = flood.join();
    }
    if behavior == "lingers" {
        hang();
    }
    Ok(ExitCode::SUCCESS)
}

/// Answers a few bytes at a time, so every line, and some characters, arrive
/// in pieces.
fn dribble(prompt: &str) -> io::Result<ExitCode> {
    let mut script = Vec::new();
    for event in [
        json!({"type": "thread.started", "thread_id": format!("thread-{}", std::process::id())}),
        json!({"type": "turn.started"}),
        agent_message("item_1", &format!("You asked: {prompt}")),
        json!({"type": "turn.completed", "usage": {}}),
    ] {
        emit(&mut script, &event)?;
    }
    let mut out = io::stdout().lock();
    for piece in script.chunks(3) {
        out.write_all(piece)?;
        out.flush()?;
        thread::sleep(Duration::from_millis(2));
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes `bytes` of stderr, or until `limit` passes, as fast as it can.
fn flood_stderr(bytes: usize, limit: Duration) {
    let give_up = Instant::now() + limit;
    let block = format!("fake codex: {SECRET} ").repeat(2048);
    let mut stderr = io::stderr().lock();
    let mut written = 0;
    while written < bytes && Instant::now() < give_up {
        if stderr.write_all(block.as_bytes()).is_err() {
            return;
        }
        written = written.saturating_add(block.len());
    }
}

fn agent_message(id: &str, text: &str) -> serde_json::Value {
    json!({"type": "item.completed", "item": {"id": id, "type": "agent_message", "text": text}})
}

/// Writes `event` as a line `count` times, or until [`ENDLESS`] passes, as
/// fast as it can: many lines to a write.
fn flood(out: &mut impl Write, event: &serde_json::Value, count: usize) -> io::Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let give_up = Instant::now() + ENDLESS;
    let mut written = 0;
    while written < count && Instant::now() < give_up {
        let lines = FLOOD_BATCH.min(count - written);
        out.write_all(&line.repeat(lines))?;
        written += lines;
    }
    out.flush()
}

/// A tool call's progress report.
fn progress(index: usize) -> serde_json::Value {
    json!({"type": "item.updated", "item": {"id": format!("item_{index}"), "type": "command_execution", "command": "ls", "aggregated_output": "", "exit_code": null, "status": "in_progress"}})
}

fn emit(out: &mut impl Write, event: &serde_json::Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn append(path: &PathBuf, text: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Waits without end, as far as any test can tell: after [`ENDLESS`] it
/// exits, so a test run that was killed leaves nothing running for long.
fn hang() -> ! {
    thread::sleep(ENDLESS);
    std::process::exit(0)
}

#[cfg(unix)]
fn ignore_termination_signal() {
    use nix::sys::signal::{SigSet, Signal};

    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    let _ = signals.thread_block();
}

#[cfg(not(unix))]
fn ignore_termination_signal() {}
