//! Provider-neutral contract cases (TST-10), run unchanged against all four
//! adapters through the runtime's `Provider` trait alone: the lifecycle of a
//! turn, what a provider may refuse, how sessions and search show in the
//! capabilities, how a turn is cancelled, and that a caller's deadline holds.
//!
//! A provider differs from another only in what its capabilities say. Codex
//! and Claude keep native sessions and search; Gemini and Grok are stateless,
//! and Grok has no search at all. The cases read that from the provider and
//! hold each to what it says.

mod support;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use seatline_core::protocol::{Authentication, Availability, Capability, ErrorCode};
use seatline_core::readiness::{Freshness, SignInPolicy};
use seatline_core::turn::{
    Message, ReasoningEffort, Role, ServiceTier, SessionPolicy, ToolPolicy, Turn,
};
use seatline_providers::{BUSY_LIMIT, Provider, Update, readiness::Ready};
use support::{
    FIXTURES, FakeClaude, FakeCodex, FakeGemini, FakeGrok, answer_text, failure, run_to_end,
    run_until_started, session_of, visible,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Codex,
    Claude,
    Gemini,
    Grok,
}

const ALL: [Kind; 4] = [Kind::Codex, Kind::Claude, Kind::Gemini, Kind::Grok];

#[test]
fn unsupported_service_tiers_are_refused_before_direct_or_readiness_processes() {
    for kind in [Kind::Claude, Kind::Gemini, Kind::Grok] {
        for missing in [false, true] {
            let rig = if missing {
                Rig::without_executable(kind)
            } else {
                Rig::new(kind, Behaviour::Answers)
            };
            assert_eq!(
                rig.provider.capabilities().service_tier,
                Capability::Unsupported
            );
            let ready = Ready::boxed(match &rig.fixture {
                Fixture::Claude(fake) => Box::new(fake.adapter()),
                Fixture::Gemini(fake) => Box::new(fake.adapter()),
                Fixture::Grok(fake) => Box::new(fake.adapter()),
                Fixture::Codex(_) => unreachable!(),
            });
            for tier in [ServiceTier::Standard, ServiceTier::Fast] {
                let request = Turn {
                    service_tier: Some(tier),
                    ..rig.ask("hi")
                };
                for mut exchange in [
                    rig.provider.send(request.clone()),
                    ready.send_with_readiness(request.clone(), Freshness::Fresh),
                    ready.send_with_readiness_policy(
                        request,
                        Freshness::Fresh,
                        SignInPolicy::try_from(vec![
                            seatline_core::turn::SignInClassification::Subscription,
                        ])
                        .unwrap(),
                    ),
                ] {
                    let updates = run_to_end(exchange.as_mut());
                    assert_eq!(
                        failure(&updates),
                        (ErrorCode::InvalidRequest, "SERVICE_TIER_UNSUPPORTED")
                    );
                    assert_eq!(rig.command_lines(), "");
                    rig.assert_nothing_left();
                }
            }
        }
    }
}

#[test]
fn adapters_refuse_an_effort_choice_they_cannot_honor_without_launching() {
    for kind in [Kind::Gemini, Kind::Grok] {
        for missing in [false, true] {
            let rig = if missing {
                Rig::without_executable(kind)
            } else {
                Rig::new(kind, Behaviour::Answers)
            };
            assert_eq!(
                rig.provider.capabilities().reasoning_effort,
                Capability::Unsupported
            );
            let request = Turn {
                reasoning_effort: Some(ReasoningEffort::Low),
                ..rig.ask("hi")
            };
            // Neither status probes nor generations may launch, even when readiness
            // would fail because the executable is absent or the account is signed out.
            let ready = Ready::boxed(match &rig.fixture {
                Fixture::Claude(fake) => Box::new(fake.adapter()),
                Fixture::Gemini(fake) => Box::new(fake.adapter()),
                Fixture::Grok(fake) => Box::new(fake.adapter()),
                Fixture::Codex(_) => unreachable!(),
            });
            for mut exchange in [
                rig.provider.send(request.clone()),
                ready.send_with_readiness(request.clone(), Freshness::Fresh),
                ready.send_with_readiness_policy(
                    request,
                    Freshness::Fresh,
                    SignInPolicy::try_from(vec![
                        seatline_core::turn::SignInClassification::Subscription,
                    ])
                    .unwrap(),
                ),
            ] {
                let updates = run_to_end(exchange.as_mut());
                assert_eq!(
                    failure(&updates),
                    (ErrorCode::InvalidRequest, "REASONING_EFFORT_UNSUPPORTED")
                );
                assert_eq!(rig.command_lines(), "");
                rig.assert_nothing_left();
            }
        }
    }
}

