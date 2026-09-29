//! TST-04, at the runtime's level: the hostile fake-process matrix.
//!
//! Each case runs a provider adapter under the scheduler's supervisor, which
//! owns the lifecycle limits and the panic boundary, against a fake CLI that
//! misbehaves in one way: a slow stream, a stderr flood, a nonzero exit, a
//! hang, an ignored cancellation, malformed output, or large output, alone or
//! next to others. Every turn must end exactly once in its normalized outcome
//! within a time bound, with each process reaped. Where the platform shows
//! them (Linux), the process's threads and file descriptors must return to
//! where they were, and its peak memory must stay within a bound far below
//! what the provider wrote.
//!
//! An application runs the same misbehaviour through its whole host (TabBeam's is
//! in `test_provider`); this is the same through no application at all. The cases
//! run one after another in a single test, so the resource measurements see
//! only the case under way.

mod support;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use seatline_core::exchange::Timeouts;
use seatline_core::protocol::ErrorCode;
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_fake_provider::resources::{held, peak_memory_growth, reset_peak_memory, settle};
use seatline_providers::codex::Limits as CodexLimits;
use seatline_providers::{Provider, Update};
use seatline_scheduler::{EndReason, Event, Supervisor, TimeoutKind, TurnId};
use support::{CLAUDE_TEST_LIMITS, FIXTURES, FakeClaude, FakeCodex, TEST_LIMITS};

/// How long a case may wait for its turns to end. Every case expects far
/// less: this only keeps a broken runtime from hanging the test.
const CASE_LIMIT: Duration = Duration::from_secs(90);

/// How long a case may take before the test gives up on a stuck runtime.
const WATCHDOG: Duration = Duration::from_secs(120);

/// How much the peak memory may grow during a case. The floods write far more
/// than this, and the longest line the Codex adapter holds is 8 MiB.
const MEMORY_GROWTH_LIMIT_KIB: u64 = 64 * 1024;

/// What the supervisor does for a turn.
enum Step {
    /// Starts a turn asking `text`.
    Ask {
        at: Duration,
        name: &'static str,
        text: &'static str,
    },
    /// Cancels the turn started as `name`.
    Cancel { at: Duration, name: &'static str },
}

impl Step {
    fn at(&self) -> Duration {
        match self {
            Self::Ask { at, .. } | Self::Cancel { at, .. } => *at,
        }
    }
}

fn ask(at_ms: u64, name: &'static str, text: &'static str) -> Step {
    Step::Ask {
        at: Duration::from_millis(at_ms),
        name,
        text,
    }
}

fn cancel(at_ms: u64, name: &'static str) -> Step {
    Step::Cancel {
        at: Duration::from_millis(at_ms),
        name,
    }
}

/// How a turn ends.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ending {
    /// Completed, after deltas that read this answer.
    Answer(String),
    /// Failed with this code and reason.
    Failed(ErrorCode, &'static str),
    /// Cancelled, as asked.
    Cancelled,
    /// Given up on by the supervisor, for the lifecycle limit it names.
    TimedOut(TimeoutKind),
}

fn answer(text: &str) -> Ending {
    Ending::Answer(text.to_owned())
}

const IDLE: Ending = Ending::TimedOut(TimeoutKind::Idle);
const EXITED: Ending = Ending::Failed(ErrorCode::ProviderFailed, "PROCESS_EXITED");
const MALFORMED: Ending = Ending::Failed(ErrorCode::ProviderFailed, "MALFORMED_PROVIDER_OUTPUT");

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// The fake provider a case runs.
#[derive(Clone, Copy)]
enum Fake {
    /// `codex exec` behaves as `exec`, and `codex login status` as `login`.
    Codex {
        exec: &'static str,
        login: &'static str,
        limits: CodexLimits,
    },
    /// `claude -p` behaves as `print`.
    Claude {
        print: &'static str,
        limits: seatline_providers::claude::Limits,
    },
}

/// Test limits, with the start and idle timeouts given in milliseconds.
fn codex_limits(start_ms: u64, idle_ms: u64) -> CodexLimits {
    CodexLimits {
        timeouts: Timeouts {
            start: Duration::from_millis(start_ms),
            idle: Duration::from_millis(idle_ms),
            ..TEST_LIMITS.timeouts
        },
        ..TEST_LIMITS
    }
}

fn claude_limits(start_ms: u64, idle_ms: u64) -> seatline_providers::claude::Limits {
    seatline_providers::claude::Limits {
        timeouts: Timeouts {
            start: Duration::from_millis(start_ms),
            idle: Duration::from_millis(idle_ms),
            ..CLAUDE_TEST_LIMITS.timeouts
        },
        ..CLAUDE_TEST_LIMITS
    }
}

struct Case {
    name: &'static str,
    /// Turns that must have reported at least this many progress updates, so a
    /// case that floods really flooded, and its memory bound means something.
    flooded: &'static [(&'static str, usize)],
    fake: Fake,
    steps: Vec<Step>,
    /// Each turn's ending, and at the latest how long after it started it
    /// comes.
    expect: Vec<(&'static str, Ending, Duration)>,
}

