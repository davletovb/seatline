//! Ending a finished Claude turn without waiting for its process (I-05).
//!
//! After its final `result` Claude is not done: it uploads its own usage analytics and
//! removes its own session bookkeeping, and only then exits, about half a
//! second later. A turn that succeeded and keeps no session has nothing left to
//! wait for, so it completes at the result and the process is left to exit on
//! its own, in the background, under a bound. A signal does not shorten that
//! wrap-up and a kill skips it, so neither is used to save the time. Against the
//! fake Claude, whose `exits-after-<ms>` takes that long to leave and records
//! its exit only if it is allowed to get that far.
//!
//! Timings are compared with each other, never with a clock: the fake takes two
//! seconds to leave, and a turn is judged by whether it was held for them.

mod support;

use std::time::{Duration, Instant};

use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn, Usage};
use seatline_providers::claude::Limits;
use seatline_providers::{Exchange, Provider, Update};
use support::{CLAUDE_TEST_LIMITS, FIXTURES, FakeClaude, failure, run_to_end};

/// How long the fake takes to leave after its result when a test needs a gap
/// wide enough to tell "waited for it" from "did not".
const LEAVES_AFTER_MS: u64 = 2000;

/// Long enough that the adapter's own grace is not what ends these processes.
fn patient() -> Limits {
    Limits {
        finish: Duration::from_secs(20),
        ..CLAUDE_TEST_LIMITS
    }
}

fn turn(text: &str, session: SessionPolicy) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: None,
        reasoning_effort: None,
        service_tier: None,
        tools: ToolPolicy::None,
        session,
        continuation: None,
        cleanup_group: None,
        check_sign_in: false,
    }
}

/// What a run showed: its updates, when its terminal update was returned, and
/// when the adapter says Claude's final result was read.
struct Run {
    updates: Vec<Update>,
    ended: Instant,
    result_at: Option<Instant>,
}

impl Run {
    /// How long the turn was held after Claude's final result.
    fn tail(&self) -> Duration {
        self.ended
            .saturating_duration_since(self.result_at.expect("the adapter saw a result"))
    }
}

fn run(exchange: &mut dyn Exchange) -> Run {
    let updates = run_to_end(exchange);
    Run {
        updates,
        ended: Instant::now(),
        result_at: exchange.result_at(),
    }
}

/// How many launches have finished leaving on their own.
fn exits(claude: &FakeClaude) -> usize {
    claude.read("claude-exits").lines().count()
}