#[test]
fn claude_takes_every_effort_but_none_and_refuses_that_one_before_any_probe_or_launch() {
    for missing in [false, true] {
        let rig = if missing {
            Rig::without_executable(Kind::Claude)
        } else {
            Rig::new(Kind::Claude, Behaviour::Answers)
        };
        assert_eq!(
            rig.provider.capabilities().reasoning_effort,
            Capability::Supported
        );
        let ready = Ready::boxed(match &rig.fixture {
            Fixture::Claude(fake) => Box::new(fake.adapter()),
            _ => unreachable!(),
        });
        let request = Turn {
            reasoning_effort: Some(ReasoningEffort::None),
            ..rig.ask("hi")
        };
        // Claude has no such level. Neither a status probe nor a generation may
        // launch, even when readiness would fail because the executable is
        // absent: the answer to a level it lacks does not depend on the machine.
        for mut exchange in [
            rig.provider.send(request.clone()),
            ready.send_with_readiness(request.clone(), Freshness::Fresh),
            ready.send_with_readiness_policy(
                request,
                Freshness::Fresh,
                SignInPolicy::try_from(vec![
                    seatline_core::turn::SignInClassification::Subscription,
                ])
                .unwrap(),
            ),
        ] {
            let updates = run_to_end(exchange.as_mut());
            assert_eq!(
                failure(&updates),
                (ErrorCode::InvalidRequest, "REASONING_EFFORT_UNSUPPORTED")
            );
            assert_eq!(rig.command_lines(), "");
            rig.assert_nothing_left();
        }
    }
}

/// How the fake provider behaves once a turn reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// It answers.
    Answers,
    /// It starts the turn, and then says nothing more.
    Hangs,
}

/// The fake provider a case runs against, and the adapter that runs it.
struct Rig {
    kind: Kind,
    provider: Box<dyn Provider>,
    fixture: Fixture,
    /// The model the turns ask for: the fakes of Gemini and Grok take their
    /// behaviour from it.
    model: Option<&'static str>,
}

enum Fixture {
    Codex(FakeCodex),
    Claude(FakeClaude),
    Gemini(FakeGemini),
    Grok(FakeGrok),
}

impl Rig {
    fn new(kind: Kind, behaviour: Behaviour) -> Self {
        let answers = behaviour == Behaviour::Answers;
        match kind {
            Kind::Codex => {
                let codex = FakeCodex::install(
                    FIXTURES,
                    if answers { "answers" } else { "goes-quiet" },
                    "signed-in",
                );
                let home = codex.dir.join("codex-home");
                std::fs::create_dir_all(&home).unwrap();
                let provider = codex.adapter_with_env([
                    (OsString::from("CODEX_HOME"), home.into_os_string()),
                    (
                        OsString::from("PATH"),
                        std::env::var_os("PATH").unwrap_or_default(),
                    ),
                ]);
                Self {
                    kind,
                    provider: Box::new(provider),
                    fixture: Fixture::Codex(codex),
                    model: None,
                }
            }
            Kind::Claude => {
                let claude = FakeClaude::install(
                    FIXTURES,
                    if answers { "answers" } else { "hangs" },
                    "signed-in",
                );
                Self {
                    kind,
                    provider: Box::new(claude.adapter()),
                    fixture: Fixture::Claude(claude),
                    model: None,
                }
            }
            Kind::Gemini => {
                let gemini = FakeGemini::install(FIXTURES);
                Self {
                    kind,
                    provider: Box::new(gemini.adapter()),
                    fixture: Fixture::Gemini(gemini),
                    model: Some(if answers {
                        "gemini-test"
                    } else {
                        "gemini-hang"
                    }),
                }
            }
            Kind::Grok => {
                let grok = FakeGrok::install(FIXTURES);
                Self {
                    kind,
                    provider: Box::new(grok.adapter()),
                    fixture: Fixture::Grok(grok),
                    model: Some(if answers { "grok-4.6" } else { "grok-hang" }),
                }
            }
        }
    }

