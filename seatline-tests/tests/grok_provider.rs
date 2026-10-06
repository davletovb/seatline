//! The Grok adapter (Grok's one-shot headless mode) against a fake `grok`, at
//! the runtime's level: a `Turn` goes in and `Update`s come out. It covers
//! status and the cached sign-in, the answer, every boundary that fails closed,
//! and the per-turn workspaces. The adapter knows no conversations, so the
//! tests that pin how an application maps its own to Grok's stateless turns
//! belong to that application (TabBeam's are in `test_provider`).

mod support;

use std::time::{Duration, Instant};

use seatline_core::protocol::{Authentication, Availability, Capability, ErrorCode};
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::grok::CAPABILITIES;
use seatline_providers::{Provider, Update};
use support::{FIXTURES, FakeGrok, answer_text, failure, run_to_end};

/// An ephemeral turn of one plain question, to a model the fake serves.
fn ask(text: &str) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: Some("grok-4.6".to_owned()),
        reasoning_effort: None,
        service_tier: None,
        tools: ToolPolicy::None,
        session: SessionPolicy::Ephemeral,
        continuation: None,
        cleanup_group: None,
        check_sign_in: true,
    }
}

fn model(model: &str) -> Turn {
    Turn {
        model: Some(model.to_owned()),
        ..ask("Say hello")
    }
}

fn status_of(updates: &[Update]) -> (Availability, Authentication) {
    match &updates[0] {
        Update::Status {
            provider_id,
            status,
        } => {
            assert_eq!(provider_id, "grok");
            (status.availability, status.authentication)
        }
        other => panic!("unexpected update: {other:?}"),
    }
}

#[test]
fn status_uses_grok_models_and_cached_oauth() {
    let fake = FakeGrok::install(FIXTURES);
    let updates = run_to_end(fake.adapter().status().as_mut());
    assert_eq!(
        status_of(&updates),
        (Availability::Available, Authentication::Authenticated)
    );
    match &updates[0] {
        Update::Status { status, .. } => assert_eq!(status.capabilities, CAPABILITIES),
        other => panic!("unexpected update: {other:?}"),
    }
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn signed_out_status_is_reported_as_available_but_unauthenticated() {
    let fake = FakeGrok::install(FIXTURES);
    std::fs::remove_file(fake.home.join(".grok/auth.json")).unwrap();
    let updates = run_to_end(fake.adapter().status().as_mut());
    assert_eq!(
        status_of(&updates),
        (Availability::Available, Authentication::Unauthenticated)
    );
}

#[test]
fn relocated_grok_home_is_used_only_to_find_the_cached_auth_file() {
    let fake = FakeGrok::install(FIXTURES);
    std::fs::remove_file(fake.home.join(".grok/auth.json")).unwrap();
    let custom = fake.home.join("custom-grok");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(custom.join("auth.json"), "{}").unwrap();

    let updates = run_to_end(
        fake.adapter_with_environment(&[("GROK_HOME", custom)])
            .status()
            .as_mut(),
    );
    assert_eq!(status_of(&updates).1, Authentication::Authenticated);
}

#[test]
fn a_question_streams_its_answer() {
    let fake = FakeGrok::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(ask("Say hello")).as_mut());
    assert!(updates.contains(&Update::Started), "{updates:?}");
    assert!(updates.contains(&Update::Activity), "{updates:?}");
    assert_eq!(answer_text(&updates), "Grok answer");
    assert_eq!(updates.last(), Some(&Update::Completed));
    // A stateless mode keeps no native session for the caller to resume.
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, Update::Session(_))),
        "{updates:?}"
    );
}

#[test]
fn earlier_messages_are_replayed_in_the_prompt() {
    let fake = FakeGrok::install(FIXTURES);
    let turn = Turn {
        messages: vec![
            Message {
                role: Role::User,
                text: "Earlier question".to_owned(),
            },
            Message {
                role: Role::Assistant,
                text: "Earlier assistant answer".to_owned(),
            },
            Message {
                role: Role::User,
                text: "Say hello".to_owned(),
            },
        ],
        ..ask("unused")
    };
    let updates = run_to_end(fake.adapter().send(turn).as_mut());
    assert_eq!(answer_text(&updates), "Grok follow-up answer");
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn resolved_model_alias_is_accepted() {
    let fake = FakeGrok::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(model("grok-4")).as_mut());
    assert_eq!(answer_text(&updates), "Grok answer");
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn web_search_is_not_advertised_or_launched_on_shipped_grok() {
    let fake = FakeGrok::install(FIXTURES);
    assert_eq!(CAPABILITIES.web_search, Capability::Unsupported);
    let updates = run_to_end(
        fake.adapter()
            .send(Turn {
                tools: ToolPolicy::NativeWebSearch,
                ..ask("Find the example source")
            })
            .as_mut(),
    );
    assert_eq!(
        failure(&updates),
        (ErrorCode::InvalidRequest, "SEARCH_UNSUPPORTED")
    );
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert!(fake.turn_dirs().is_empty());
}

#[test]
fn every_init_boundary_fails_closed_with_a_specific_reason() {
    let fake = FakeGrok::install(FIXTURES);
    let adapter = fake.adapter();
    for (name, reason) in [
        ("grok-init-auth", "AUTH_REJECTED"),
        ("grok-init-model", "MODEL_MISMATCH"),
        ("grok-init-cwd", "WORKSPACE_MISMATCH"),
        ("grok-init-tools", "TOOLSET_MISMATCH"),
        ("grok-init-skills", "SKILLS_MISMATCH"),
        ("grok-init-mcp", "MCP_MISMATCH"),
    ] {
        let updates = run_to_end(adapter.send(model(name)).as_mut());
        assert_eq!(failure(&updates).1, reason, "{name}: {updates:?}");
    }
}

#[test]
fn unexpected_client_tool_activity_fails_closed() {
    let fake = FakeGrok::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(model("grok-tool-violation")).as_mut());
    assert_eq!(
        failure(&updates),
        (ErrorCode::ProviderFailed, "PROVIDER_BOUNDARY_VIOLATION")
    );
}

