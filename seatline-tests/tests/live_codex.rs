//! The opt-in smoke test against the real Codex CLI (`codex`), through the Codex
//! adapter and nothing of an application: status and sign-in, a plain answer, a
//! system prompt, a web search with its sources, and that an ephemeral turn
//! leaves nothing that holds its prompt in Codex's own files.
//!
//! The adapter gives Codex its system prompt as the first part of the prompt it
//! reads on stdin (`seatline_core::prompt::SYSTEM_INTRO`), never on the command
//! line. This is the test of whether Codex follows it there.
//!
//! `SEATLINE_LIVE_CODEX` turns it on: `1` runs it when `codex` is installed and
//! signed in and skips it, passing, when it isn't; `required` fails instead.
//! Unset, as in normal CI runs, it passes at once. What a model says is checked
//! for credentials before it is printed. TabBeam's own live test of Codex
//! (`tabbeam-host`'s `live_codex`) covers the host on top of the same adapter.
//!
//! ```bash
//! cd native
//! SEATLINE_LIVE_CODEX=1 cargo test -p seatline-tests --test live_codex -- --nocapture
//! ```

mod support;

use seatline_core::protocol::{Authentication, Availability, ErrorCode};
use seatline_core::turn::{Namespace, ToolPolicy};
use seatline_platform::discovery;
use seatline_platform::layout::Layout;
use seatline_providers::codex::Codex;
use seatline_providers::{Provider, Update};
use support::live::{
    ANSWER_TIMEOUT, Mode, answer_of, assert_completed, assert_follows_system_prompt,
    assert_no_marker, marker, mode, provider_home, run_within, scratch, skip_or_fail, status_of,
    turn,
};

const VARIABLE: &str = "SEATLINE_LIVE_CODEX";

/// Environment variables that may hold a credential in a CI job.
const CREDENTIAL_VARIABLES: &[&str] = &["OPENAI_API_KEY", "CODEX_API_KEY"];

#[test]
fn live_codex_answers_searches_and_leaves_nothing_behind() {
    let mode = mode(VARIABLE);
    if mode == Mode::Off {
        eprintln!("skipped: set {VARIABLE}=1 to ask the installed Codex CLI");
        return;
    }
    let namespace = Namespace::fixed("seatline-live").unwrap();
    let layout = Layout::new(namespace);
    let work = scratch("codex");
    let codex = Codex::new(discovery::installed(&layout), work.join("workspace"));

    // Discovery and sign-in.
    let status = status_of(&codex);
    let Some(Update::Status { status: state, .. }) = status.first() else {
        panic!("a status update first: {status:?}");
    };
    eprintln!(
        "Codex is {:?}, {:?}",
        state.availability, state.authentication
    );
    if state.availability != Availability::Available {
        return skip_or_fail(
            VARIABLE,
            mode,
            &format!("Codex is {:?}", state.availability),
        );
    }
    if state.authentication == Authentication::Unauthenticated {
        return skip_or_fail(VARIABLE, mode, "Codex isn't signed in");
    }

    // A plain answer, with a reference no earlier run could have written, to
    // look for afterwards.
    let reference = marker();
    let question = format!("Reply with the single word: pong (reference {reference})");
    let plain = run_within(
        codex.send(turn(None, &question, ToolPolicy::None)).as_mut(),
        ANSWER_TIMEOUT,
    );
    match plain.last() {
        Some(Update::Failed(error)) if error.code == ErrorCode::ProviderNotAuthenticated => {
            return skip_or_fail(VARIABLE, mode, "Codex isn't signed in");
        }
        // The user's own Codex configuration decides this one, not the adapter.
        Some(Update::Failed(error)) if error.reason == "TOOL_ISOLATION_UNAVAILABLE" => {
            return skip_or_fail(
                VARIABLE,
                mode,
                "Codex's own configuration exposes tools that can't be switched off",
            );
        }
        _ => {}
    }
    assert_completed("the plain turn", &plain);
    let plain_answer = answer_of("the plain answer", &plain, CREDENTIAL_VARIABLES);
    eprintln!("Codex answered: {plain_answer}");
    assert!(
        plain_answer.to_lowercase().contains("pong"),
        "unexpected answer"
    );

    // A system prompt is followed.
    assert_follows_system_prompt("Codex", &codex, ToolPolicy::None, CREDENTIAL_VARIABLES);

    // A native search returns sources, and completes only with them.
    let searched = run_within(
        codex
            .send(turn(
                None,
                "Search the official Rust blog for a recent Rust release. Answer in one short sentence and include at least one full source URL as a Markdown link.",
                ToolPolicy::NativeWebSearch,
            ))
            .as_mut(),
        ANSWER_TIMEOUT,
    );
    assert_completed("the search turn", &searched);
    let searched_answer = answer_of("the search answer", &searched, CREDENTIAL_VARIABLES);
    eprintln!("Codex searched: {searched_answer}");
    assert!(
        searched
            .iter()
            .any(|update| matches!(update, Update::Source(_))),
        "native search silently completed without sources"
    );

    // An ephemeral turn leaves nothing that holds its prompt, in the workspace
    // or in anything Codex keeps of its own: its sessions and its logs.
    assert_no_marker("the scratch directory", &work, &reference);
    if let Some(codex_home) = provider_home("CODEX_HOME", ".codex") {
        assert_no_marker("Codex's own files", &codex_home, &reference);
    }
}
