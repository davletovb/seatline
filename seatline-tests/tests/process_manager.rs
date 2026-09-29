//! The provider process manager (NAT-04), driven with the fake provider:
//! success, failure, crash, timeout, cancellation, and kill escalation, with
//! nothing left running afterwards.

use std::io;
use std::time::{Duration, Instant};

use seatline_core::process::{Ending, Event, Exit, MAX_CHUNK_BYTES, Process, ProcessSpec};

const PROVIDER: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const NORMAL_OUTPUT: &str = concat!(
    "{\"type\":\"delta\",\"text\":\"alpha\"}\n",
    "{\"type\":\"delta\",\"text\":\" beta\"}\n",
    "{\"type\":\"completed\"}\n",
);
const READY_LINE: &str = "{\"type\":\"ready\"}\n";
/// Long enough for any mode that exits on its own, even on a slow CI runner.
const DEADLINE: Duration = Duration::from_secs(10);

fn spawn(mode: &str) -> Process {
    Process::spawn(&ProcessSpec::new(PROVIDER).args(["--mode", mode]))
        .expect("spawn the fake provider")
}

#[derive(Default)]
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    largest_chunk: usize,
}

/// Pulls events until the process exits.
fn run_to_exit(process: &mut Process) -> (Output, Exit) {
    let deadline = Instant::now() + DEADLINE;
    let mut output = Output::default();
    loop {
        match process
            .next_event(deadline)
            .expect("the process should exit before the deadline")
        {
            Event::Stdout(bytes) => {
                output.largest_chunk = output.largest_chunk.max(bytes.len());
                output.stdout.extend(bytes);
            }
            Event::Stderr(bytes) => {
                output.largest_chunk = output.largest_chunk.max(bytes.len());
                output.stderr.extend(bytes);
            }
            Event::Exited(exit) => return (output, exit),
        }
    }
}

/// Pulls stdout until it holds `lines` complete lines, which the running modes
/// write once they are up.
fn wait_for_lines(process: &mut Process, lines: usize) -> Vec<u8> {
    let deadline = Instant::now() + DEADLINE;
    let mut stdout = Vec::new();
    while stdout.iter().filter(|&&byte| byte == b'\n').count() < lines {
        match process
            .next_event(deadline)
            .expect("the process should start before the deadline")
        {
            Event::Stdout(bytes) => stdout.extend(bytes),
            Event::Stderr(_) => {}
            Event::Exited(exit) => panic!("the process exited early: {exit:?}"),
        }
    }
    stdout
}

#[test]
fn a_successful_run_delivers_its_output_then_its_exit() {
    let mut process = spawn("normal");
    let (output, exit) = run_to_exit(&mut process);

    assert_eq!(output.stdout, NORMAL_OUTPUT.as_bytes());
    assert!(output.stderr.is_empty());
    assert!(exit.status.expect("exit status").success());
    assert_eq!(exit.ending, Ending::Natural);
    assert!(exit.output_closed);

    // The exit is the last event, and stopping a finished process changes
    // nothing.
    assert_eq!(
        process.next_event(Instant::now()),
        Some(Event::Exited(exit))
    );
    assert_eq!(process.terminate(Duration::ZERO), exit);
    assert_eq!(process.kill(), exit);
}

#[test]
fn stderr_stays_separate_from_stdout() {
    let (output, exit) = run_to_exit(&mut spawn("stderr"));
    assert_eq!(output.stdout, NORMAL_OUTPUT.as_bytes());
    assert_eq!(
        output.stderr,
        b"fake-provider: deterministic stderr message\n"
    );
    assert!(exit.status.expect("exit status").success());
}

#[test]
fn a_failing_exit_is_reported_with_its_code() {
    let (output, exit) = run_to_exit(&mut spawn("exit-nonzero"));
    assert_eq!(exit.status.expect("exit status").code(), Some(42));
    assert_eq!(exit.ending, Ending::Natural);
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"fake-provider: exiting with status 42\n");
}

#[test]
fn a_crash_is_reported_as_an_abnormal_exit() {
    let (_, exit) = run_to_exit(&mut spawn("crash"));
    let status = exit.status.expect("exit status");
    assert!(!status.success());
    assert_eq!(exit.ending, Ending::Natural);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGABRT as i32)
        );
    }
}

#[test]
fn large_output_arrives_whole_in_bounded_chunks() {
    let (output, exit) = run_to_exit(&mut spawn("large"));
    assert!(exit.status.expect("exit status").success());
    assert_eq!(output.stdout.len(), 2 * 1024 * 1024);
    assert!(output.stdout.iter().all(|&byte| byte == b'x'));
    assert!(output.largest_chunk <= MAX_CHUNK_BYTES);
}