    fn directory(&self) -> &PathBuf {
        match &self.fixture {
            Fixture::Codex(fake) => &fake.dir,
            Fixture::Claude(fake) => &fake.dir,
            Fixture::Gemini(fake) => &fake.dir,
            Fixture::Grok(fake) => &fake.dir,
        }
    }

    /// The same provider with its executable gone.
    fn without_executable(kind: Kind) -> Self {
        let rig = Self::new(kind, Behaviour::Answers);
        let name = match kind {
            Kind::Codex => "codex",
            Kind::Claude => "claude",
            Kind::Gemini => "agy",
            Kind::Grok => "grok",
        };
        let file = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        };
        std::fs::remove_file(rig.directory().join(file)).expect("the fake is installed");
        rig
    }

    /// A turn of one plain question, with no state kept.
    fn ask(&self, text: &str) -> Turn {
        Turn {
            system: None,
            messages: vec![Message {
                role: Role::User,
                text: text.to_owned(),
            }],
            model: self.model.map(str::to_owned),
            reasoning_effort: None,
            service_tier: None,
            tools: ToolPolicy::None,
            session: SessionPolicy::Ephemeral,
            continuation: None,
            cleanup_group: None,
            check_sign_in: true,
        }
    }

    fn search(&self) -> Turn {
        Turn {
            tools: ToolPolicy::NativeWebSearch,
            ..self.ask("What is new in Rust?")
        }
    }

    fn persistent(&self, text: &str) -> Turn {
        Turn {
            session: SessionPolicy::Persistent,
            ..self.ask(text)
        }
    }

    /// The processes this provider started that have not exited and been
    /// reaped yet.
    fn still_running(&self) -> Vec<u32> {
        match &self.fixture {
            Fixture::Codex(fake) => fake.still_running(),
            Fixture::Claude(fake) => fake.still_running(),
            Fixture::Gemini(fake) => fake.still_running(),
            Fixture::Grok(fake) => fake.still_running(),
        }
    }

    /// What each launch of this provider read as its prompt.
    fn prompts(&self) -> Vec<String> {
        match &self.fixture {
            Fixture::Codex(fake) => fake.prompts(),
            Fixture::Claude(fake) => fake.prompts(),
            Fixture::Gemini(fake) => fake.prompts(),
            Fixture::Grok(fake) => fake.prompts(),
        }
    }

    /// The command lines of this provider's launches, in one string.
    fn command_lines(&self) -> String {
        match &self.fixture {
            Fixture::Codex(fake) => fake.invocations(),
            Fixture::Claude(fake) => fake.invocations(),
            Fixture::Gemini(fake) => fake.invocations(),
            Fixture::Grok(fake) => fake.invocations(),
        }
        .concat()
    }

    /// What a provider saved for a turn that is still there: its transcripts,
    /// and its per-turn workspaces.
    fn left_behind(&self) -> usize {
        match &self.fixture {
            Fixture::Codex(_) | Fixture::Claude(_) => 0,
            Fixture::Gemini(fake) => fake.kept(),
            Fixture::Grok(fake) => fake.turn_dirs().len(),
        }
    }

    /// Nothing this provider started is still running, and nothing it saved
    /// for a turn is left behind. Call it once the exchange is gone; a
    /// workspace is removed in the background, so it gets a moment.
    fn assert_nothing_left(&self) {
        assert_eq!(self.still_running(), Vec::<u32>::new(), "{:?}", self.kind);
        let give_up = Instant::now() + Duration::from_secs(5);
        while self.left_behind() > 0 {
            assert!(
                Instant::now() < give_up,
                "{:?} left {} behind",
                self.kind,
                self.left_behind()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Every update of a run, and the position of the first of each kind.
fn position(updates: &[Update], wanted: impl Fn(&Update) -> bool) -> Option<usize> {
    updates.iter().position(wanted)
}

#[test]
fn every_provider_reports_a_status_that_matches_what_it_can_do() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let provider = rig.provider.as_ref();
        let updates = run_to_end(provider.status().as_mut());

        assert_eq!(updates.len(), 2, "{kind:?}: {updates:?}");
        let Update::Status {
            provider_id,
            status,
        } = &updates[0]
        else {
            panic!("{kind:?}: expected a status first, got {updates:?}");
        };
        assert_eq!(provider_id, provider.id(), "{kind:?}");
        assert_eq!(status.availability, Availability::Available, "{kind:?}");
        assert_eq!(
            status.authentication,
            Authentication::Authenticated,
            "{kind:?}"
        );
        assert_eq!(status.capabilities, provider.capabilities(), "{kind:?}");
        assert_eq!(updates[1], Update::Completed, "{kind:?}");

        // Whatever else differs, every provider can be cancelled.
        assert_eq!(
            provider.capabilities().cancellation,
            Capability::Supported,
            "{kind:?}"
        );
        // The lifecycle limits are real limits.
        let timeouts = provider.timeouts();
        assert!(timeouts.start > Duration::ZERO, "{kind:?}");
        assert!(timeouts.idle > Duration::ZERO, "{kind:?}");
        assert!(timeouts.max_turn >= timeouts.idle, "{kind:?}");
    }
}

#[test]
fn a_provider_that_is_not_installed_is_not_found_everywhere() {
    for kind in ALL {
        let rig = Rig::without_executable(kind);

        let updates = run_to_end(rig.provider.status().as_mut());
        let Update::Status { status, .. } = &updates[0] else {
            panic!("{kind:?}: expected a status first, got {updates:?}");
        };
        assert_eq!(status.availability, Availability::NotFound, "{kind:?}");
        assert_eq!(updates.last(), Some(&Update::Completed), "{kind:?}");

        let updates = run_to_end(rig.provider.send(rig.ask("hi")).as_mut());
        assert_eq!(
            failure(&updates),
            (ErrorCode::ProviderNotFound, "EXECUTABLE_NOT_FOUND"),
            "{kind:?}"
        );
    }
}

#[test]
fn a_turn_starts_in_order_streams_and_ends_exactly_once() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let mut exchange = rig.provider.send(rig.ask("first"));
        let updates = run_to_end(exchange.as_mut());

        let launched = position(&updates, |update| matches!(update, Update::Launched))
            .unwrap_or_else(|| panic!("{kind:?}: no Launched in {updates:?}"));
        let started = position(&updates, |update| matches!(update, Update::Started))
            .unwrap_or_else(|| panic!("{kind:?}: no Started in {updates:?}"));
        let first_delta = position(&updates, |update| matches!(update, Update::Delta(_)))
            .unwrap_or_else(|| panic!("{kind:?}: no Delta in {updates:?}"));
        assert!(
            launched < started && started < first_delta,
            "{kind:?}: {updates:?}"
        );
        assert!(!answer_text(&updates).is_empty(), "{kind:?}");

        // Exactly one terminal update, the last; and nothing after it.
        let terminals = updates.iter().filter(|update| update.is_terminal()).count();
        assert_eq!(terminals, 1, "{kind:?}: {updates:?}");
        assert_eq!(updates.last(), Some(&Update::Completed), "{kind:?}");
        assert_eq!(
            exchange.next(Instant::now() + Duration::from_secs(1)),
            None,
            "{kind:?}: an exchange that ended has nothing more"
        );
        drop(exchange);
        rig.assert_nothing_left();
    }
}

