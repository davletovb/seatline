//! What phase timing costs inside the scheduler, with it off and with it on.
//!
//! The scheduler is where every update of every turn passes. With timing off a
//! turn carries `None` and the per-update cost is one branch on it; with it on
//! each update also reads the clock when it is a boundary. This measures both
//! on scripted turns, so that the figure is the scheduler's own and no process,
//! provider or socket is in it.

use std::io;
use std::time::{Duration, Instant};

use seatline_core::exchange::{Exchange, Update};
use seatline_core::telemetry::{Kind, Timeline};
use seatline_scheduler::{Event, Supervisor};
use serde_json::json;

use crate::stats::summarize;

/// A turn that reports `updates` updates and ends: a launch, a start, one
/// piece of text, progress for the rest, and its completion.
struct Burst {
    sent: usize,
    updates: usize,
}

impl Exchange for Burst {
    fn next(&mut self, _deadline: Instant) -> Option<Update> {
        let step = self.sent;
        self.sent += 1;
        Some(match step {
            0 => Update::Launched,
            1 => Update::Started,
            2 => Update::Delta("x".to_owned()),
            step if step + 1 >= self.updates => Update::Completed,
            _ => Update::Activity,
        })
    }

    fn cancel(&mut self, _grace: Duration) {}
}

/// Runs `turns` bursts of `updates` updates to the end and returns how many
/// events came out and how long it took.
pub fn run(turns: usize, updates: usize, timed: bool) -> (usize, Duration) {
    let mut supervisor = Supervisor::new();
    for _ in 0..turns {
        let exchange = Box::new(Burst { sent: 0, updates });
        if timed {
            let mut timeline = Timeline::new(Kind::Send, Instant::now());
            timeline.admitted(Instant::now());
            supervisor.start_timed(exchange, None, Duration::ZERO, timeline);
        } else {
            supervisor.start(exchange, None, Duration::ZERO);
        }
    }
    let begun = Instant::now();
    let mut events = 0;
    while !supervisor.is_empty() {
        events += supervisor
            .poll(Duration::from_millis(1))
            .iter()
            .filter(|event| matches!(event, Event::Update { .. } | Event::Ended { .. }))
            .count();
    }
    (events, begun.elapsed())
}

pub fn command(args: &[String]) -> io::Result<()> {
    let (mut turns, mut updates, mut rounds) = (200, 200, 15);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| io::Error::other(format!("{flag} needs a positive number")))?;
        match flag.as_str() {
            "--turns" => turns = value,
            "--updates" => updates = value.max(4),
            "--rounds" => rounds = value,
            other => return Err(io::Error::other(format!("unknown option {other}"))),
        }
    }
    // Alternating the two keeps a machine that speeds up or slows down during
    // the run from favoring one of them.
    let (mut off, mut on) = (Vec::new(), Vec::new());
    for round in 0..rounds + 1 {
        let (_, untimed) = run(turns, updates, false);
        let (_, timed) = run(turns, updates, true);
        if round > 0 {
            // The first round only warms the caches.
            off.push(untimed);
            on.push(timed);
        }
    }
    let per_update = |runs: &[Duration]| {
        let nanos: Vec<u64> = runs
            .iter()
            .map(|run| (run.as_nanos() / (turns * updates) as u128) as u64)
            .collect();
        summarize(&nanos).expect("at least one round")
    };
    let (off, on) = (per_update(&off), per_update(&on));
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "turns": turns,
            "updates_per_turn": updates,
            "rounds": rounds,
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "ns_per_update": {"timing_off": off, "timing_on": on},
            "timing_on_minus_off_p50_ns": on.p50 as i64 - off.p50 as i64,
        }))
        .map_err(io::Error::other)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_does_not_change_what_the_scheduler_reports() {
        let (without, _) = run(5, 12, false);
        let (with, _) = run(5, 12, true);
        // Every update but the terminal one, and one ending, per turn.
        assert_eq!(without, 5 * 12);
        assert_eq!(with, without);
    }
}
