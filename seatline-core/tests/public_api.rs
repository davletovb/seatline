//! Exercises only the standalone crate's public surface (no host or browser).

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Read;
use std::time::{Duration, Instant};

use seatline_core::discovery::SearchPath;
use seatline_core::exchange::{Exchange, Scripted, Update};
use seatline_core::process::{Process, ProcessSpec};
use seatline_core::protocol::{
    Authentication, Availability, Capabilities, Capability, ProviderState,
};
use seatline_core::stream::{LineSplitter, Output, StreamError, split_text};

#[test]
fn child_fixture() {
    match std::env::var("SEATLINE_CORE_CHILD").as_deref() {
        Ok("echo") => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            println!("echo:{input}");
            eprintln!("child-stderr");
        }
        Ok("hang") => std::thread::sleep(Duration::from_secs(30)),
        _ => {}
    }
}

#[test]
fn process_and_stream_lifecycle_work_without_host() {
    let self_exe = std::env::current_exe().unwrap();
    let spec = ProcessSpec::new(&self_exe)
        .args(["--exact", "child_fixture", "--nocapture"])
        .env("SEATLINE_CORE_CHILD", "echo");
    let mut process = Process::spawn(&spec).unwrap();
    process.write(b"ping").unwrap();
    process.close_stdin();
    let mut lines = seatline_core::stream::LineStream::new(process, 4096).keeping_stderr_tail(64);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_echo = false;
    loop {
        match lines.next(deadline) {
            Some(Output::Line(line)) => saw_echo |= line.contains("echo:ping"),
            Some(Output::Final(exit)) => {
                assert!(exit.status.unwrap().success());
                break;
            }
            other => panic!("unexpected stream state: {other:?}"),
        }
    }
    assert!(saw_echo);
    assert!(lines.stderr_tail().ends_with(b"child-stderr\n"));

    let mut hanging = Process::spawn(
        &ProcessSpec::new(self_exe)
            .args(["--exact", "child_fixture", "--nocapture"])
            .env("SEATLINE_CORE_CHILD", "hang"),
    )
    .unwrap();
    let exit = hanging.kill();
    assert!(!exit.status.unwrap().success());
}

#[test]
fn bounded_utf8_chunks_work_without_host() {
    let mut splitter = LineSplitter::new(4);
    let mut lines = VecDeque::new();
    splitter.push(&[0xc3], &mut lines).unwrap();
    splitter.push(&[0xa9, b'\r', b'\n'], &mut lines).unwrap();
    assert_eq!(lines.pop_front().as_deref(), Some("é"));
    assert_eq!(splitter.finish().unwrap(), None);
    let chunks: Vec<_> = split_text("ééé", 2).collect();
    assert_eq!(chunks.concat(), "ééé");
    assert!(chunks.iter().all(|chunk| chunk.len() <= 4));

    let mut too_long = LineSplitter::new(1);
    assert_eq!(
        too_long.push(b"ab\n", &mut lines),
        Err(StreamError::LineTooLong)
    );
}

#[test]
fn exchange_and_platform_types_do_not_depend_on_host() {
    let status = ProviderState {
        availability: Availability::Available,
        authentication: Authentication::Authenticated,
        capabilities: Capabilities {
            streaming: Capability::Supported,
            continuation: Capability::Unknown,
            web_search: Capability::Unsupported,
            model_selection: Capability::Unknown,
            cancellation: Capability::Supported,
            tool_isolation: Capability::Supported,
        },
        models: Cow::Borrowed(&[]),
        sign_in: None,
        readiness: None,
    };
    let mut exchange: Box<dyn Exchange> = Box::new(Scripted::new([
        Update::Status {
            provider_id: "example".into(),
            status,
        },
        Update::Completed,
    ]));
    assert!(matches!(
        exchange.next(Instant::now()),
        Some(Update::Status { .. })
    ));
    assert!(exchange.next(Instant::now()).unwrap().is_terminal());
    exchange.cancel(Duration::ZERO);
    assert!(exchange.next(Instant::now()).is_none());

    let mut pending = Scripted::new([Update::Started, Update::Completed]);
    pending.cancel(Duration::ZERO);
    assert_eq!(pending.next(Instant::now()), Some(Update::Stopped));
    assert_eq!(pending.next(Instant::now()), None);

    assert!(SearchPath::new(["relative".into()]).dirs().is_empty());
    assert!(Process::spawn(&ProcessSpec::new("relative-program")).is_err());
}
