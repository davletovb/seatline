//! The Gemini adapter (Antigravity's one-shot mode) against a fake `agy`, at
//! the runtime's level: a `Turn` goes in and `Update`s come out. It covers
//! status, the answer and the sources, what fails closed, and that no
//! Antigravity transcript outlives a turn. The adapter knows no conversations,
//! so the tests that pin how an application maps its own to Gemini's stateless
//! turns belong to that application (TabBeam's are in `test_provider`).

mod support;

use std::time::{Duration, Instant};

use seatline_core::protocol::{Authentication, Availability, ErrorCode};
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::gemini::{CAPABILITIES, Gemini};
use seatline_providers::{Provider, Update};
use support::{FIXTURES, FakeGemini, answer_text, failure, run_to_end};

/// An ephemeral turn of one plain question, to a model the fake serves.
fn ask(text: &str) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: Some("gemini-test".to_owned()),
        reasoning_effort: None,
        tools: ToolPolicy::None,
        session: SessionPolicy::Ephemeral,
        continuation: None,
        cleanup_group: None,
        check_sign_in: true,
    }
}

fn search() -> Turn {
    Turn {
        tools: ToolPolicy::NativeWebSearch,
        ..ask("Find the example source")
    }
}

fn model(model: &str) -> Turn {
    Turn {
        model: Some(model.to_owned()),
        ..ask("Say hello")
    }
}

/// How many transcripts and conversation databases the fake has kept.
fn transcripts(fake: &FakeGemini) -> usize {
    fake.kept()
}

#[test]
fn status_uses_agy_models_to_confirm_authentication() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().status().as_mut());
    match &updates[0] {
        Update::Status {
            provider_id,
            status,
        } => {
            assert_eq!(provider_id, "gemini");
            assert_eq!(status.availability, Availability::Available);
            assert_eq!(status.authentication, Authentication::Authenticated);
            assert_eq!(status.capabilities, CAPABILITIES);
        }
        other => panic!("unexpected update: {other:?}"),
    }
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn a_question_streams_its_answer() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(ask("Say hello")).as_mut());
    assert!(updates.contains(&Update::Started), "{updates:?}");
    assert_eq!(answer_text(&updates), "Gemini answer");
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
    let fake = FakeGemini::install(FIXTURES);
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
    assert_eq!(answer_text(&updates), "Gemini continued answer");
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn native_search_requires_the_actual_search_tool_and_emits_sources() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(search()).as_mut());
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, Update::Delta(text) if text.contains("I will search")))
    );
    assert!(updates.iter().any(|update| matches!(
        update,
        Update::Source(source)
            if source.backend_id == "gemini"
                && source.url == "https://example.com/agy-search"
    )));
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn every_finished_turn_removes_antigravitys_persisted_transcript() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(ask("Say hello")).as_mut());
    assert_eq!(updates.last(), Some(&Update::Completed));
    assert_eq!(
        transcripts(&fake),
        0,
        "the agy transcript survived the turn"
    );
}

#[test]
fn unexpected_antigravity_tools_fail_closed() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(model("gemini-tool-violation")).as_mut());
    assert_eq!(
        failure(&updates),
        (ErrorCode::ProviderFailed, "PROVIDER_BOUNDARY_VIOLATION")
    );
    assert_eq!(transcripts(&fake), 0);
}

#[test]
fn unknown_antigravity_step_types_fail_closed() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(model("gemini-unknown-step")).as_mut());
    assert_eq!(
        failure(&updates),
        (ErrorCode::ProviderFailed, "PROVIDER_BOUNDARY_VIOLATION")
    );
}

#[test]
fn unsafe_init_still_cleans_the_transcript_it_already_created() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(model("gemini-bad-init")).as_mut());
    assert_eq!(
        failure(&updates),
        (ErrorCode::ProviderFailed, "PROVIDER_AGENT_NOT_USED")
    );
    assert_eq!(
        transcripts(&fake),
        0,
        "unsafe init leaked its Antigravity transcript"
    );
}

