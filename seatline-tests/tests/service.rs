//! The provider adapters behind the runtime's threaded service: what an
//! embedding application sees. A turn ends exactly once however it ends, a
//! cancelled turn or a dropped handle stops its provider process, stopping the
//! service stops every turn, and one turn's silence holds up no other.

mod support;

use std::time::{Duration, Instant};

use seatline_core::discovery::SearchPath;
use seatline_core::exchange::{Exchange, Timeouts, Update};
use seatline_core::turn::{
    Message, Namespace, Role, SessionPolicy, ToolPolicy, Turn as TurnRequest,
};
use seatline_providers::Provider;
use seatline_providers::claude::Claude;
use seatline_providers::codex::{Codex, Limits as CodexLimits};
use seatline_scheduler::{EndReason, Event, TimeoutKind};
use seatline_service::{CONSUMER_TOO_SLOW, Limits, Runtime, Turn as ServiceTurn, TurnFactory};
use support::{CLAUDE_TEST_LIMITS, FIXTURES, FakeClaude, FakeCodex, TEST_LIMITS};

/// Serves each turn with one provider.
struct Serves<P>(P);

impl<P: Provider + 'static> TurnFactory for Serves<P> {
    fn start(
        &mut self,
        request: TurnRequest,
    ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String> {
        Ok((self.0.send(request), Some(self.0.timeouts())))
    }
}

/// A service whose provider is made on its thread: an adapter keeps
/// single-threaded state, so it is never sent between threads.
fn service<P>(provider: impl FnOnce() -> P + Send + 'static) -> Runtime
where
    P: Provider + 'static,
{
    Runtime::start(Namespace::fixed("seatline-tests").unwrap(), move || {
        Box::new(Serves(provider()))
    })
}

/// Like [`service`], with `limits` for slow consumers.
fn service_with<P>(limits: Limits, provider: impl FnOnce() -> P + Send + 'static) -> Runtime
where
    P: Provider + 'static,
{
    Runtime::start_with(
        Namespace::fixed("seatline-tests").unwrap(),
        limits,
        move || Box::new(Serves(provider())),
    )
}

/// The Codex adapter that `fake` would give, made where it is called.
fn codex_of(fake: &FakeCodex, limits: CodexLimits) -> impl FnOnce() -> Codex + Send + 'static {
    let dir = fake.dir.clone();
    move || Codex::new(SearchPath::new([dir.clone()]), dir.join("work")).with_limits(limits)
}

/// The Claude adapter that `fake` would give, made where it is called.
fn claude_of(fake: &FakeClaude) -> impl FnOnce() -> Claude + Send + 'static {
    let dir = fake.dir.clone();
    move || {
        Claude::new(SearchPath::new([dir.clone()]), dir.join("claude-work"))
            .with_limits(CLAUDE_TEST_LIMITS)
    }
}

fn ask(text: &str) -> TurnRequest {
    TurnRequest {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: None,
        reasoning_effort: None,
        service_tier: None,
        tools: ToolPolicy::ProviderDefault,
        session: SessionPolicy::Ephemeral,
        continuation: None,
        cleanup_group: None,
        check_sign_in: true,
    }
}