#[test]
fn failed_results_are_normalized() {
    let fake = FakeGrok::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(model("grok-result-auth")).as_mut());
    assert_eq!(failure(&updates).1, "AUTH_REJECTED");
}

#[test]
fn a_second_host_does_not_remove_a_live_turn_workspace() {
    let fake = FakeGrok::install(FIXTURES);
    let first_adapter = fake.adapter();
    let mut running = first_adapter.send(model("grok-hang"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(update) = running.next(deadline) {
        if matches!(update, Update::Started) {
            break;
        }
    }
    let live = fake.turn_dirs();
    assert_eq!(live.len(), 1, "expected one live turn workspace");

    let _second_host = fake.adapter();
    for path in live {
        assert!(path.exists(), "a second host removed {path:?}");
    }

    running.cancel(Duration::from_millis(100));
    let updates = run_to_end(running.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped));
}

#[test]
fn a_turn_is_owned_and_named_after_the_applications_namespace() {
    let fake = FakeGrok::install(FIXTURES);
    let adapter = fake.adapter();

    // While it runs, the turn's workspace holds the owner file of its
    // namespace, and that alone.
    let mut running = adapter.send(model("grok-hang"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(update) = running.next(deadline) {
        if matches!(update, Update::Started) {
            break;
        }
    }
    let live = fake.turn_dirs();
    assert_eq!(live.len(), 1);
    assert!(live[0].join(".seatline-tests-owner").exists());
    assert!(!live[0].join(".pervue-owner").exists());
    assert!(!live[0].join(".tabbeam-owner").exists());
    running.cancel(Duration::from_millis(100));
    run_to_end(running.as_mut());

    // The agent it runs as is named the same way.
    let agents = fake.read("grok-agents");
    assert!(agents.contains("name: seatline-tests-text\n"), "{agents}");
    assert!(!agents.contains("pervue"), "{agents}");
    assert!(!agents.contains("tabbeam"), "{agents}");
}

#[test]
fn a_system_prompt_goes_ahead_of_the_question_and_never_onto_the_command_line() {
    let fake = FakeGrok::install(FIXTURES);
    let updates = run_to_end(
        fake.adapter()
            .send(Turn {
                system: Some("Answer in French. SYSTEM-MARKER".to_owned()),
                ..ask("What is muse?")
            })
            .as_mut(),
    );
    assert_eq!(updates.last(), Some(&Update::Completed));
    // Grok follows instructions in the prompt file when they claim precedence
    // over the messages, and ignores them in the body of its agent file, so the
    // instructions come first in the prompt, in its own words.
    assert_eq!(
        fake.prompts(),
        [format!(
            "{}Answer in French. SYSTEM-MARKER\n\nWhat is muse?",
            seatline_providers::grok::SYSTEM_INTRO
        )]
    );
    assert!(
        !fake.read("grok-agents").contains("SYSTEM-MARKER"),
        "the system prompt is in the agent, where Grok ignores it"
    );
    assert!(
        !fake.invocations().concat().contains("SYSTEM-MARKER"),
        "the system prompt reached the command line"
    );
}

#[test]
fn turns_the_adapter_cannot_serve_are_refused_before_grok_runs() {
    let fake = FakeGrok::install(FIXTURES);
    let adapter = fake.adapter();

    for (turn, refusal) in [
        (
            model("claude-opus"),
            (ErrorCode::InvalidRequest, "MODEL_NOT_SUPPORTED"),
        ),
        (
            // Grok keeps no session: only the caller's messages continue.
            Turn {
                session: SessionPolicy::Persistent,
                ..ask("hi")
            },
            (ErrorCode::InvalidRequest, "PERSISTENT_SESSION_UNSUPPORTED"),
        ),
        (
            // Anything that could become an option of its own never reaches argv.
            model("--latest"),
            (ErrorCode::InvalidRequest, "INVALID_TURN"),
        ),
        (
            Turn {
                messages: Vec::new(),
                ..ask("hi")
            },
            (ErrorCode::InvalidRequest, "INVALID_TURN"),
        ),
        (
            Turn {
                cleanup_group: Some("../escape".to_owned()),
                ..ask("hi")
            },
            (ErrorCode::InvalidRequest, "INVALID_TURN"),
        ),
    ] {
        let updates = run_to_end(adapter.send(turn.clone()).as_mut());
        assert_eq!(updates.len(), 1, "{turn:?}: {updates:?}");
        assert_eq!(failure(&updates), refusal, "{turn:?}");
    }
    assert!(fake.turn_dirs().is_empty());
}