#[test]
fn a_session_is_kept_exactly_where_the_provider_can_keep_one() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let provider = rig.provider.as_ref();

        // An ephemeral turn saves nothing anywhere, so it names nothing.
        let ephemeral = run_to_end(provider.send(rig.ask("no state")).as_mut());
        assert_eq!(session_of(&ephemeral), None, "{kind:?}: {ephemeral:?}");
        assert_eq!(ephemeral.last(), Some(&Update::Completed), "{kind:?}");

        let persistent = run_to_end(provider.send(rig.persistent("keep this")).as_mut());
        if !provider.supports_persistent_session() {
            assert_eq!(
                failure(&persistent),
                (ErrorCode::InvalidRequest, "PERSISTENT_SESSION_UNSUPPORTED"),
                "{kind:?}"
            );
            continue;
        }

        // The session is named before the turn starts, and a later turn that
        // is given it continues there.
        let session = session_of(&persistent)
            .unwrap_or_else(|| panic!("{kind:?}: no session in {persistent:?}"));
        let named = position(&persistent, |update| matches!(update, Update::Session(_))).unwrap();
        let started = position(&persistent, |update| matches!(update, Update::Started)).unwrap();
        assert!(named < started, "{kind:?}: {persistent:?}");
        assert_eq!(persistent.last(), Some(&Update::Completed), "{kind:?}");

        let follow_up = run_to_end(
            provider
                .send(Turn {
                    continuation: Some(session),
                    ..rig.persistent("and then?")
                })
                .as_mut(),
        );
        assert_eq!(
            visible(&follow_up).first(),
            Some(&Update::Started),
            "{kind:?}: {follow_up:?}"
        );
        assert_eq!(follow_up.last(), Some(&Update::Completed), "{kind:?}");
    }
}