fn cases() -> Vec<Case> {
    let codex = |exec, limits| Fake::Codex {
        exec,
        login: "signed-in",
        limits,
    };
    let quick = codex_limits(1_000, 1_000);
    vec![
        Case {
            name: "slow stream: every line arrives a few bytes at a time",
            flooded: &[],
            fake: codex("dribble", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Dribble é✓😀 for me")],
            expect: vec![("turn", answer("You asked: Dribble é✓😀 for me"), secs(5))],
        },
        Case {
            name: "stderr flood: 128 MiB of stderr while answering",
            flooded: &[],
            fake: codex("stderr-flood", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Answer through the noise")],
            expect: vec![(
                "turn",
                answer("You asked: Answer through the noise"),
                secs(30),
            )],
        },
        Case {
            name: "stderr without end: stderr is not progress",
            flooded: &[],
            fake: codex("endless-stderr", quick),
            steps: vec![ask(0, "turn", "Say nothing")],
            expect: vec![("turn", IDLE, secs(5))],
        },
        Case {
            name: "stdout flood: 100,000 progress events, then the answer",
            flooded: &[("turn", 100_000)],
            fake: codex("stdout-flood", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Work hard")],
            expect: vec![("turn", answer("Done flooding."), secs(30))],
        },
        Case {
            name: "progress without end, cancelled",
            flooded: &[("turn", 10_000)],
            fake: codex("endless-flood", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Never finish"), cancel(500, "turn")],
            expect: vec![("turn", Ending::Cancelled, secs(5))],
        },
        Case {
            name: "unknown events without end: not progress",
            flooded: &[],
            fake: codex("unknown-flood", quick),
            steps: vec![ask(0, "turn", "Speak in riddles")],
            expect: vec![("turn", IDLE, secs(5))],
        },
        Case {
            name: "nonzero exit mid-turn",
            flooded: &[],
            fake: codex("exits-nonzero", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Give up")],
            expect: vec![("turn", EXITED, secs(5))],
        },
        Case {
            name: "crash mid-turn",
            flooded: &[],
            fake: codex("crashes", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Fall over")],
            expect: vec![("turn", EXITED, secs(5))],
        },
        Case {
            name: "hang before the turn starts",
            flooded: &[],
            fake: codex("never-starts", quick),
            steps: vec![ask(0, "turn", "Wait forever")],
            expect: vec![("turn", Ending::TimedOut(TimeoutKind::Start), secs(5))],
        },
        Case {
            name: "hang mid-turn",
            flooded: &[],
            fake: codex("goes-quiet", quick),
            steps: vec![ask(0, "turn", "Go quiet")],
            expect: vec![("turn", IDLE, secs(5))],
        },
        Case {
            name: "ignored cancellation: killed after the grace period",
            flooded: &[],
            fake: codex("ignores-cancel", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Ignore me"), cancel(500, "turn")],
            expect: vec![("turn", Ending::Cancelled, secs(5))],
        },
        Case {
            name: "ignored cancellation while flooding",
            flooded: &[("turn", 10_000)],
            fake: codex("floods-and-ignores-cancel", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Ignore me loudly"), cancel(500, "turn")],
            expect: vec![("turn", Ending::Cancelled, secs(5))],
        },
        Case {
            name: "malformed output: not JSON",
            flooded: &[],
            fake: codex("malformed", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Garble")],
            expect: vec![("turn", MALFORMED, secs(5))],
        },
        Case {
            name: "malformed output: not UTF-8",
            flooded: &[],
            fake: codex("invalid-utf8", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Garble bytes")],
            expect: vec![("turn", MALFORMED, secs(5))],
        },
        Case {
            name: "large output: a 330 KB answer",
            flooded: &[],
            fake: codex("huge", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Say a lot")],
            expect: vec![("turn", Ending::Answer("é✓😀 ".repeat(30_000)), secs(10))],
        },
        Case {
            name: "large output: a 9 MiB line",
            flooded: &[],
            fake: codex("oversized", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Say too much")],
            expect: vec![("turn", MALFORMED, secs(10))],
        },
        Case {
            name: "large output: a line without end",
            flooded: &[],
            fake: codex("endless-line", TEST_LIMITS),
            steps: vec![ask(0, "turn", "Never stop talking")],
            expect: vec![("turn", MALFORMED, secs(10))],
        },
        Case {
            name: "a sign-in check that floods: given up on, and the question asked",
            flooded: &[],
            fake: Fake::Codex {
                exec: "answers",
                login: "floods",
                limits: CodexLimits {
                    probe: Duration::from_millis(1_000),
                    ..TEST_LIMITS
                },
            },
            steps: vec![ask(0, "turn", "Ask anyway")],
            expect: vec![("turn", answer("You asked: Ask anyway"), secs(5))],
        },
        Case {
            name: "hostile neighbours: a plain answer and a cancel get through",
            flooded: &[("flood", 10_000)],
            fake: codex("by-prompt", codex_limits(10_000, 2_000)),
            steps: vec![
                ask(0, "flood", "endless-flood"),
                ask(0, "stderr", "endless-stderr"),
                ask(0, "line", "endless-line"),
                ask(0, "quiet", "goes-quiet"),
                ask(300, "answer", "answers please"),
                cancel(700, "flood"),
            ],
            expect: vec![
                ("answer", answer("You asked: answers please"), secs(2)),
                ("flood", Ending::Cancelled, secs(4)),
                ("stderr", IDLE, secs(6)),
                ("line", MALFORMED, secs(10)),
                ("quiet", IDLE, secs(6)),
            ],
        },
        // Claude speaks another dialect, and gets the same treatment.
        Case {
            name: "claude: silence after its first event",
            flooded: &[],
            fake: Fake::Claude {
                print: "hangs",
                limits: claude_limits(1_000, 1_000),
            },
            steps: vec![ask(0, "turn", "Go quiet")],
            expect: vec![("turn", IDLE, secs(5))],
        },
        Case {
            name: "claude: events it does not know are not progress",
            flooded: &[],
            fake: Fake::Claude {
                print: "flooding",
                limits: claude_limits(1_000, 1_000),
            },
            steps: vec![ask(0, "turn", "Speak in riddles")],
            expect: vec![("turn", IDLE, secs(5))],
        },
        Case {
            name: "claude: output that is not JSON",
            flooded: &[],
            fake: Fake::Claude {
                print: "malformed",
                limits: CLAUDE_TEST_LIMITS,
            },
            steps: vec![ask(0, "turn", "Garble")],
            expect: vec![("turn", MALFORMED, secs(5))],
        },
        Case {
            name: "claude: a clean exit without a result is malformed output",
            flooded: &[],
            fake: Fake::Claude {
                print: "no-result",
                limits: CLAUDE_TEST_LIMITS,
            },
            steps: vec![ask(0, "turn", "Vanish")],
            expect: vec![("turn", MALFORMED, secs(5))],
        },
        Case {
            name: "claude: ignored cancellation: killed after the grace period",
            flooded: &[],
            fake: Fake::Claude {
                print: "ignores-cancel",
                limits: CLAUDE_TEST_LIMITS,
            },
            steps: vec![ask(0, "turn", "Ignore me"), cancel(500, "turn")],
            expect: vec![("turn", Ending::Cancelled, secs(5))],
        },
    ]
}

/// A turn of one question, with no state kept and Codex's or Claude's own
/// tools as configured.
fn turn(text: &str) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: None,
        tools: ToolPolicy::ProviderDefault,
        session: SessionPolicy::Ephemeral,
        continuation: None,
        cleanup_group: None,
        check_sign_in: true,
    }
}

/// What a turn showed the supervisor, and when it ended.
#[derive(Default)]
struct Seen {
    started_at: Option<Instant>,
    /// Everything but the progress counted in `activity`.
    events: Vec<Event>,
    activity: usize,
    ended_at: Option<Instant>,
}

impl Seen {
    fn answer(&self) -> String {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Update {
                    update: Update::Delta(text),
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reason(&self) -> Option<EndReason> {
        match self.events.last() {
            Some(Event::Ended { reason, .. }) => Some(*reason),
            _ => None,
        }
    }
}

/// Files what the supervisor reported under the turns it is about. Progress
/// with nothing to show (`Update::Activity`) is only counted: a provider that
/// floods progress reports hundreds of thousands of them, and a log of every one
/// would be the largest thing the test holds, and what its memory bound measures.
fn record(seen: &mut HashMap<TurnId, Seen>, events: Vec<Event>) {
    for event in events {
        let (Event::Update { turn_id, .. } | Event::Ended { turn_id, .. }) = &event;
        let entry = seen.entry(*turn_id).or_default();
        match event {
            Event::Update {
                update: Update::Activity,
                ..
            } => entry.activity += 1,
            Event::Ended { .. } => {
                entry.ended_at = Some(Instant::now());
                entry.events.push(event);
            }
            Event::Update { .. } => entry.events.push(event),
        }
    }
}

/// Runs `steps` under a supervisor, as the service does: every turn's events
/// and the moment each ended.
fn drive(
    provider: &dyn Provider,
    steps: &[Step],
    names: &[&'static str],
) -> HashMap<&'static str, Seen> {
    let mut supervisor = Supervisor::new();
    let start = Instant::now();
    let mut pending: Vec<&Step> = steps.iter().collect();
    pending.sort_by_key(|step| step.at());
    pending.reverse();
    let mut ids: HashMap<&'static str, TurnId> = HashMap::new();
    let mut seen: HashMap<TurnId, Seen> = HashMap::new();

    loop {
        while pending
            .last()
            .is_some_and(|step| start.elapsed() >= step.at())
        {
            match pending.pop().expect("a step is due") {
                Step::Ask { name, text, .. } => {
                    let id = supervisor.start(
                        provider.send(turn(text)),
                        Some(provider.timeouts()),
                        Duration::from_millis(250),
                    );
                    ids.insert(name, id);
                    seen.entry(id).or_default().started_at = Some(Instant::now());
                }
                Step::Cancel { name, .. } => {
                    supervisor.cancel(ids[name]);
                }
            }
        }

        record(&mut seen, supervisor.poll(Duration::from_millis(5)));

        let done = pending.is_empty()
            && names
                .iter()
                .all(|name| ids.get(name).is_some_and(|id| seen[id].ended_at.is_some()));
        if done {
            // The turns that were not waited for are given a moment to end.
            let stragglers = Instant::now() + Duration::from_secs(3);
            while !supervisor.is_empty() && Instant::now() < stragglers {
                record(&mut seen, supervisor.poll(Duration::from_millis(5)));
            }
            break;
        }
        assert!(
            start.elapsed() < CASE_LIMIT,
            "the runtime did not end every turn in {CASE_LIMIT:?}"
        );
        if supervisor.is_empty() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    ids.into_iter()
        .map(|(name, id)| (name, seen.remove(&id).unwrap_or_default()))
        .collect()
}

fn run(case: &Case) {
    let (provider, assert_nothing_left_running): (Box<dyn Provider>, Box<dyn Fn()>) =
        match case.fake {
            Fake::Codex {
                exec,
                login,
                limits,
            } => {
                let fake = FakeCodex::install(FIXTURES, exec, login);
                let provider = fake.adapter_with(limits);
                (
                    Box::new(provider),
                    Box::new(move || fake.assert_nothing_left_running()),
                )
            }
            Fake::Claude { print, limits } => {
                let fake = FakeClaude::install(FIXTURES, print, "signed-in");
                let provider = fake.adapter_with(limits);
                (
                    Box::new(provider),
                    Box::new(move || fake.assert_nothing_left_running()),
                )
            }
        };

    let names: Vec<&'static str> = case.expect.iter().map(|(name, _, _)| *name).collect();
    let seen = drive(provider.as_ref(), &case.steps, &names);

    for (name, ending, within) in &case.expect {
        check(case, name, &seen[name], ending, *within);
    }
    for (name, minimum) in case.flooded {
        assert!(
            seen[name].activity >= *minimum,
            "{}: {name} reported {} progress updates, fewer than {minimum}",
            case.name,
            seen[name].activity
        );
    }

    // Nothing the provider wrote to stderr reaches the events.
    let everything = format!(
        "{:?}",
        seen.values().map(|seen| &seen.events).collect::<Vec<_>>()
    );
    assert!(
        !everything.contains("SECRET"),
        "{}: a secret leaked",
        case.name
    );
    drop(provider);
    assert_nothing_left_running();
}

fn check(case: &Case, name: &str, seen: &Seen, ending: &Ending, within: Duration) {
    let case_name = case.name;
    let ends = seen
        .events
        .iter()
        .filter(|event| matches!(event, Event::Ended { .. }))
        .count();
    assert_eq!(
        ends, 1,
        "{case_name}: {name} should end exactly once: {:?}",
        seen.events
    );
    let reason = seen
        .reason()
        .unwrap_or_else(|| panic!("{case_name}: {name} ended early: {:?}", seen.events));

    let took = seen
        .ended_at
        .zip(seen.started_at)
        .map(|(ended, started)| ended.saturating_duration_since(started))
        .expect("the turn started and ended");
    eprintln!("          {name}: {took:.2?} of {within:?}");
    assert!(
        took <= within,
        "{case_name}: {name} took {took:?}, more than {within:?}"
    );

    match ending {
        Ending::Answer(text) => {
            assert_eq!(reason, EndReason::Completed, "{case_name}: {name}");
            assert!(
                seen.answer() == *text,
                "{case_name}: {name} answered differently"
            );
            // Started comes before any of the answer.
            let started = seen.events.iter().position(|event| {
                matches!(
                    event,
                    Event::Update {
                        update: Update::Started,
                        ..
                    }
                )
            });
            let first_delta = seen.events.iter().position(|event| {
                matches!(
                    event,
                    Event::Update {
                        update: Update::Delta(_),
                        ..
                    }
                )
            });
            assert!(
                started.is_some() && started < first_delta,
                "{case_name}: {name}: {:?}",
                seen.events
            );
        }
        Ending::Failed(code, why) => {
            let EndReason::Failed(failure) = reason else {
                panic!("{case_name}: {name} ended as {reason:?}, not as a failure");
            };
            assert_eq!(
                (failure.code, failure.reason),
                (*code, *why),
                "{case_name}: {name}"
            );
        }
        Ending::Cancelled => assert_eq!(reason, EndReason::Cancelled, "{case_name}: {name}"),
        Ending::TimedOut(kind) => {
            assert_eq!(reason, EndReason::Timeout(*kind), "{case_name}: {name}");
        }
    }
}

#[test]
fn hostile_providers_end_normalized_with_bounded_resources() {
    let baseline = held();
    for case in cases() {
        let name = case.name;
        let memory_before = reset_peak_memory();
        let started = Instant::now();
        // A watchdog: a runtime stuck in a loop fails the case instead of
        // hanging the test.
        let (done, finished) = std::sync::mpsc::channel();
        let runner = std::thread::spawn(move || {
            run(&case);
            let _ = done.send(());
        });
        match finished.recv_timeout(WATCHDOG) {
            Ok(()) => runner.join().expect("the case runs"),
            // The case panicked: report its own message.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(panic) = runner.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{name}: the runtime is stuck")
            }
        }
        let growth = peak_memory_growth(memory_before);
        eprintln!(
            "{:>8.2?}  {name} (peak memory +{} KiB)",
            started.elapsed(),
            growth.map_or_else(|| "?".to_owned(), |growth| growth.to_string())
        );
        if let Some(growth) = growth {
            assert!(
                growth <= MEMORY_GROWTH_LIMIT_KIB,
                "{name}: peak memory grew by {growth} KiB"
            );
        }
        assert_eq!(
            settle(baseline),
            baseline,
            "{name}: threads or file descriptors were left behind"
        );
    }
}