/// What a turn showed, and how it ended.
fn finish(turn: &mut ServiceTurn) -> (String, EndReason) {
    let mut answer = String::new();
    let mut ended = Vec::new();
    while let Some(event) = turn.recv() {
        match event {
            Event::Update {
                update: Update::Delta(text),
                ..
            } => answer.push_str(&text),
            Event::Update { .. } => {}
            Event::Ended { reason, .. } => ended.push(reason),
        }
    }
    assert_eq!(ended.len(), 1, "a turn ends exactly once: {ended:?}");
    // Nothing more comes once it has ended.
    assert!(turn.recv().is_none());
    (answer, ended[0])
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let give_up = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < give_up, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn quiet_codex() -> FakeCodex {
    FakeCodex::install(FIXTURES, "goes-quiet", "signed-in")
}

#[test]
fn a_turn_through_the_service_streams_its_answer_and_ends_once() {
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let runtime = service(codex_of(&fake, TEST_LIMITS));
    let mut turn = runtime.start_turn(ask("Say hello")).unwrap();
    let (answer, reason) = finish(&mut turn);
    assert_eq!(answer, "You asked: Say hello");
    assert_eq!(reason, EndReason::Completed);
    drop(turn);
    drop(runtime);
    fake.assert_nothing_left_running();
}

#[test]
fn a_turn_the_provider_cannot_run_ends_as_a_failure_not_as_a_start_error() {
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let runtime = service(codex_of(&fake, TEST_LIMITS));
    // With no Codex to run, the turn is accepted, and ends as a failure.
    std::fs::remove_file(fake.dir.join(FakeCodex::file_name())).unwrap();
    let mut turn = runtime.start_turn(ask("hi")).unwrap();
    let (_, reason) = finish(&mut turn);
    assert!(
        matches!(reason, EndReason::Failed(failure) if failure.reason == "EXECUTABLE_NOT_FOUND"),
        "{reason:?}"
    );
    // A turn that cannot be valid never reaches the provider at all.
    let invalid = TurnRequest {
        messages: Vec::new(),
        ..ask("hi")
    };
    assert!(runtime.start_turn(invalid).is_err());
    assert!(fake.invocations().is_empty(), "{:?}", fake.invocations());
}

#[test]
fn a_cancelled_turn_ends_cancelled_and_its_process_is_reaped() {
    let fake = quiet_codex();
    let runtime = service(codex_of(&fake, TEST_LIMITS));
    let mut turn = runtime.start_turn(ask("Never finish")).unwrap();
    // Wait until the provider process is there to be stopped.
    wait_until("codex never started", || !fake.pids().is_empty());
    turn.cancel();
    let (_, reason) = finish(&mut turn);
    assert_eq!(reason, EndReason::Cancelled);
    wait_until("the process outlived its cancelled turn", || {
        fake.still_running().is_empty()
    });
}

#[test]
fn a_dropped_turn_stops_its_provider() {
    let fake = FakeClaude::install(FIXTURES, "hangs", "signed-in");
    let runtime = service(claude_of(&fake));
    let turn = runtime.start_turn(ask("Never finish")).unwrap();
    wait_until("claude never started", || !fake.pids().is_empty());
    drop(turn);
    wait_until("the process outlived its dropped turn", || {
        fake.still_running().is_empty()
    });
}

#[test]
fn stopping_the_service_stops_every_turn() {
    let fake = quiet_codex();
    let runtime = service(codex_of(&fake, TEST_LIMITS));
    let first = runtime.start_turn(ask("Never finish")).unwrap();
    let second = runtime.start_turn(ask("Not this either")).unwrap();
    wait_until("codex never started", || fake.pids().len() == 2);

    // Dropping the service waits for its turns to stop, and they end as
    // cancelled for whoever still holds them.
    drop(runtime);
    fake.assert_nothing_left_running();
    for mut turn in [first, second] {
        let (_, reason) = finish(&mut turn);
        assert_eq!(reason, EndReason::Cancelled);
    }
}

#[test]
fn a_silent_turn_times_out_and_holds_up_no_other() {
    let fake = FakeCodex::install(FIXTURES, "by-prompt", "signed-in");
    let limits = seatline_providers::codex::Limits {
        timeouts: Timeouts {
            idle: Duration::from_secs(2),
            ..TEST_LIMITS.timeouts
        },
        ..TEST_LIMITS
    };
    let runtime = service(codex_of(&fake, limits));

    let mut quiet = runtime.start_turn(ask("goes-quiet")).unwrap();
    let mut answering = runtime.start_turn(ask("answers please")).unwrap();

    // The answering turn is done long before the quiet one is given up on.
    let started = Instant::now();
    let (answer, reason) = finish(&mut answering);
    assert_eq!(answer, "You asked: answers please");
    assert_eq!(reason, EndReason::Completed);
    assert!(started.elapsed() < Duration::from_millis(1_500));

    let (_, reason) = finish(&mut quiet);
    assert_eq!(reason, EndReason::Timeout(TimeoutKind::Idle));
    drop(runtime);
    fake.assert_nothing_left_running();
}

#[test]
fn a_consumer_that_does_not_read_gets_a_prefix_of_the_answer_and_a_stop() {
    // `two-messages` answers in two pieces. A consumer that reads as they come
    // gets both.
    let fake = FakeCodex::install(FIXTURES, "two-messages", "signed-in");
    let runtime = service(codex_of(&fake, TEST_LIMITS));
    let mut turn = runtime.start_turn(ask("Say two things")).unwrap();
    let (whole, reason) = finish(&mut turn);
    assert_eq!(reason, EndReason::Completed);
    drop(turn);
    drop(runtime);

    // One that has not read by the time 150 bytes are queued, which the turn's
    // start and its first piece pass, has its turn stopped before the second:
    // what it was given is the start of the answer, whole, and it is told
    // that the rest was not kept. The process is stopped and reaped.
    let runtime = service_with(
        Limits {
            max_unread_bytes: 150,
        },
        codex_of(&fake, TEST_LIMITS),
    );
    let mut turn = runtime.start_turn(ask("Say two things")).unwrap();
    wait_until("the second answer never came", || {
        fake.invocations()
            .iter()
            .filter(|line| line.starts_with("exec "))
            .count()
            == 2
    });
    std::thread::sleep(Duration::from_millis(500));
    let (given, reason) = finish(&mut turn);
    assert!(
        matches!(reason, EndReason::Failed(failure) if failure.reason == CONSUMER_TOO_SLOW),
        "{reason:?}"
    );
    assert!(
        whole.starts_with(&given),
        "{given:?} is not the start of {whole:?}"
    );
    assert!(given.len() < whole.len(), "nothing was left out: {given:?}");
    drop(turn);
    drop(runtime);
    fake.assert_nothing_left_running();
}