#[test]
fn cancellation_before_init_scans_the_unique_workspace_and_cleans_transcript() {
    let fake = FakeGemini::install(FIXTURES);
    let mut exchange = fake.adapter().send(model("gemini-slow-init"));

    let transcript_ready = || {
        std::fs::read_dir(fake.brain()).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .path()
                    .join(".system_generated/logs/transcript.jsonl")
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() > 0)
            })
        })
    };
    let give_up = Instant::now() + Duration::from_secs(2);
    while Instant::now() < give_up && !transcript_ready() {
        assert!(matches!(
            exchange.next(Instant::now()),
            None | Some(Update::Launched)
        ));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        transcript_ready(),
        "fake never finished writing its pre-init transcript"
    );

    exchange.cancel(Duration::from_millis(10));
    let updates = run_to_end(exchange.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped));
    assert_eq!(
        transcripts(&fake),
        0,
        "pre-init cancellation leaked its transcript"
    );
}

/// Real `agy` echoes the prompt as a `user_input` step and can add
/// `system_message` steps; neither is answer text or an action, and the
/// answer's DONE update names only its step.
#[test]
fn the_echoed_prompt_and_system_messages_are_not_answer_or_violations() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(fake.adapter().send(ask("Say hello")).as_mut());
    assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    let answer = answer_text(&updates);
    assert_eq!(answer, "Gemini answer");
    assert!(!answer.contains("Session ready"));
}

#[test]
fn a_done_update_that_repeats_the_answer_does_not_double_it() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(
        fake.adapter()
            .send(model("gemini-cumulative-done"))
            .as_mut(),
    );
    assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
    assert_eq!(answer_text(&updates), "Gemini answer");
}

#[test]
fn narration_before_each_search_is_not_saved_into_the_answer() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(
        fake.adapter()
            .send(Turn {
                model: Some("gemini-multi-search".to_owned()),
                ..search()
            })
            .as_mut(),
    );
    let answer = answer_text(&updates);
    assert_eq!(
        answer,
        "Gemini search answer [Example](https://example.com/agy-search)."
    );
    assert!(!answer.contains("Let me check"));
    assert_eq!(updates.last(), Some(&Update::Completed));
}

#[test]
fn a_turn_runs_as_an_agent_named_after_the_applications_namespace() {
    let fake = FakeGemini::install(FIXTURES);
    let adapter = fake.adapter();
    run_to_end(adapter.send(ask("Say hello")).as_mut());
    run_to_end(adapter.send(search()).as_mut());

    let agents: Vec<String> = fake
        .invocations()
        .iter()
        .filter_map(|line| {
            let mut words = line.split(' ');
            words.find(|word| *word == "--agent")?;
            words.next().map(str::to_owned)
        })
        .collect();
    assert_eq!(agents, ["seatline-tests-text", "seatline-tests-search"]);
    let invocations = fake.invocations().concat();
    assert!(!invocations.contains("pervue"));
    assert!(!invocations.contains("tabbeam"));
}

#[test]
fn a_system_prompt_goes_ahead_of_the_question_and_never_onto_the_command_line() {
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(
        fake.adapter()
            .send(Turn {
                system: Some("Answer in French. SYSTEM-MARKER".to_owned()),
                ..ask("What is muse?")
            })
            .as_mut(),
    );
    assert_eq!(updates.last(), Some(&Update::Completed));
    // Antigravity reads its system prompt from the agent it runs as, and
    // treats instructions in the user's message as something to resist: so the
    // question arrives alone, and the instructions end the agent's own.
    assert_eq!(fake.prompts(), ["What is muse?"]);
    let agents = fake.read("agy-agents");
    assert!(
        agents.contains("\nAnswer in French. SYSTEM-MARKER\n"),
        "{agents}"
    );
    assert!(
        agents.find("# System Prompt").unwrap() < agents.find("SYSTEM-MARKER").unwrap(),
        "{agents}"
    );
    assert!(
        !fake.invocations().concat().contains("SYSTEM-MARKER"),
        "the system prompt reached the command line"
    );
}