/// Waits until `count` launches have recorded that they left on their own.
fn wait_until_exits(claude: &FakeClaude, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while exits(claude) < count {
        assert!(
            Instant::now() < deadline,
            "{} of {count} launches left on their own",
            exits(claude)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until every launch has been reaped, which is when the reaper is done.
/// Only where a process can be asked whether it still exists.
fn wait_until_none_left(claude: &FakeClaude) {
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !claude.still_running().is_empty() {
            assert!(
                Instant::now() < deadline,
                "still around: {:?}",
                claude.still_running()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(unix))]
    let _ = claude;
}

fn usage(updates: &[Update]) -> Option<Usage> {
    updates.iter().find_map(|update| match update {
        Update::Usage(usage) => Some(*usage),
        _ => None,
    })
}

#[test]
fn a_successful_turn_that_keeps_no_session_ends_at_its_result_and_the_process_leaves_on_its_own() {
    let claude = FakeClaude::install(
        FIXTURES,
        &format!("exits-after-{LEAVES_AFTER_MS}"),
        "signed-in",
    );
    let adapter = claude.adapter_with(patient());
    let mut exchange = adapter.send(turn("hello", SessionPolicy::Ephemeral));
    let run = run(exchange.as_mut());

    // The whole answer, its usage and exactly one end, whole.
    assert_eq!(
        run.updates.last(),
        Some(&Update::Completed),
        "{:?}",
        run.updates
    );
    assert_eq!(run.updates.iter().filter(|u| u.is_terminal()).count(), 1);
    let answer: String = run
        .updates
        .iter()
        .filter_map(|update| match update {
            Update::Delta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answer, "You asked: hello");
    let usage = usage(&run.updates).expect("usage is kept");
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_write_input_tokens,
            usage.output_tokens
        ),
        (Some(18), Some(5), Some(2), Some(3))
    );

    // It was not held for the process: the process is still there, and has not
    // got as far as recording its exit; the adapter was not held either.
    #[cfg(unix)]
    assert!(
        !claude.still_running().is_empty(),
        "the process was waited for"
    );
    assert_eq!(exits(&claude), 0, "the process had already left");
    assert!(
        run.tail() < Duration::from_millis(LEAVES_AFTER_MS / 2),
        "held {:?} after the result",
        run.tail()
    );

    // And it was left alone to finish: it records its exit, which a signal or a
    // kill would have prevented, and then it is reaped.
    wait_until_exits(&claude, 1);
    wait_until_none_left(&claude);
    assert_eq!(exits(&claude), 1, "it was stopped before it could leave");
}

#[test]
fn a_turn_that_keeps_a_session_waits_for_the_process_to_leave() {
    let claude = FakeClaude::install(FIXTURES, "exits-after-600", "signed-in");
    let adapter = claude.adapter_with(patient());
    let mut exchange = adapter.send(turn("hello", SessionPolicy::Persistent));
    let run = run(exchange.as_mut());

    assert_eq!(
        run.updates.last(),
        Some(&Update::Completed),
        "{:?}",
        run.updates
    );
    // Claude was still writing the session the next turn resumes: it had left,
    // and been reaped, before the turn ended.
    assert_eq!(exits(&claude), 1);
    #[cfg(unix)]
    assert!(claude.still_running().is_empty());
    assert!(
        run.tail() >= Duration::from_millis(500),
        "not held for the process: {:?}",
        run.tail()
    );
}

#[test]
fn a_failed_turn_waits_for_the_process_to_leave() {
    let claude = FakeClaude::install(FIXTURES, "fails-after-600", "signed-in");
    let adapter = claude.adapter_with(patient());
    let mut exchange = adapter.send(turn("hello", SessionPolicy::Ephemeral));
    let run = run(exchange.as_mut());

    assert_eq!(
        failure(&run.updates).1,
        "PROVIDER_RATE_LIMITED",
        "{:?}",
        run.updates
    );
    assert_eq!(exits(&claude), 1);
    #[cfg(unix)]
    assert!(claude.still_running().is_empty());
    assert!(
        run.tail() >= Duration::from_millis(500),
        "not held for the process: {:?}",
        run.tail()
    );
}

#[cfg(unix)]
#[test]
fn a_process_that_never_leaves_is_stopped_in_the_background_after_its_grace() {
    let claude = FakeClaude::install(FIXTURES, "lingers", "signed-in");
    let adapter = claude.adapter_with(Limits {
        finish: Duration::from_secs(2),
        ..CLAUDE_TEST_LIMITS
    });
    let mut exchange = adapter.send(turn("hello", SessionPolicy::Ephemeral));
    let started = Instant::now();
    let run = run(exchange.as_mut());

    assert_eq!(
        run.updates.last(),
        Some(&Update::Completed),
        "{:?}",
        run.updates
    );
    // The turn did not sit through the grace; the process is still running.
    assert!(run.tail() < Duration::from_secs(1), "{:?}", run.tail());
    assert!(!claude.still_running().is_empty());

    // After the grace, and the stop that follows it, nothing is left. It was
    // stopped, so it never recorded a natural exit.
    wait_until_none_left(&claude);
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "stopped too early"
    );
    assert_eq!(exits(&claude), 0);
}

#[test]
fn only_so_many_processes_are_left_to_leave_and_a_turn_beyond_that_waits_for_its_own() {
    let claude = FakeClaude::install(FIXTURES, "exits-after-1500", "signed-in");
    let adapter = claude.adapter_with(patient()).with_background_exits(1);

    let mut first = adapter.send(turn("one", SessionPolicy::Ephemeral));
    let first = run(first.as_mut());
    let mut second = adapter.send(turn("two", SessionPolicy::Ephemeral));
    let second = run(second.as_mut());

    // The first took the one place and did not wait. The second found none, and
    // waited for its own process as turns did before processes were left.
    assert!(
        first.tail() < Duration::from_millis(750),
        "{:?}",
        first.tail()
    );
    assert!(
        second.tail() >= Duration::from_millis(1000),
        "{:?}",
        second.tail()
    );
    assert_eq!(second.updates.last(), Some(&Update::Completed));
    wait_until_exits(&claude, 2);
    wait_until_none_left(&claude);
    assert_eq!(exits(&claude), 2);

    // With no places at all, every turn waits.
    let none = claude.adapter_with(patient()).with_background_exits(0);
    let mut waiting = none.send(turn("three", SessionPolicy::Ephemeral));
    assert!(run(waiting.as_mut()).tail() >= Duration::from_millis(1000));
}

#[test]
fn cancelling_a_turn_that_has_ended_stops_it_and_leaves_the_process_alone() {
    let claude = FakeClaude::install(
        FIXTURES,
        &format!("exits-after-{LEAVES_AFTER_MS}"),
        "signed-in",
    );
    let adapter = claude.adapter_with(patient());
    let mut exchange = adapter.send(turn("hello", SessionPolicy::Ephemeral));

    // Up to the usage that comes just before the end, which is queued and not
    // yet returned.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let update = exchange.next(deadline).expect("exchange timed out");
        assert!(!update.is_terminal(), "ended before it could be cancelled");
        if matches!(update, Update::Usage(_)) {
            break;
        }
    }
    exchange.cancel(Duration::from_millis(300));
    let rest = run_to_end(exchange.as_mut());
    assert_eq!(rest.last(), Some(&Update::Stopped), "{rest:?}");

    // The answer was complete, so what Claude was still doing is not cut short.
    wait_until_exits(&claude, 1);
    wait_until_none_left(&claude);
    assert_eq!(exits(&claude), 1);
}