#[test]
fn input_is_written_while_output_is_read() {
    // `echo` writes back what it reads, so with more input than a pipe buffer
    // holds, writing from the caller's own thread would deadlock here.
    let input: Vec<u8> = (0..1024 * 1024 + 7)
        .map(|index| (index % 251) as u8)
        .collect();
    let mut process = spawn("echo");
    let (first, second) = input.split_at(input.len() / 2);
    process.write(first).expect("queue the first half");
    process.write(second).expect("queue the second half");
    process.close_stdin();

    let (output, exit) = run_to_exit(&mut process);
    assert!(exit.status.expect("exit status").success());
    assert_eq!(output.stdout.len(), input.len());
    assert!(output.stdout == input, "the echo differs from the input");
}

#[test]
fn writes_fail_once_stdin_is_closed() {
    let mut process = spawn("echo");
    process.write(b"ping\n").expect("queue input");
    process.close_stdin();
    let error = process.write(b"late").expect_err("stdin is closed");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    let (output, _) = run_to_exit(&mut process);
    assert_eq!(output.stdout, b"ping\n");
}

#[test]
fn input_reaches_a_running_process() {
    // `ignore-cancel` reads one command line and reports it on stderr.
    let mut process = spawn("ignore-cancel");
    wait_for_lines(&mut process, 1);
    process.write(b"cancel\n").expect("queue input");

    let deadline = Instant::now() + DEADLINE;
    let mut stderr = Vec::new();
    while !stderr.ends_with(b"cancellation ignored\n") {
        match process
            .next_event(deadline)
            .expect("the process should answer")
        {
            Event::Stderr(bytes) => stderr.extend(bytes),
            Event::Stdout(_) => {}
            Event::Exited(exit) => panic!("the process exited: {exit:?}"),
        }
    }
    assert_eq!(process.kill().ending, Ending::Killed);
}

#[test]
fn a_deadline_returns_control_without_an_event() {
    let mut process = spawn("hang");
    assert_eq!(wait_for_lines(&mut process, 1), READY_LINE.as_bytes());

    let started = Instant::now();
    assert_eq!(
        process.next_event(started + Duration::from_millis(200)),
        None
    );
    assert!(started.elapsed() >= Duration::from_millis(200));

    // A deadline that has already passed only polls.
    let started = Instant::now();
    assert_eq!(process.next_event(started), None);
    assert!(started.elapsed() < Duration::from_secs(1));

    assert_eq!(process.kill().ending, Ending::Killed);
}

#[test]
fn terminate_stops_a_process_that_honours_the_request() {
    let mut process = spawn("hang");
    wait_for_lines(&mut process, 1);
    let grace = Duration::from_secs(2);
    let started = Instant::now();
    let exit = process.terminate(grace);
    assert!(exit.output_closed);

    #[cfg(unix)]
    {
        // SIGTERM ends `hang` well inside the grace period.
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit.ending, Ending::Stopped);
        assert_eq!(
            exit.status.expect("exit status").signal(),
            Some(nix::sys::signal::Signal::SIGTERM as i32)
        );
        assert!(started.elapsed() < grace);
    }
    #[cfg(not(unix))]
    {
        // Closing stdin is the only request on Windows, and `hang` never
        // reads it.
        assert_eq!(exit.ending, Ending::Killed);
        assert!(started.elapsed() >= grace);
    }
}

#[test]
fn terminate_kills_a_process_that_ignores_the_request() {
    let mut process = spawn("ignore-cancel");
    wait_for_lines(&mut process, 1);
    let grace = Duration::from_millis(300);
    let started = Instant::now();
    let exit = process.terminate(grace);

    assert!(started.elapsed() >= grace);
    assert_eq!(exit.ending, Ending::Killed);
    assert!(exit.output_closed);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            exit.status.expect("exit status").signal(),
            Some(nix::sys::signal::Signal::SIGKILL as i32)
        );
    }
}