#[test]
fn search_is_served_or_refused_as_the_capabilities_say() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let provider = rig.provider.as_ref();
        let updates = run_to_end(provider.send(rig.search()).as_mut());

        match provider.capabilities().web_search {
            Capability::Supported => {
                assert_eq!(updates.last(), Some(&Update::Completed), "{kind:?}");
                // A search that cites nothing is not a search: every one that
                // completes shows where its answer came from.
                assert!(
                    updates.iter().any(
                        |update| matches!(update, Update::Source(source) if !source.url.is_empty())
                    ),
                    "{kind:?}: {updates:?}"
                );
            }
            _ => {
                assert_eq!(updates.len(), 1, "{kind:?}: {updates:?}");
                assert_eq!(
                    failure(&updates),
                    (ErrorCode::InvalidRequest, "SEARCH_UNSUPPORTED"),
                    "{kind:?}"
                );
            }
        }
    }
}

#[test]
fn a_turn_the_provider_cannot_serve_is_refused_before_anything_runs() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let provider = rig.provider.as_ref();
        for (why, turn, refusal) in [
            (
                "no messages",
                Turn {
                    messages: Vec::new(),
                    ..rig.ask("hi")
                },
                "INVALID_TURN",
            ),
            (
                "a model that would be an option",
                Turn {
                    model: Some("--help".to_owned()),
                    ..rig.ask("hi")
                },
                "INVALID_TURN",
            ),
            (
                "a cleanup group that is a path",
                Turn {
                    cleanup_group: Some("../escape".to_owned()),
                    ..rig.ask("hi")
                },
                "INVALID_TURN",
            ),
            ("text with a NUL", rig.ask("one\0two"), "INVALID_TURN"),
        ] {
            let updates = run_to_end(provider.send(turn).as_mut());
            assert_eq!(updates.len(), 1, "{kind:?}, {why}: {updates:?}");
            assert_eq!(
                failure(&updates),
                (ErrorCode::InvalidRequest, refusal),
                "{kind:?}, {why}"
            );
        }
        rig.assert_nothing_left();
    }
}

