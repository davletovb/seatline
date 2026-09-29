//! The opt-in smoke test against the real Claude Code CLI (`claude`), through the
//! Claude adapter and nothing of an application: status and sign-in, a plain
//! answer, a system prompt, a web search with its sources, and that an ephemeral
//! turn leaves nothing that holds its prompt in Claude's own files.
//!
//! The adapter gives Claude its system prompt as the first part of the prompt it
//! reads on stdin (`seatline_core::prompt::SYSTEM_INTRO`), never on the command
//! line. This is the test of whether Claude follows it there.
//!
//! `SEATLINE_LIVE_CLAUDE` turns it on: `1` runs it when `claude` is installed and
//! signed in and skips it, passing, when it isn't; `required` fails instead.
//! Unset, as in normal CI runs, it passes at once. What a model says is checked
//! for credentials before it is printed. TabBeam's own live test of Claude
//! (`tabbeam-host`'s `live_claude`) covers the host on top of the same adapter.
//!
//! ```bash
//! cd native
//! SEATLINE_LIVE_CLAUDE=1 cargo test -p seatline-tests --test live_claude -- --nocapture
//! ```

mod support;

use seatline_core::protocol::{Authentication, Availability, ErrorCode};
use seatline_core::turn::{Namespace, ToolPolicy};
use seatline_platform::discovery;
use seatline_platform::layout::Layout;
use seatline_providers::claude::Claude;
use seatline_providers::{Provider, Update};
use support::live::{
    ANSWER_TIMEOUT, Mode, answer_of, assert_completed, assert_follows_system_prompt,
    assert_no_marker, marker, mode, provider_home, run_within, scratch, skip_or_fail, status_of,
    turn,
};

const VARIABLE: &str = "SEATLINE_LIVE_CLAUDE";

/// Environment variables that may hold a credential in a CI job.
const CREDENTIAL_VARIABLES: &[&str] = &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"];

#[test]
fn live_claude_answers_searches_and_leaves_nothing_behind() {
    let mode = mode(VARIABLE);
    if mode == Mode::Off {
        eprintln!("skipped: set {VARIABLE}=1 to ask the installed Claude CLI");
        return;
    }
    let namespace = Namespace::fixed("seatline-live").unwrap();
    let layout = Layout::new(namespace);
    let work = scratch("claude");
    let claude = Claude::new(discovery::installed(&layout), work.join("workspace"));

    // Discovery and sign-in.
    let status = status_of(&claude);
    let Some(Update::Status { status: state, .. }) = status.first() else {
        panic!("a status update first: {status:?}");
    };
    eprintln!(
        "Claude is {:?}, {:?}",
        state.availability, state.authentication
    );
    if state.availability != Availability::Available {
        return skip_or_fail(
            VARIABLE,
            mode,
            &format!("Claude is {:?}", state.availability),
        );
    }
    if state.authentication == Authentication::Unauthenticated {
        return skip_or_fail(VARIABLE, mode, "Claude isn't signed in");
    }

    // A plain answer, with a reference no earlier run could have written, to
    // look for afterwards.
    let reference = marker();
    let question = format!("Reply with the single word: pong (reference {reference})");
    let plain = run_within(
        claude
            .send(turn(None, &question, ToolPolicy::None))
            .as_mut(),
        ANSWER_TIMEOUT,
    );
    if matches!(plain.last(), Some(Update::Failed(error)) if error.code == ErrorCode::ProviderNotAuthenticated)
    {
        return skip_or_fail(VARIABLE, mode, "Claude isn't signed in");
    }
    assert_completed("the plain turn", &plain);
    let plain_answer = answer_of("the plain answer", &plain, CREDENTIAL_VARIABLES);
    eprintln!("Claude answered: {plain_answer}");
    assert!(
        plain_answer.to_lowercase().contains("pong"),
        "unexpected answer"
    );

    // A system prompt is followed.
    assert_follows_system_prompt("Claude", &claude, ToolPolicy::None, CREDENTIAL_VARIABLES);

    // A native search returns sources, and completes only with them.
    let searched = run_within(
        claude
            .send(turn(
                None,
                "Search the web for the official Rust language website and answer in one short sentence with a source.",
                ToolPolicy::NativeWebSearch,
            ))
            .as_mut(),
        ANSWER_TIMEOUT,
    );
    assert_completed("the search turn", &searched);
    let searched_answer = answer_of("the search answer", &searched, CREDENTIAL_VARIABLES);
    eprintln!("Claude searched: {searched_answer}");
    assert!(
        searched
            .iter()
            .any(|update| matches!(update, Update::Source(_))),
        "native search silently completed without sources"
    );

    // An ephemeral turn leaves nothing that holds its prompt, in the workspace
    // or in anything Claude keeps of its own: its session transcripts.
    assert_no_marker("the scratch directory", &work, &reference);
    if let Some(claude_home) = provider_home("CLAUDE_CONFIG_DIR", ".claude") {
        assert_no_marker("Claude's own files", &claude_home, &reference);
    }
}
