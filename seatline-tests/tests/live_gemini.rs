//! The opt-in smoke test against the real Antigravity CLI (`agy`), through the
//! Gemini adapter and nothing of an application: status and sign-in, a plain
//! answer, a system prompt, a web search with its sources, and that nothing
//! holding the prompt outlives an ephemeral turn: neither the transcript under
//! `brain` nor the conversation database Antigravity keeps beside it.
//!
//! `SEATLINE_LIVE_GEMINI` turns it on: `1` runs it when `agy` is installed and
//! signed in and skips it, passing, when it isn't; `required` fails instead.
//! Unset, as in normal CI runs, it passes at once. What a model says is checked
//! for credentials before it is printed.
//!
//! ```bash
//! cd native
//! SEATLINE_LIVE_GEMINI=1 cargo test -p seatline-tests --test live_gemini -- --nocapture
//! ```

mod support;

use seatline_core::protocol::{Authentication, Availability, ErrorCode};
use seatline_core::turn::{Namespace, ToolPolicy};
use seatline_platform::discovery;
use seatline_platform::layout::Layout;
use seatline_providers::gemini::Gemini;
use seatline_providers::{Provider, Update};
use support::live::{
    ANSWER_TIMEOUT, Mode, Scan, answer_of, assert_completed, assert_follows_system_prompt,
    assert_no_marker, find_marker, home, marker, mode, run_within, scratch, skip_or_fail,
    status_of, turn,
};

const VARIABLE: &str = "SEATLINE_LIVE_GEMINI";

/// Environment variables that may hold a credential in a CI job.
const CREDENTIAL_VARIABLES: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

#[test]
fn live_gemini_answers_searches_and_leaves_nothing_behind() {
    let mode = mode(VARIABLE);
    if mode == Mode::Off {
        eprintln!("skipped: set {VARIABLE}=1 to ask the installed Antigravity CLI");
        return;
    }
    let namespace = Namespace::fixed("seatline-live").unwrap();
    let layout = Layout::new(namespace.clone());
    let work = scratch("gemini");
    let gemini = Gemini::new(
        &namespace,
        discovery::installed(&layout),
        work.join("workspace"),
    );

    // Discovery and sign-in.
    let status = status_of(&gemini);
    let Some(Update::Status { status: state, .. }) = status.first() else {
        panic!("a status update first: {status:?}");
    };
    eprintln!(
        "Antigravity is {:?}, {:?}",
        state.availability, state.authentication
    );
    if state.availability != Availability::Available {
        return skip_or_fail(
            VARIABLE,
            mode,
            &format!("Antigravity is {:?}", state.availability),
        );
    }
    if state.authentication == Authentication::Unauthenticated {
        return skip_or_fail(VARIABLE, mode, "Antigravity isn't signed in");
    }

    // A plain answer, with a reference no earlier run could have written, to
    // look for afterwards.
    let reference = marker();
    let question = format!("Reply with the single word: pong (reference {reference})");
    let plain = run_within(
        gemini
            .send(turn(None, &question, ToolPolicy::None))
            .as_mut(),
        ANSWER_TIMEOUT,
    );
    let last = plain.last().unwrap();
    if matches!(last, Update::Failed(error) if error.code == ErrorCode::ProviderNotAuthenticated) {
        return skip_or_fail(VARIABLE, mode, "Antigravity isn't signed in");
    }
    assert_completed("the plain turn", &plain);
    let plain_answer = answer_of("the plain answer", &plain, CREDENTIAL_VARIABLES);
    eprintln!("Gemini answered: {plain_answer}");
    assert!(
        plain_answer.to_lowercase().contains("pong"),
        "unexpected answer"
    );

    // A system prompt is followed.
    assert_follows_system_prompt("Gemini", &gemini, ToolPolicy::None, CREDENTIAL_VARIABLES);

    // A native search returns sources, and completes only with them.
    let searched = run_within(
        gemini
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
    eprintln!("Gemini searched: {searched_answer}");
    assert!(
        searched
            .iter()
            .any(|update| matches!(update, Update::Source(_))),
        "native search silently completed without sources"
    );

    // An ephemeral turn leaves nothing that holds its prompt, in the workspace
    // or in anything Antigravity keeps of its own: its transcripts, and the
    // conversation databases it keeps prompts in.
    assert_no_marker("the scratch directory", &work, &reference);
    if let Some(antigravity) = home().map(|home| home.join(".gemini/antigravity-cli")) {
        assert_no_marker("Antigravity's own files", &antigravity, &reference);
    }
}

#[test]
fn the_marker_search_finds_only_what_was_written_during_the_run() {
    let dir = scratch("gemini-marker-self-test");
    std::fs::create_dir(dir.join("nested")).unwrap();
    // Written before the run began, and dated so: never read.
    let earlier = dir.join("earlier");
    let reference = marker();
    std::fs::write(&earlier, format!("before {reference} after")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&earlier)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    let scan = find_marker(&dir, &reference);
    assert_eq!(
        scan,
        Scan {
            found: None,
            incomplete: false
        }
    );

    // Written during it, anywhere below: found.
    std::fs::write(dir.join("nested/file"), format!("before {reference} after")).unwrap();
    let scan = find_marker(&dir, &reference);
    assert_eq!(scan.found, Some(dir.join("nested/file")));
    assert!(!scan.incomplete);

    // Another run's marker is not in it.
    assert_eq!(find_marker(&dir, &marker()).found, None);
}