#[test]
fn a_system_prompt_reaches_every_provider_ahead_of_the_question() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let updates = run_to_end(
            rig.provider
                .send(Turn {
                    system: Some("Answer in French. SYSTEM-MARKER".to_owned()),
                    ..rig.ask("What is muse?")
                })
                .as_mut(),
        );
        assert_eq!(updates.last(), Some(&Update::Completed), "{kind:?}");

        // Where the instructions go is each provider's own: real runs showed
        // that Antigravity follows its agent file and resists a message, while
        // Grok follows a message that claims precedence and ignores its agent
        // file. So Antigravity gets the question alone and the instructions in
        // its agent, Grok the instructions first in the prompt in its own
        // words, and Codex and Claude first in the prompt under the shared
        // introduction. None of them is on a command line, where anyone on the
        // machine could read it.
        let in_prompt =
            |intro: &str| format!("{intro}Answer in French. SYSTEM-MARKER\n\nWhat is muse?");
        match &rig.fixture {
            Fixture::Gemini(fake) => {
                assert_eq!(rig.prompts(), ["What is muse?"], "{kind:?}");
                assert!(
                    fake.read("agy-agents")
                        .contains("\nAnswer in French. SYSTEM-MARKER\n"),
                    "{kind:?}: the agent lacks the system prompt"
                );
            }
            Fixture::Grok(_) => assert_eq!(
                rig.prompts(),
                [in_prompt(seatline_providers::grok::SYSTEM_INTRO)],
                "{kind:?}"
            ),
            Fixture::Codex(_) | Fixture::Claude(_) => assert_eq!(
                rig.prompts(),
                [in_prompt(seatline_core::prompt::SYSTEM_INTRO)],
                "{kind:?}"
            ),
        }
        assert!(
            !rig.command_lines().contains("SYSTEM-MARKER"),
            "{kind:?}: the system prompt reached the command line"
        );
    }
}

#[test]
fn a_cancelled_turn_stops_promptly_and_leaves_nothing_running() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Hangs);
        let mut exchange = rig.provider.send(rig.ask("Never finish"));
        let updates = run_until_started(exchange.as_mut());
        assert_eq!(
            updates.last(),
            Some(&Update::Started),
            "{kind:?}: {updates:?}"
        );

        let cancelled = Instant::now();
        exchange.cancel(rig.provider.timeouts().stop_grace);
        let updates = run_to_end(exchange.as_mut());
        assert_eq!(
            updates.last(),
            Some(&Update::Stopped),
            "{kind:?}: {updates:?}"
        );
        let took = cancelled.elapsed();
        assert!(
            took < rig.provider.timeouts().stop_grace + Duration::from_secs(5),
            "{kind:?}: stopping took {took:?}"
        );
        drop(exchange);
        rig.assert_nothing_left();
    }
}

#[test]
fn a_turn_dropped_mid_run_stops_its_process() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Hangs);
        let mut exchange = rig.provider.send(rig.ask("Never finish"));
        run_until_started(exchange.as_mut());
        drop(exchange);
        // The process is killed with its exchange, and reaped promptly.
        let give_up = Instant::now() + Duration::from_secs(5);
        while !rig.still_running().is_empty() {
            assert!(
                Instant::now() < give_up,
                "{kind:?}: the process outlived its turn"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn a_caller_s_deadline_holds_however_quiet_the_provider_is() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Hangs);
        let mut exchange = rig.provider.send(rig.ask("Never finish"));
        run_until_started(exchange.as_mut());
        // What the provider wrote before it went quiet.
        while exchange
            .next(Instant::now() + Duration::from_millis(300))
            .is_some()
        {}

        // Nothing is due, so each call gives the caller back its thread at the
        // deadline it named, not later than that by more than the busy limit.
        for _ in 0..3 {
            let asked = Instant::now();
            let update = exchange.next(asked + Duration::from_millis(100));
            let took = asked.elapsed();
            assert_eq!(update, None, "{kind:?}");
            assert!(
                took < Duration::from_millis(100) + BUSY_LIMIT + Duration::from_millis(500),
                "{kind:?}: a 100 ms wait took {took:?}"
            );
        }
        exchange.cancel(Duration::from_millis(100));
        let updates = run_to_end(exchange.as_mut());
        assert_eq!(updates.last(), Some(&Update::Stopped), "{kind:?}");
    }
}

#[test]
fn asking_to_forget_what_was_never_kept_is_harmless() {
    for kind in ALL {
        let rig = Rig::new(kind, Behaviour::Answers);
        let provider = rig.provider.as_ref();

        for cleanup in [
            provider.cleanup_sessions(&[]),
            provider.cleanup_sessions(&["never-existed".to_owned()]),
            provider.cleanup_sessions(&["../../etc/passwd".to_owned()]),
            provider.cleanup_group("never-existed"),
            provider.cleanup_group("../escape"),
        ] {
            (cleanup.work)().unwrap_or_else(|error| panic!("{kind:?}: {error}"));
            (cleanup.completed)();
        }
    }
}