#[test]
fn turns_the_adapter_cannot_serve_are_refused_before_agy_runs() {
    let fake = FakeGemini::install(FIXTURES);
    let adapter = fake.adapter();

    for (turn, refusal) in [
        (
            model("claude-test"),
            (ErrorCode::InvalidRequest, "MODEL_NOT_SUPPORTED"),
        ),
        (
            // Gemini keeps no session: only the caller's messages continue.
            Turn {
                session: SessionPolicy::Persistent,
                ..ask("hi")
            },
            (ErrorCode::InvalidRequest, "PERSISTENT_SESSION_UNSUPPORTED"),
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
    assert_eq!(transcripts(&fake), 0);
    let made = std::fs::read_dir(fake.dir.join("workspace")).map_or(0, |entries| entries.count());
    assert_eq!(made, 0, "a refused turn made a workspace");
}

#[test]
fn a_cleanup_group_names_the_transcripts_a_restarted_adapter_removes() {
    let fake = FakeGemini::install(FIXTURES);
    let cleanup_dir = fake.dir.join("durable-cleanups");
    let group = "conv_0000000000000001";
    let agy_id = "agy-restart-1";

    // A transcript and a conversation database a turn of this group left
    // behind, and the record of them.
    let transcript = fake.brain().join(agy_id).join(".system_generated/logs");
    std::fs::create_dir_all(&transcript).unwrap();
    std::fs::write(transcript.join("transcript.jsonl"), "left behind").unwrap();
    std::fs::create_dir_all(fake.conversations()).unwrap();
    let database = fake.conversations().join(format!("{agy_id}.db"));
    std::fs::write(&database, "left behind").unwrap();
    std::fs::write(format!("{}-wal", database.display()), "left behind").unwrap();
    let record = cleanup_dir.join(group);
    std::fs::create_dir_all(&record).unwrap();
    std::fs::write(record.join(agy_id), "pending\n").unwrap();

    // A fresh adapter has nothing in memory and recovers from the record.
    let adapter: Gemini = fake.adapter().with_cleanup_dir(cleanup_dir);
    let cleanup = adapter.cleanup_group(group);
    (cleanup.work)().expect("the cleanup works");
    (cleanup.completed)();
    assert!(!fake.brain().join(agy_id).exists());
    assert!(!database.exists());
    assert!(!std::path::Path::new(&format!("{}-wal", database.display())).exists());
    assert!(!record.exists());

    // A name that isn't a group, or a group with nothing left, is nothing.
    for group in ["latest", "../conv_0000000000000001", group] {
        let cleanup = adapter.cleanup_group(group);
        (cleanup.work)().expect("nothing to remove");
        (cleanup.completed)();
    }
}

#[test]
fn slow_cleanup_keeps_the_terminal_pending_without_stalling_another_app() {
    use seatline_core::work::Worker;
    use std::sync::{Arc, mpsc};
    let pool = Arc::new(Worker::new(1, 2).unwrap());
    let fake = FakeGemini::install(FIXTURES);
    let mut exchange = fake
        .adapter()
        .with_cleanup_worker(pool.clone())
        .send(ask("hello"));
    support::run_until_started(exchange.as_mut());
    let (release, wait) = mpsc::channel();
    let (entered, started) = mpsc::channel();
    let blocked = pool
        .reserve()
        .unwrap()
        .submit(move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let mut saw_text = false;
    while Instant::now() < until {
        match exchange.next(Instant::now() + Duration::from_millis(1)) {
            Some(Update::Delta(_)) => {
                saw_text = true;
            }
            Some(update) => assert!(
                !update.is_terminal(),
                "cleanup acknowledged early: {update:?}"
            ),
            None if saw_text => break,
            None => {}
        }
    }
    assert!(saw_text);
    assert!(fake.kept() > 0);
    let other = FakeGemini::install(FIXTURES);
    let updates = run_to_end(other.adapter().send(ask("hello")).as_mut());
    assert_eq!(updates.last(), Some(&Update::Completed));
    assert_eq!(other.kept(), 0);
    // Cancelling during filesystem work still waits for deletion and emits
    // exactly one terminal event. Capacity was reserved before generation.
    exchange.cancel(Duration::ZERO);
    assert!(exchange.next(Instant::now()).is_none());
    assert!(pool.reserve().is_err());
    release.send(()).unwrap();
    assert!(
        blocked
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_ok()
    );
    let updates = run_to_end(exchange.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped));
    assert_eq!(fake.kept(), 0);
}

#[test]
fn a_cleanup_failure_is_explicit_and_a_restart_retries_the_scoped_workspace_marker() {
    let fake = FakeGemini::install(FIXTURES);
    let dir = fake.dir.join("durable-cleanups");
    let adapter = fake.adapter().with_cleanup_dir(dir.clone());
    let mut exchange = adapter.send(ask("hello"));
    support::run_until_started(exchange.as_mut());
    let group = dir.join("ungrouped");
    let saved = dir.join("saved");
    std::fs::rename(&group, &saved).unwrap();
    std::fs::write(&group, b"injected non-directory").unwrap();
    let updates = run_to_end(exchange.as_mut());
    assert_eq!(
        failure(&updates),
        (ErrorCode::InternalError, "CLEANUP_FAILED")
    );
    assert!(fake.kept() > 0);
    std::fs::remove_file(&group).unwrap();
    std::fs::rename(&saved, &group).unwrap();
    let restarted = fake.adapter().with_cleanup_dir(dir);
    let cleanup = restarted.cleanup_group("ungrouped");
    (cleanup.work)().unwrap();
    (cleanup.completed)();
    assert_eq!(fake.kept(), 0);
}

#[test]
fn cancelling_a_supervised_turn_still_reports_cleanup_failure_for_retry() {
    use seatline_core::work::Worker;
    use seatline_scheduler::{EndReason, Event, Supervisor};
    use std::sync::{Arc, mpsc};
    let pool = Arc::new(Worker::new(1, 2).unwrap());
    let fake = FakeGemini::install(FIXTURES);
    let dir = fake.dir.join("durable-cleanups");
    let adapter = fake
        .adapter()
        .with_cleanup_dir(dir.clone())
        .with_cleanup_worker(pool.clone());
    let mut exchange = adapter.send(ask("hello"));
    support::run_until_started(exchange.as_mut());
    let (release, wait) = mpsc::channel();
    let (entered, started) = mpsc::channel();
    let blocked = pool
        .reserve()
        .unwrap()
        .submit(move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let group = dir.join("ungrouped");
    let saved = dir.join("saved");
    std::fs::rename(&group, &saved).unwrap();
    std::fs::write(&group, b"injected non-directory").unwrap();
    let mut supervisor = Supervisor::new();
    let id = supervisor.start(exchange, None, Duration::ZERO);
    assert!(supervisor.cancel(id));
    release.send(()).unwrap();
    blocked
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let mut terminal = Vec::new();
    while !supervisor.is_empty() && Instant::now() < until {
        terminal.extend(supervisor.poll(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(matches!(terminal.as_slice(), [Event::Ended {
        turn_id,
        reason: EndReason::Failed(error),
    }] if *turn_id == id && error.reason == "CLEANUP_FAILED" && error.retryable));
    assert!(fake.kept() > 0);
    std::fs::remove_file(&group).unwrap();
    std::fs::rename(&saved, &group).unwrap();
    let cleanup = adapter.cleanup_group("ungrouped");
    (cleanup.work)().unwrap();
    (cleanup.completed)();
    assert_eq!(fake.kept(), 0);
}

#[test]
fn a_full_cleanup_pool_refuses_generation_before_starting_a_child() {
    use seatline_core::work::Worker;
    use std::sync::Arc;
    let pool = Arc::new(Worker::new(1, 1).unwrap());
    let _reserved = pool.reserve().unwrap();
    let fake = FakeGemini::install(FIXTURES);
    let updates = run_to_end(
        fake.adapter()
            .with_cleanup_worker(pool)
            .send(ask("hello"))
            .as_mut(),
    );
    assert_eq!(
        failure(&updates),
        (ErrorCode::ProviderFailed, "CLEANUP_BACKLOG_FULL")
    );
    assert_eq!(fake.kept(), 0);
    assert!(fake.invocations().is_empty());
}

#[test]
fn failed_spawns_remove_markers_and_workspaces_before_reporting_failure() {
    let fake = FakeGemini::install(FIXTURES);
    let executable = fake.dir.join(if cfg!(windows) { "agy.exe" } else { "agy" });
    // The fixture executable may be hard-linked to the shared test binary.
    std::fs::remove_file(&executable).unwrap();
    std::fs::write(&executable, b"#!/seatline-missing-interpreter\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let dir = fake.dir.join("durable-cleanups");
    let adapter = fake.adapter().with_cleanup_dir(dir.clone());
    for _ in 0..3 {
        let updates = run_to_end(adapter.send(ask("hello")).as_mut());
        assert_eq!(
            failure(&updates),
            (ErrorCode::ProviderFailed, "PROVIDER_UNAVAILABLE")
        );
        assert!(!updates.contains(&Update::Launched));
        assert_eq!(std::fs::read_dir(dir.join("ungrouped")).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(fake.dir.join("workspace"))
                .unwrap()
                .count(),
            0
        );
    }
    assert_eq!(fake.kept(), 0);
}

fn stalled_marker_turn(drop_turn: bool) {
    use seatline_core::work::Worker;
    use std::sync::{Arc, mpsc};
    let pool = Arc::new(Worker::new(1, 3).unwrap());
    let (release, wait) = mpsc::channel();
    let (entered, started) = mpsc::channel();
    let blocked = pool
        .reserve()
        .unwrap()
        .submit(move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let fake = FakeGemini::install(FIXTURES);
    let dir = fake.dir.join("durable-cleanups");
    let mut exchange = fake
        .adapter()
        .with_cleanup_dir(dir.clone())
        .with_cleanup_worker(pool.clone())
        .send(ask("hello"));
    assert!(exchange.next(Instant::now()).is_none());
    assert!(
        fake.invocations().is_empty(),
        "launched before marker persistence"
    );
    assert!(
        !dir.join("ungrouped").exists(),
        "marker write did not use the stalled worker"
    );
    assert!(pool.reserve().is_err());
    let other = FakeGemini::install(FIXTURES);
    let updates = run_to_end(other.adapter().send(ask("hello")).as_mut());
    assert_eq!(updates.last(), Some(&Update::Completed));
    if drop_turn {
        drop(exchange);
        release.send(()).unwrap();
        blocked
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        // FIFO worker barrier: both marker persistence and dropped-turn cleanup
        // must have finished before these filesystem assertions.
        pool.reserve()
            .unwrap()
            .submit(|| Ok(()))
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    } else {
        exchange.cancel(Duration::ZERO);
        assert!(exchange.next(Instant::now()).is_none());
        release.send(()).unwrap();
        blocked
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(run_to_end(exchange.as_mut()), vec![Update::Stopped]);
    }
    assert!(fake.invocations().is_empty());
    assert_eq!(fake.kept(), 0);
    assert_eq!(std::fs::read_dir(dir.join("ungrouped")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(fake.dir.join("workspace"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn stalled_marker_persistence_does_not_block_another_app_or_pre_launch_cancellation() {
    stalled_marker_turn(false);
}

#[test]
fn dropping_a_turn_before_marker_persistence_still_cleans_up_without_launching() {
    stalled_marker_turn(true);
}

fn restart_marker(dir: &std::path::Path, name: &str, workspace: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("workspace-{name}.json")),
        serde_json::to_vec(workspace).unwrap(),
    )
    .unwrap();
}

fn workspace_transcript(fake: &FakeGemini, id: &str, workspace: &std::path::Path) {
    let dir = fake.brain().join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("transcript.jsonl"),
        serde_json::to_vec(workspace).unwrap(),
    )
    .unwrap();
}

#[test]
fn invalid_markers_are_quarantined_without_blocking_valid_group_cleanup() {
    let fake = FakeGemini::install(FIXTURES);
    let dir = fake.dir.join("durable-cleanups");
    let group = dir.join("ungrouped");
    let base = seatline_platform::workspace::prepare(&fake.dir.join("workspace")).unwrap();
    let valid = base.join("turn-valid");
    std::fs::create_dir(&valid).unwrap();
    let foreign = fake.dir.join("foreign/turn-other-app");
    std::fs::create_dir_all(&foreign).unwrap();
    restart_marker(&group, "0-foreign", &foreign);
    std::fs::write(group.join("workspace-1-malformed.json"), b"invalid JSON").unwrap();
    restart_marker(&group, "2-valid", &valid);
    workspace_transcript(&fake, "agy-valid", &valid);
    workspace_transcript(&fake, "agy-foreign", &foreign);
    let adapter = fake.adapter().with_cleanup_dir(dir.clone());
    assert!((adapter.cleanup_group("ungrouped").work)().is_err());
    assert!(!valid.exists());
    assert!(!fake.brain().join("agy-valid").exists());
    assert!(foreign.exists());
    assert!(fake.brain().join("agy-foreign").exists());
    assert_eq!(
        std::fs::read_dir(dir.join(".quarantine/ungrouped"))
            .unwrap()
            .count(),
        2
    );
    let cleanup = adapter.cleanup_group("ungrouped");
    (cleanup.work)().unwrap();
    (cleanup.completed)();
    assert!(!group.exists());
    assert!(
        dir.join(".quarantine/ungrouped/workspace-0-foreign.json")
            .exists()
    );
}

#[cfg(windows)]
#[test]
fn restart_cleanup_accepts_verbatim_case_and_separator_variants() {
    let fake = FakeGemini::install(FIXTURES);
    let dir = fake.dir.join("durable-cleanups");
    let base = seatline_platform::workspace::prepare(&fake.dir.join("workspace")).unwrap();
    let canonical = std::fs::canonicalize(&base).unwrap();
    let alternate =
        std::path::PathBuf::from(base.to_string_lossy().replace('\\', "/").to_uppercase());
    for (n, parent) in [canonical, alternate].into_iter().enumerate() {
        let stored = parent.join(format!("turn-mixed-{n}"));
        std::fs::create_dir(&stored).unwrap();
        restart_marker(&dir.join("ungrouped"), &n.to_string(), &stored);
        workspace_transcript(&fake, &format!("agy-mixed-{n}"), &stored);
    }
    let adapter = fake.adapter().with_cleanup_dir(dir.clone());
    (adapter.cleanup_group("ungrouped").work)().unwrap();
    assert_eq!(fake.kept(), 0);
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
    assert!(!dir.join(".quarantine").exists());
}

#[cfg(unix)]
#[test]
fn restart_cleanup_accepts_a_trusted_home_symlink_alias() {
    use seatline_core::discovery::SearchPath;
    let fake = FakeGemini::install(FIXTURES);
    let alias = fake.dir.join("home-alias");
    std::os::unix::fs::symlink(&fake.home, &alias).unwrap();
    let base = fake.home.join("workspace");
    seatline_platform::workspace::prepare(&base).unwrap();
    let stored = alias.join("workspace/turn-alias");
    std::fs::create_dir(&stored).unwrap();
    let dir = fake.dir.join("durable-cleanups");
    restart_marker(&dir.join("ungrouped"), "alias", &stored);
    workspace_transcript(&fake, "agy-alias", &stored);
    let adapter = Gemini::new(&FIXTURES.namespace(), SearchPath::new([]), base)
        .with_environment(fake.environment())
        .with_cleanup_dir(dir.clone());
    (adapter.cleanup_group("ungrouped").work)().unwrap();
    assert_eq!(fake.kept(), 0);
    assert!(!stored.exists());
    assert!(!dir.join(".quarantine").exists());
}

#[test]
fn pre_init_cleanup_scans_a_large_shared_tree_and_preserves_foreign_transcripts() {
    let fake = FakeGemini::install(FIXTURES);
    for n in 0..320 {
        let dir = fake
            .brain()
            .join(format!("00000000-0000-0000-0000-{n:012x}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("transcript.jsonl"),
            b"another application's private workspace",
        )
        .unwrap();
    }
    let mut exchange = fake.adapter().send(model("gemini-slow-init"));
    let until = Instant::now() + Duration::from_secs(2);
    let ready = || {
        std::fs::read_dir(fake.brain()).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .path()
                    .join(".system_generated/logs/transcript.jsonl")
                    .metadata()
                    .is_ok_and(|m| m.len() > 0)
            })
        })
    };
    while Instant::now() < until && !ready() {
        assert!(matches!(
            exchange.next(Instant::now()),
            None | Some(Update::Launched)
        ));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(ready());
    exchange.cancel(Duration::ZERO);
    let updates = run_to_end(exchange.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped));
    assert_eq!(
        fake.kept(),
        320,
        "removed foreign data or missed the turn's transcript"
    );
}
