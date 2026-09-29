//! Behavior of every fake-provider mode, including the non-terminating ones,
//! which are killed at a deadline like a supervisor would.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PROVIDER: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const NORMAL_OUTPUT: &str = concat!(
    "{\"type\":\"delta\",\"text\":\"alpha\"}\n",
    "{\"type\":\"delta\",\"text\":\" beta\"}\n",
    "{\"type\":\"completed\"}\n",
);
const FIRST_LINE: &str = "{\"type\":\"delta\",\"text\":\"alpha\"}\n";
const READY_LINE: &str = "{\"type\":\"ready\"}\n";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

struct Outcome {
    /// `None` when the process was still running at the deadline and was killed.
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: String,
}

/// Kills and reaps the child if a test panics while it is still running.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn drain<R: Read + Send + 'static>(mut stream: R) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).expect("read child output");
        bytes
    })
}

#[allow(
    clippy::disallowed_methods,
    reason = "this harness starts the fake provider binary, not a provider from browser input"
)]
fn run(args: &[&str], stdin: &[u8], timeout: Duration) -> Outcome {
    let child = Command::new(PROVIDER)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fake provider");
    let mut guard = Guard(child);
    let child = &mut guard.0;

    // Modes that never read stdin may exit before this write lands.
    let _ = child.stdin.take().expect("stdin").write_all(stdin);
    let stdout = drain(child.stdout.take().expect("stdout"));
    let stderr = drain(child.stderr.take().expect("stderr"));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll fake provider") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill fake provider");
            child.wait().expect("reap fake provider");
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };

    Outcome {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: String::from_utf8_lossy(&stderr.join().expect("stderr reader")).into_owned(),
    }
}

fn run_mode(mode: &str, stdin: &[u8], timeout: Duration) -> Outcome {
    run(&["--mode", mode], stdin, timeout)
}

fn exit_code(outcome: &Outcome) -> Option<i32> {
    outcome
        .status
        .expect("process should exit before the deadline")
        .code()
}

#[test]
fn normal_streams_three_lines() {
    let outcome = run_mode("normal", b"", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(0));
    assert_eq!(outcome.stdout, NORMAL_OUTPUT.as_bytes());
}

#[test]
fn slow_streams_the_same_lines() {
    let outcome = run_mode("slow", b"", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(0));
    assert_eq!(outcome.stdout, NORMAL_OUTPUT.as_bytes());
}

#[test]
fn slow_takes_longer_than_one_second() {
    // Two 700 ms pauses mean the stream cannot finish inside one second.
    let outcome = run_mode("slow", b"", Duration::from_secs(1));
    assert!(outcome.status.is_none(), "slow mode finished too early");
}

#[test]
fn slow_flushes_the_first_line_before_pausing() {
    // Killing at 0.5 s must leave exactly one complete line.
    let outcome = run_mode("slow", b"", Duration::from_millis(500));
    assert!(outcome.status.is_none());
    assert_eq!(outcome.stdout, FIRST_LINE.as_bytes());
}

#[test]
fn stderr_writes_a_diagnostic_and_streams_normally() {
    let outcome = run_mode("stderr", b"", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(0));
    assert_eq!(outcome.stdout, NORMAL_OUTPUT.as_bytes());
    assert!(outcome.stderr.contains("deterministic stderr message"));
}

#[test]
fn exit_nonzero_exits_42_without_stdout() {
    let outcome = run_mode("exit-nonzero", b"", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(42));
    assert!(outcome.stdout.is_empty());
    assert!(outcome.stderr.contains("exiting with status 42"));
}

#[test]
fn hang_emits_ready_and_never_exits() {
    let outcome = run_mode("hang", b"", Duration::from_millis(500));
    assert!(outcome.status.is_none(), "hang mode exited");
    assert_eq!(outcome.stdout, READY_LINE.as_bytes());
}

#[test]
fn ignore_cancel_ignores_a_cancel_command() {
    let outcome = run_mode("ignore-cancel", b"cancel\n", Duration::from_millis(500));
    assert!(outcome.status.is_none(), "ignore-cancel mode exited");
    assert_eq!(outcome.stdout, READY_LINE.as_bytes());
    assert!(outcome.stderr.contains("cancellation ignored"));
}

#[test]
fn malformed_emits_invalid_json() {
    let outcome = run_mode("malformed", b"", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(0));
    assert_eq!(outcome.stdout, b"{not-json\n");
}

#[test]
fn large_emits_exactly_two_mebibytes_without_newlines() {
    let outcome = run_mode("large", b"", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(0));
    assert_eq!(outcome.stdout.len(), 2 * 1024 * 1024);
    assert!(outcome.stdout.iter().all(|&byte| byte == b'x'));
}

#[test]
fn crash_ends_abnormally() {
    let outcome = run_mode("crash", b"", DEFAULT_TIMEOUT);
    let status = outcome
        .status
        .expect("process should exit before the deadline");
    assert!(!status.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGABRT as i32)
        );
    }
    assert!(outcome.stdout.is_empty());
}

#[test]
fn echo_copies_stdin_to_stdout() {
    let outcome = run_mode("echo", b"one\ntwo\n\x00\xff", DEFAULT_TIMEOUT);
    assert_eq!(exit_code(&outcome), Some(0));
    assert_eq!(outcome.stdout, b"one\ntwo\n\x00\xff");
}

#[test]
fn invalid_arguments_print_usage() {
    for args in [
        &[][..],
        &["--mode"],
        &["--mode", "bogus"],
        &["normal", "--mode"],
        &["--mode", "normal", "extra"],
    ] {
        let outcome = run(args, b"", DEFAULT_TIMEOUT);
        assert_eq!(exit_code(&outcome), Some(64), "{args:?}");
        assert!(outcome.stdout.is_empty(), "{args:?}");
        assert!(outcome.stderr.contains("usage:"), "{args:?}");
    }
}