#[test]
fn kill_stops_a_process_without_asking() {
    let mut process = spawn("ignore-cancel");
    wait_for_lines(&mut process, 1);
    let started = Instant::now();
    let exit = process.kill();
    assert_eq!(exit.ending, Ending::Killed);
    assert!(exit.status.is_some());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_missing_executable_fails_to_spawn() {
    let missing = std::path::Path::new(PROVIDER).with_file_name("no-such-provider");
    let error = Process::spawn(&ProcessSpec::new(missing))
        .err()
        .expect("nothing to start");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_process_gets_only_the_environment_its_spec_sets() {
    // The test runs with cargo's variables and the whole environment of its
    // shell; the process sees none of them.
    let spec = ProcessSpec::new(PROVIDER)
        .args(["--mode", "env"])
        .env("RUNTIME_TEST_VARIABLE", "set")
        .envs([("SECOND", "2")]);
    let (output, exit) = run_to_exit(&mut Process::spawn(&spec).unwrap());
    assert!(exit.status.unwrap().success());
    let launch: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        launch["env"],
        serde_json::json!({"RUNTIME_TEST_VARIABLE": "set", "SECOND": "2"})
    );
}

#[test]
fn each_argument_reaches_the_process_whole() {
    // No shell parses the arguments: each arrives as its own element, as is.
    let args = [
        "a b",
        "$(id)",
        "`id`",
        "; rm -rf /",
        "&& echo pwned",
        "| cat",
        "*",
        "~",
        "%PATH%",
        "\"quoted\"",
        "it's",
        "",
        "line\nbreak",
        "back\\slash\\",
        "é✓😀",
        "--mode",
        "-",
    ];
    let spec = ProcessSpec::new(PROVIDER)
        .args(["--mode", "args"])
        .args(args);
    let (output, exit) = run_to_exit(&mut Process::spawn(&spec).unwrap());
    assert!(exit.status.unwrap().success());
    let received: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(received, args);
}

#[test]
fn a_process_runs_in_the_directory_its_spec_names() {
    let dir = std::env::temp_dir();
    let spec = ProcessSpec::new(PROVIDER)
        .args(["--mode", "env"])
        .current_dir(&dir);
    let (output, exit) = run_to_exit(&mut Process::spawn(&spec).unwrap());
    assert!(exit.status.unwrap().success());
    let launch: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        std::fs::canonicalize(launch["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&dir).unwrap()
    );

    // Without its directory, the process doesn't start at all.
    let missing = dir.join("no-such-directory");
    let error = Process::spawn(&ProcessSpec::new(PROVIDER).current_dir(missing))
        .err()
        .expect("no directory to run in");
    if cfg!(unix) {
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}

#[cfg(unix)]
mod posix {
    //! Process groups: the child leads its own, so stopping it reaches every
    //! process it started, and no process is left behind as an orphan or a
    //! zombie.
    //!
    //! A descendant's exit is observed through the stdout it shares with the
    //! child: a pipe reaches end of file only once every process holding it has
    //! exited, even where nothing reaps orphans.

    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::{Pid, getpgid};

    use super::*;

    fn pid(process: &Process) -> Pid {
        Pid::from_raw(i32::try_from(process.id()).expect("pid fits in pid_t"))
    }

    #[test]
    fn the_process_leads_its_own_process_group() {
        let mut process = spawn("hang");
        wait_for_lines(&mut process, 1);
        let pid = pid(&process);
        assert_eq!(getpgid(Some(pid)), Ok(pid));
        assert_ne!(getpgid(None), Ok(pid));
    }

    #[test]
    fn a_dropped_process_is_killed_and_reaped() {
        let mut process = spawn("hang");
        wait_for_lines(&mut process, 1);
        let pid = pid(&process);
        drop(process);
        // An unreaped process would still exist, as a zombie.
        assert_eq!(kill(pid, None), Err(Errno::ESRCH));
    }

    #[test]
    fn stopping_a_process_stops_its_descendants() {
        // `tree` and the descendant it starts each write a ready line.
        let mut process = spawn("tree");
        wait_for_lines(&mut process, 2);
        let exit = process.terminate(Duration::from_secs(5));
        assert_eq!(exit.ending, Ending::Stopped);
        assert!(exit.output_closed, "the descendant outlived the stop");
    }

    #[test]
    fn descendants_left_behind_are_stopped_when_the_process_exits() {
        // `orphan` exits at once, leaving a descendant that holds its stdout.
        let (_, exit) = run_to_exit(&mut spawn("orphan"));
        assert_eq!(exit.ending, Ending::Natural);
        assert!(exit.status.expect("exit status").success());
        assert!(exit.output_closed, "the descendant was left running");
    }

    #[test]
    fn a_descendant_that_leaves_the_group_cannot_hold_the_process_open() {
        // `escape` leaves a descendant in a session of its own, out of reach
        // of the group signal. The host stops waiting for its output after a
        // bounded drain and says so.
        let started = Instant::now();
        let (output, exit) = run_to_exit(&mut spawn("escape"));
        let elapsed = started.elapsed();

        let line = String::from_utf8(output.stdout).expect("UTF-8 output");
        let escaped: i32 = line
            .trim_end()
            .strip_prefix("{\"type\":\"detached\",\"pid\":")
            .and_then(|rest| rest.strip_suffix('}'))
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("no pid line in {line:?}"));
        kill(Pid::from_raw(escaped), Signal::SIGKILL).expect("stop the escaped process");

        assert_eq!(exit.ending, Ending::Natural);
        assert!(exit.status.expect("exit status").success());
        assert!(!exit.output_closed);
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    }
}
