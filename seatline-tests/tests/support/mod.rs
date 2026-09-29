//! Helpers shared by the provider-level tests: the fake provider's fixtures,
//! and small functions that run an exchange to its end.

#![allow(dead_code, reason = "each test crate uses part of the support")]

pub mod live;

use std::time::{Duration, Instant};

use seatline_fake_provider::harness::Fixtures;
use seatline_providers::{Exchange, Update};

#[allow(unused_imports, reason = "each test crate uses part of the support")]
pub use seatline_fake_provider::harness::{
    CLAUDE_TEST_LIMITS, FakeClaude, FakeCodex, FakeGemini, FakeGrok, PROMPT_STOP_GRACE, TEST_LIMITS,
};

/// The fake provider binary this package builds, and where tests may put
/// scratch directories.
pub const FIXTURES: Fixtures = Fixtures::new(
    env!("CARGO_BIN_EXE_seatline-fake-provider"),
    env!("CARGO_TARGET_TMPDIR"),
    "seatline-tests",
);

/// How long a test waits for an exchange before it calls the exchange hung.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// Pulls updates until a terminal one.
pub fn run_to_end(exchange: &mut dyn Exchange) -> Vec<Update> {
    let deadline = Instant::now() + DEADLINE;
    let mut updates = Vec::new();
    loop {
        let update = exchange.next(deadline).expect("exchange timed out");
        let terminal = update.is_terminal();
        updates.push(update);
        if terminal {
            return updates;
        }
    }
}

/// Pulls updates until `Started`, which is the last one it returns.
pub fn run_until_started(exchange: &mut dyn Exchange) -> Vec<Update> {
    let deadline = Instant::now() + DEADLINE;
    let mut updates = Vec::new();
    loop {
        let update = exchange.next(deadline).expect("exchange timed out");
        let started = matches!(update, Update::Started);
        let terminal = update.is_terminal();
        updates.push(update);
        if started || terminal {
            return updates;
        }
    }
}

/// The updates that carry the answer, without the ones about the process.
pub fn visible(updates: &[Update]) -> Vec<Update> {
    updates
        .iter()
        .filter(|update| {
            !matches!(
                update,
                Update::Activity | Update::Launched | Update::Session(_) | Update::Usage(_)
            )
        })
        .cloned()
        .collect()
}

/// The code and reason a run failed with.
pub fn failure(updates: &[Update]) -> (seatline_core::protocol::ErrorCode, &'static str) {
    match updates.last() {
        Some(Update::Failed(error)) => (error.code, error.reason),
        other => panic!("expected failure, got {other:?}"),
    }
}

/// The text of the answer, in one string.
pub fn answer_text(updates: &[Update]) -> String {
    updates
        .iter()
        .filter_map(|update| match update {
            Update::Delta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The native session a run reported, if it did.
pub fn session_of(updates: &[Update]) -> Option<String> {
    updates.iter().find_map(|update| match update {
        Update::Session(session) => Some(session.clone()),
        _ => None,
    })
}
