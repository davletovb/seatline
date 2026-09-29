//! The native stream manager (NAT-05) on real processes: incremental line
//! buffering, UTF-8 characters split between chunks, the bounded memory policy
//! under large output and a slow reader, and prompt cancellation mid-chunk.

use std::time::{Duration, Instant};

use seatline_core::process::{Ending, Process, ProcessSpec};
use seatline_core::stream::{LineStream, Output, StreamError};

const PROVIDER: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const DEADLINE: Duration = Duration::from_secs(10);
const MIB: usize = 1024 * 1024;

/// The grace period of a cancel that should stop a process promptly. On POSIX
/// the stop request, SIGTERM, ends the process well inside a long grace period.
/// Windows has no stop request that reaches a process not reading its input,
/// so there a short grace period ends in a kill.
const PROMPT_STOP_GRACE: Duration = if cfg!(unix) {
    Duration::from_secs(5)
} else {
    Duration::from_millis(300)
};

fn stream(mode: &str, max_line_bytes: usize) -> LineStream {
    LineStream::new(spawn(mode), max_line_bytes)
}

fn spawn(mode: &str) -> Process {
    Process::spawn(&ProcessSpec::new(PROVIDER).args(["--mode", mode]))
        .expect("spawn the fake provider")
}

/// Pulls lines until a terminal state.
fn run_to_end(stream: &mut LineStream) -> (Vec<String>, Output) {
    let deadline = Instant::now() + DEADLINE;
    let mut lines = Vec::new();
    loop {
        match stream
            .next(deadline)
            .expect("the stream should end before the deadline")
        {
            Output::Line(line) => lines.push(line),
            terminal => return (lines, terminal),
        }
    }
}

#[test]
fn lines_arrive_in_order_then_a_final_state() {
    let (lines, terminal) = run_to_end(&mut stream("normal", 1024));
    assert_eq!(
        lines,
        [
            r#"{"type":"delta","text":"alpha"}"#,
            r#"{"type":"delta","text":" beta"}"#,
            r#"{"type":"completed"}"#,
        ]
    );
    let Output::Final(exit) = terminal else {
        panic!("expected a final state, got {terminal:?}");
    };
    assert!(exit.status.expect("exit status").success());
}

#[test]
fn characters_split_between_chunks_are_reassembled() {
    // Output arrives in chunks of at most 8 KiB, cut wherever a read ended; with
    // 3- and 4-byte characters throughout, many cuts fall inside one.
    let lines: Vec<String> = (0..2000)
        .map(|index| format!("{index} é✓😀 {}", "ü".repeat(index % 50)))
        .collect();
    let mut input = lines.join("\n");
    input.push('\n');

    let mut process = spawn("echo");
    process.write(input.as_bytes()).expect("queue input");
    process.close_stdin();
    let (received, terminal) = run_to_end(&mut LineStream::new(process, 1024));

    assert!(matches!(terminal, Output::Final(_)), "{terminal:?}");
    assert_eq!(received.len(), lines.len());
    assert!(received == lines, "the reassembled lines differ");
}

#[test]
fn stderr_is_counted_and_never_delivered() {
    let mut stream = stream("stderr", 1024);
    let (lines, terminal) = run_to_end(&mut stream);
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|line| !line.contains("stderr message")));
    assert!(matches!(terminal, Output::Final(_)));
    assert_eq!(
        stream.stderr_bytes(),
        "fake-provider: deterministic stderr message\n".len() as u64
    );
}

#[test]
fn a_line_past_the_limit_ends_the_stream_without_growing_memory() {
    // `large` writes 2 MiB with no line ending; the limit is 1 MiB.
    let mut stream = stream("large", MIB);
    let deadline = Instant::now() + DEADLINE;
    // The line never completes, so the first thing delivered is the error.
    let terminal = stream.next(deadline).expect("the stream should end");
    assert!(stream.buffered_bytes() <= MIB);
    assert_eq!(terminal, Output::Error(StreamError::LineTooLong));
    // The process was killed, and the error repeats.
    assert_eq!(stream.next(Instant::now()), Some(terminal));
}

#[test]
fn a_long_line_within_the_limit_arrives_whole_even_to_a_slow_reader() {
    // A reader that pauses between pulls: the provider waits on its own writes,
    // and the host never holds more than the line limit.
    let mut stream = stream("large", 4 * MIB);
    let deadline = Instant::now() + DEADLINE;
    let mut lines = Vec::new();
    let terminal = loop {
        std::thread::sleep(Duration::from_millis(1));
        assert!(stream.buffered_bytes() <= 4 * MIB);
        match stream.next(deadline).expect("the stream should end") {
            Output::Line(line) => lines.push(line),
            terminal => break terminal,
        }
    };
    assert!(matches!(terminal, Output::Final(_)), "{terminal:?}");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), 2 * MIB);
}

#[test]
fn cancelling_mid_chunk_drops_the_partial_line_and_stops_promptly() {
    // `partial` writes the start of a line and then waits: cancel while that
    // unfinished line is buffered.
    let mut stream = stream("partial", 4 * MIB);
    let deadline = Instant::now() + DEADLINE;
    while stream.buffered_bytes() == 0 {
        assert_eq!(
            stream.next(Instant::now() + Duration::from_millis(20)),
            None
        );
        assert!(Instant::now() < deadline, "no output arrived");
    }

    let started = Instant::now();
    stream.cancel(PROMPT_STOP_GRACE);
    assert_eq!(stream.buffered_bytes(), 0);
    // No line follows a cancel: the next thing delivered is the stop.
    let terminal = stream.next(deadline).expect("the stream should stop");
    assert!(matches!(terminal, Output::Stopped(_)), "{terminal:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn cancelling_between_lines_delivers_nothing_more() {
    // `slow` pauses 700 ms after its first line.
    let mut stream = stream("slow", 1024);
    let deadline = Instant::now() + DEADLINE;
    assert!(matches!(stream.next(deadline), Some(Output::Line(_))));

    let started = Instant::now();
    stream.cancel(PROMPT_STOP_GRACE);
    let terminal = stream.next(deadline).expect("the stream should stop");
    let Output::Stopped(exit) = terminal else {
        panic!("expected a stopped state, got {terminal:?}");
    };
    assert!(started.elapsed() < Duration::from_millis(600));
    #[cfg(unix)]
    assert_eq!(exit.ending, Ending::Stopped);
    #[cfg(not(unix))]
    assert_eq!(exit.ending, Ending::Killed);
}

#[test]
fn a_process_that_ignores_cancellation_is_killed_after_the_grace_period() {
    let mut stream = stream("ignore-cancel", 1024);
    let deadline = Instant::now() + DEADLINE;
    assert!(matches!(stream.next(deadline), Some(Output::Line(_))));

    let grace = Duration::from_millis(300);
    let started = Instant::now();
    stream.cancel(grace);
    let terminal = stream.next(deadline).expect("the stream should stop");
    let Output::Stopped(exit) = terminal else {
        panic!("expected a stopped state, got {terminal:?}");
    };
    assert_eq!(exit.ending, Ending::Killed);
    assert!(started.elapsed() >= grace);
}

#[test]
fn a_crash_ends_the_stream_in_a_final_state() {
    let (lines, terminal) = run_to_end(&mut stream("crash", 1024));
    assert!(lines.is_empty());
    let Output::Final(exit) = terminal else {
        panic!("expected a final state, got {terminal:?}");
    };
    assert!(!exit.status.expect("exit status").success());
}
