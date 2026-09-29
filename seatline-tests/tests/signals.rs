//! POSIX signal semantics: `hang` dies on SIGTERM, while `ignore-cancel`
//! survives SIGTERM until it is escalated to SIGKILL.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const PROVIDER: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const READY_LINE: &str = "{\"type\":\"ready\"}\n";

/// Kills and reaps the child if a test panics while it is still running.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Guard {
    fn pid(&self) -> Pid {
        Pid::from_raw(i32::try_from(self.0.id()).expect("pid fits in pid_t"))
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.0.try_wait().expect("poll fake provider") {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

/// Starts `mode` and waits until it has announced readiness.
#[allow(
    clippy::disallowed_methods,
    reason = "this harness starts the fake provider binary to test signal handling"
)]
fn spawn_ready(mode: &str) -> Guard {
    let child = Command::new(PROVIDER)
        .args(["--mode", mode])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fake provider");
    let mut guard = Guard(child);

    let stdout: ChildStdout = guard.0.stdout.take().expect("stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read ready line");
    assert_eq!(line, READY_LINE);
    guard
}

#[test]
fn hang_terminates_on_sigterm() {
    let mut child = spawn_ready("hang");
    kill(child.pid(), Signal::SIGTERM).expect("send SIGTERM");

    let status = child
        .wait_for_exit(Duration::from_secs(1))
        .expect("hang mode should exit after SIGTERM");
    assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
}

#[test]
fn ignore_cancel_survives_sigterm_until_sigkill() {
    let mut child = spawn_ready("ignore-cancel");
    kill(child.pid(), Signal::SIGTERM).expect("send SIGTERM");

    thread::sleep(Duration::from_millis(300));
    assert!(
        child.0.try_wait().expect("poll fake provider").is_none(),
        "ignore-cancel mode exited on SIGTERM"
    );

    kill(child.pid(), Signal::SIGKILL).expect("send SIGKILL");
    let status = child
        .wait_for_exit(Duration::from_secs(1))
        .expect("ignore-cancel mode should exit after SIGKILL");
    assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
}
