//! The opt-in smoke test against the real Grok Build CLI (`grok`), through the
//! Grok adapter and nothing of an application: status and the cached sign-in, a
//! plain answer, a system prompt, that web search is refused while the shipped
//! CLI can't offer it safely, and that a turn's private workspace, prompt file
//! and Grok home don't outlive it.
//!
//! `SEATLINE_LIVE_GROK` turns it on: `1` runs it when `grok` is installed and
//! signed in and skips it, passing, when it isn't; `required` fails instead.
//! Unset, as in normal CI runs, it passes at once. What a model says is checked
//! for credentials before it is printed. `required` needs Grok's cached sign-in
//! to be fresh: an expired token reads as signed out, and running `grok models`
//! once refreshes it.
//!
//! ```bash
//! cd native
//! SEATLINE_LIVE_GROK=1 cargo test -p seatline-tests --test live_grok -- --nocapture
//! ```

mod support;

use std::time::{Duration, Instant};

use seatline_core::protocol::{Authentication, Availability, ErrorCode};
use seatline_core::turn::{Namespace, ToolPolicy};
use seatline_platform::discovery;
use seatline_platform::layout::Layout;
use seatline_providers::grok::Grok;
use seatline_providers::{Provider, Update};
use support::live::{
    ANSWER_TIMEOUT, Mode, answer_of, assert_completed, assert_follows_system_prompt,
    assert_no_marker, home, marker, mode, run_within, scratch, skip_or_fail, status_of, turn,
};

const VARIABLE: &str = "SEATLINE_LIVE_GROK";

/// Environment variables that may hold a credential in a CI job.
const CREDENTIAL_VARIABLES: &[&str] = &["XAI_API_KEY", "GROK_CODE_XAI_API_KEY"];

/// Waits a moment for the background removal of a turn's directories, and
/// returns the ones still there.
fn left_in(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        let left: Vec<_> = std::fs::read_dir(base)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("turn-") || name.starts_with("status-"))
            })
            .collect();
        if left.is_empty() || Instant::now() >= give_up {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn live_grok_answers_and_leaves_nothing_behind() {
    let mode = mode(VARIABLE);
    if mode == Mode::Off {
        eprintln!("skipped: set {VARIABLE}=1 to ask the installed Grok CLI");
        return;
    }
    let namespace = Namespace::fixed("seatline-live").unwrap();
    let layout = Layout::new(namespace.clone());
    let work = scratch("grok");
    let workspace = work.join("workspace");
    let grok = Grok::new(&namespace, discovery::installed(&layout), workspace.clone());

    // Discovery and the cached sign-in.
    let status = status_of(&grok);
    let Some(Update::Status { status: state, .. }) = status.first() else {
        panic!("a status update first: {status:?}");
    };
    eprintln!(
        "Grok is {:?}, {:?}",
        state.availability, state.authentication
    );
    if state.availability != Availability::Available {
        return skip_or_fail(VARIABLE, mode, &format!("Grok is {:?}", state.availability));
    }
    if state.authentication != Authentication::Authenticated {
        return skip_or_fail(
            VARIABLE,
            mode,
            "Grok isn't signed in, or its cached sign-in expired (`grok models` refreshes it)",
        );
    }

    // A plain answer, with a reference no earlier run could have written, to
    // look for afterwards.
    let reference = marker();
    let question = format!("Reply with the single word: pong (reference {reference})");
    let plain = run_within(
        grok.send(turn(None, &question, ToolPolicy::None)).as_mut(),
        ANSWER_TIMEOUT,
    );
    let last = plain.last().unwrap();
    if matches!(last, Update::Failed(error) if error.code == ErrorCode::ProviderNotAuthenticated) {
        return skip_or_fail(VARIABLE, mode, "Grok isn't signed in");
    }
    assert_completed("the plain turn", &plain);
    let plain_answer = answer_of("the plain answer", &plain, CREDENTIAL_VARIABLES);
    eprintln!("Grok answered: {plain_answer}");
    assert!(
        plain_answer.to_lowercase().contains("pong"),
        "unexpected answer"
    );

    // A system prompt is followed.
    assert_follows_system_prompt("Grok", &grok, ToolPolicy::None, CREDENTIAL_VARIABLES);

    // Search is refused before anything runs, until the shipped CLI can offer
    // it without weakening the text-only boundary.
    let searched = run_within(
        grok.send(turn(
            None,
            "Search the web for Rust.",
            ToolPolicy::NativeWebSearch,
        ))
        .as_mut(),
        ANSWER_TIMEOUT,
    );
    assert!(
        matches!(searched.as_slice(), [Update::Failed(error)] if error.reason == "SEARCH_UNSUPPORTED"),
        "{searched:?}"
    );

    // Nothing that holds the prompt is left: the turns' workspaces (prompt
    // file, private Grok home) are removed, and Grok wrote nothing of its own
    // where it keeps its files.
    assert!(left_in(&workspace).is_empty(), "a turn workspace was left");
    assert_no_marker("the scratch directory", &work, &reference);
    if let Some(grok_home) = home().map(|home| home.join(".grok")) {
        assert_no_marker("Grok's own files", &grok_home, &reference);
    }
}
