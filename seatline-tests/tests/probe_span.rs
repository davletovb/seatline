//! The sign-in probe as phase telemetry sees it (B-01): Codex and Claude report
//! the stretch they spent probing; adapters without a probe report none. Every
//! adapter also reports when it read the provider's final result and what the
//! provider said it used (I-01). Marks are compared by order only, never
//! against a wall-clock threshold.

mod support;

use std::time::{Duration, Instant};

use seatline_core::exchange::Exchange;
use seatline_core::telemetry::{Kind, Timeline};
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn, Usage};
use seatline_providers::{Provider, Update};
use seatline_scheduler::{EndReason, Event, Scheduler};
use support::{FIXTURES, FakeClaude, FakeCodex, FakeGemini, FakeGrok, run_to_end};

fn ask(check_sign_in: bool) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: "hello".to_owned(),
        }],
        model: None,
        reasoning_effort: None,
        service_tier: None,
        tools: ToolPolicy::ProviderDefault,
        session: SessionPolicy::Persistent,
        continuation: None,
        cleanup_group: None,
        check_sign_in,
    }
}

/// Runs the exchange `build` makes under a scheduler with a timeline, to its end.
///
/// `build` makes the exchange *after* the timeline exists and the request is
/// admitted, as the hub does (received, admitted, then the adapter builds): an
/// adapter takes its probe's start instant while it builds, so building first
/// would put that instant before the request was received.
fn timed(build: impl FnOnce() -> Box<dyn Exchange>, kind: Kind) -> (EndReason, Timeline) {
    let mut scheduler = Scheduler::new();
    let mut timeline = Timeline::new(kind, Instant::now());
    timeline.admitted(Instant::now());
    let exchange = build();
    let id = scheduler.start_timed(
        exchange,
        Some(support::TEST_LIMITS.timeouts),
        Duration::from_secs(1),
        timeline,
    );
    let give_up = Instant::now() + support::DEADLINE;
    while Instant::now() < give_up {
        for event in scheduler.poll(Duration::from_millis(5)) {
            if let Event::Ended { reason, .. } = event {
                let timeline = scheduler.take_timeline(id).expect("a timeline");
                return (reason, timeline);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("the exchange never ended");
}

/// The marks that are present, in the order the runtime must have taken them.
fn in_order(timeline: &Timeline) -> Vec<(&'static str, u64)> {
    let marks = timeline.marks();
    [
        ("admitted", marks.admitted),
        ("probe_started", marks.probe_started),
        ("probe_ended", marks.probe_ended),
        ("launched", marks.launched),
        ("started", marks.started),
        ("first_text", marks.first_text),
        ("terminal", marks.terminal),
        ("released", marks.released),
    ]
    .into_iter()
    .filter_map(|(name, at)| Some((name, at?)))
    .collect()
}

fn assert_ordered(timeline: &Timeline) {
    let marks = in_order(timeline);
    assert!(
        marks.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "marks out of order: {marks:?}"
    );
    assert_eq!(timeline.phases().sum(), timeline.total_us());
}

fn names(timeline: &Timeline) -> Vec<&'static str> {
    in_order(timeline)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn codex_reports_the_probe_it_ran_before_its_turn() {
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let (reason, timeline) = timed(|| fake.adapter().send(ask(true)), Kind::Send);
    assert_eq!(reason, EndReason::Completed);
    assert_eq!(
        names(&timeline),
        [
            "admitted",
            "probe_started",
            "probe_ended",
            "launched",
            "started",
            "first_text",
            "terminal",
            "released"
        ]
    );
    assert_ordered(&timeline);
    assert_eq!((timeline.probes(), timeline.launches()), (1, 1));
    // The fake saw one probe and one turn: the counts are the truth.
    let invocations = fake.invocations();
    assert_eq!(
        invocations
            .iter()
            .filter(|l| l.starts_with("login "))
            .count(),
        1,
        "fake invocations: {invocations:?}; telemetry marks: {:?}",
        in_order(&timeline)
    );
    assert_eq!(
        invocations
            .iter()
            .filter(|l| l.starts_with("exec "))
            .count(),
        1,
        "fake invocations: {invocations:?}; telemetry marks: {:?}",
        in_order(&timeline)
    );
}

#[test]
fn a_turn_that_asks_for_no_probe_has_no_probe_span() {
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let mut exchange = fake.adapter().send(ask(false));
    assert!(exchange.probe_span().is_none());
    run_to_end(exchange.as_mut());
    assert!(exchange.probe_span().is_none());
    assert_eq!(
        fake.invocations()
            .iter()
            .filter(|l| l.starts_with("login "))
            .count(),
        0
    );
}

#[test]
fn a_signed_out_probe_ends_the_span_and_the_request_before_any_launch() {
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-out");
    let (reason, timeline) = timed(|| fake.adapter().send(ask(true)), Kind::Send);
    assert!(matches!(reason, EndReason::Failed(_)), "{reason:?}");
    assert_eq!(
        names(&timeline),
        [
            "admitted",
            "probe_started",
            "probe_ended",
            "terminal",
            "released"
        ]
    );
    assert_ordered(&timeline);
    assert_eq!((timeline.probes(), timeline.launches()), (1, 0));
    assert!(!timeline.text());
}

#[test]
fn cancelling_during_the_probe_ends_the_span() {
    let fake = FakeCodex::install(FIXTURES, "answers", "hangs");
    let mut exchange = fake.adapter().send(ask(true));
    assert!(
        exchange.probe_span().is_some_and(|span| span.end.is_none()),
        "the probe is still running"
    );
    exchange.cancel(Duration::ZERO);
    let updates = run_to_end(exchange.as_mut());
    assert_eq!(updates.last(), Some(&Update::Stopped));
    let span = exchange.probe_span().expect("the probe's span");
    assert!(span.end.is_some_and(|end| end >= span.start));
}

#[test]
fn claude_reports_the_probe_it_ran_before_its_turn() {
    let fake = FakeClaude::install(FIXTURES, "answers", "signed-in");
    let (reason, timeline) = timed(|| fake.adapter().send(ask(true)), Kind::Send);
    assert_eq!(reason, EndReason::Completed);
    assert!(names(&timeline).contains(&"probe_started"));
    assert!(names(&timeline).contains(&"probe_ended"));
    assert_ordered(&timeline);
    assert_eq!((timeline.probes(), timeline.launches()), (1, 1));

    let mut without = fake.adapter().send(ask(false));
    run_to_end(without.as_mut());
    assert!(without.probe_span().is_none());
}

#[test]
fn a_status_request_is_counted_as_one_readiness_check() {
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let (reason, timeline) = timed(|| fake.adapter().status(), Kind::Status);
    assert_eq!(reason, EndReason::Completed);
    assert!(timeline.marks().status.is_some());
    assert_eq!((timeline.probes(), timeline.launches()), (1, 0));
    assert_ordered(&timeline);
}

#[test]
fn adapters_without_a_sign_in_probe_report_none() {
    let gemini = FakeGemini::install(FIXTURES);
    let mut turn = Turn {
        model: Some("gemini-test".to_owned()),
        tools: ToolPolicy::None,
        session: SessionPolicy::Ephemeral,
        ..ask(true)
    };
    let mut exchange = gemini.adapter().send(turn.clone());
    run_to_end(exchange.as_mut());
    assert!(exchange.probe_span().is_none());

    let grok = FakeGrok::install(FIXTURES);
    turn.model = Some("grok-4.6".to_owned());
    let mut exchange = grok.adapter().send(turn);
    run_to_end(exchange.as_mut());
    assert!(exchange.probe_span().is_none());
}

#[test]
fn every_adapter_reports_when_it_read_the_providers_result_and_what_it_used() {
    // Codex: the fake reports its counts the way Codex 0.156 does.
    let codex = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let (reason, timeline) = timed(|| codex.adapter().send(ask(false)), Kind::Send);
    assert_eq!(reason, EndReason::Completed);
    assert!(timeline.tail_us().is_some());
    assert_eq!(
        timeline.usage(),
        Some(Usage {
            input_tokens: Some(12),
            output_tokens: Some(7),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            reasoning_output_tokens: Some(0),
        })
    );
    assert_ordered(&timeline);

    // Claude: the fake's result line carries no usage, so there is none.
    let claude = FakeClaude::install(FIXTURES, "answers", "signed-in");
    let (reason, timeline) = timed(|| claude.adapter().send(ask(false)), Kind::Send);
    assert_eq!(reason, EndReason::Completed);
    assert!(timeline.tail_us().is_some());
    assert_eq!(timeline.usage(), None);

    // A turn that failed has a result too, and a tail after it: what an
    // application waits after being refused is as real as after an answer.
    claude.set("fails-rate", "signed-in");
    let (reason, timeline) = timed(|| claude.adapter().send(ask(false)), Kind::Send);
    assert!(matches!(reason, EndReason::Failed(_)), "{reason:?}");
    assert!(timeline.tail_us().is_some());

    // A Claude that ends without ever sending one reports no result at all,
    // so there is nothing to measure a tail from.
    claude.set("no-result", "signed-in");
    let (reason, timeline) = timed(|| claude.adapter().send(ask(false)), Kind::Send);
    assert!(matches!(reason, EndReason::Failed(_)), "{reason:?}");
    assert_eq!(timeline.tail_us(), None);

    // Antigravity (Gemini): totals only.
    let gemini = FakeGemini::install(FIXTURES);
    let turn = Turn {
        model: Some("gemini-test".to_owned()),
        tools: ToolPolicy::None,
        session: SessionPolicy::Ephemeral,
        ..ask(false)
    };
    let (reason, timeline) = timed(|| gemini.adapter().send(turn.clone()), Kind::Send);
    assert_eq!(reason, EndReason::Completed);
    assert!(timeline.tail_us().is_some());
    assert_eq!(
        timeline.usage(),
        Some(Usage {
            input_tokens: Some(17),
            output_tokens: Some(9),
            ..Usage::default()
        })
    );

    // Grok.
    let grok = FakeGrok::install(FIXTURES);
    let turn = Turn {
        model: Some("grok-4.6".to_owned()),
        ..turn
    };
    let (reason, timeline) = timed(|| grok.adapter().send(turn.clone()), Kind::Send);
    assert_eq!(reason, EndReason::Completed);
    assert!(timeline.tail_us().is_some());
}
