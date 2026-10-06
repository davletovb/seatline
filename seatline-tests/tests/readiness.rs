//! Readiness and preparation through the public API and real fake CLIs.
mod support;

use seatline_core::protocol::{Authentication, Availability, ProviderState};
use seatline_core::readiness::{Freshness, Source};
use seatline_core::turn::{Message, Role, SessionPolicy, SignInClassification, ToolPolicy, Turn};
use seatline_providers::{Provider, Update, readiness::Ready};
use std::ffi::OsString;
use std::time::Duration;
use support::{FIXTURES, FakeClaude, FakeCodex, FakeGemini, FakeGrok, run_to_end};

const CACHED: Freshness = Freshness::Cached { max_age_ms: 30_000 };

// These tests copy the fake executable to give each cache a stable identity.
// A concurrent spawn can inherit another test's temporary writable copy fd
// until exec closes it, making that test's CLI fail with ETXTBSY. Serialize
// fixture lifetimes; concurrent readiness subscribers are exercised within
// the tests themselves.
static FIXTURE_LIFETIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fixture_lifetime() -> std::sync::MutexGuard<'static, ()> {
    FIXTURE_LIFETIME
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn state(updates: &[Update]) -> &ProviderState {
    updates
        .iter()
        .find_map(|update| {
            if let Update::Status { status, .. } = update {
                Some(status)
            } else {
                None
            }
        })
        .unwrap()
}

// Fixtures normally hard-link the same binary. Installing/removing another
// test's link changes its ctime, correctly invalidating executable evidence.
// These cache tests need independent executable identities.
fn independent_executable(dir: &std::path::Path, name: &str) {
    let executable = dir.join(name);
    std::fs::remove_file(&executable).unwrap();
    std::fs::copy(FIXTURES.provider(), executable).unwrap();
}

#[test]
fn claude_mixed_state_rewrites_preserve_cache_but_account_changes_invalidate_it() {
    let _fixture = fixture_lifetime();
    // Exercise both the default home file and the relocated profile file.
    for relocated in [false, true] {
        let fake = FakeClaude::install(FIXTURES, "answers", "signed-in");
        independent_executable(&fake.dir, FakeClaude::file_name());
        let home = fake.dir.join("isolated-home");
        let config = home.join(if relocated { "profile" } else { ".claude" });
        std::fs::create_dir_all(&config).unwrap();
        let state_file = if relocated {
            config.join(".claude.json")
        } else {
            home.join(".claude.json")
        };
        let mut mixed = serde_json::json!({
            "oauthAccount": {"accountUuid":"account-one","organizationUuid":"org-one"},
            "numStartups": 1,
            "projects": {"history": "x".repeat(2 * 1024 * 1024)},
        });
        let write = |value: &serde_json::Value| {
            // An atomic replacement changes inode/timestamps as well as the
            // volatile state, like the real CLI's global config writer.
            let temporary = state_file.with_extension("new");
            std::fs::write(&temporary, serde_json::to_vec(value).unwrap()).unwrap();
            if state_file.exists() {
                std::fs::remove_file(&state_file).unwrap();
            }
            std::fs::rename(temporary, &state_file).unwrap();
        };
        write(&mixed);
        let mut host = vec![(
            OsString::from(if cfg!(windows) { "USERPROFILE" } else { "HOME" }),
            home.into_os_string(),
        )];
        if relocated {
            host.push(("CLAUDE_CONFIG_DIR".into(), config.clone().into_os_string()));
        }
        let ready = Ready::new(fake.adapter_with_env(host));
        let check = || {
            let mut exchange = ready.prepare(CACHED);
            let updates = run_to_end(exchange.as_mut());
            assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
            state(&updates).readiness.unwrap().source
        };
        assert_eq!(check(), Source::Fresh);
        mixed["numStartups"] = serde_json::json!(2);
        mixed["lastSessionId"] = serde_json::json!("session-two");
        write(&mixed);
        std::fs::write(config.join("history.jsonl"), "unrelated session history").unwrap();
        std::fs::create_dir(config.join("debug")).unwrap();
        assert_eq!(check(), Source::Cached);
        assert_eq!(fake.invocations().len(), 1);
        mixed["oauthAccount"]["accountUuid"] = serde_json::json!("account-two");
        write(&mixed);
        assert_eq!(check(), Source::Fresh);
        assert_eq!(fake.invocations().len(), 2);
        std::fs::write(config.join(".credentials.json"), "new credentials").unwrap();
        assert_eq!(check(), Source::Fresh);
        assert_eq!(fake.invocations().len(), 3);
        mixed["primaryApiKey"] = serde_json::json!("fake-key");
        write(&mixed);
        assert_eq!(check(), Source::Fresh);
        std::fs::write(&state_file, "malformed json").unwrap();
        assert_eq!(check(), Source::Fresh);
        assert_eq!(check(), Source::Fresh);
        assert_eq!(fake.invocations().len(), 6);
        std::fs::write(&state_file, vec![b' '; 16 * 1024 * 1024 + 1]).unwrap();
        assert_eq!(check(), Source::Fresh);
        assert_eq!(check(), Source::Fresh);
        assert_eq!(fake.invocations().len(), 8);
    }
}

#[test]
fn codex_session_files_do_not_invalidate_readiness_but_profile_config_changes_do() {
    let _fixture = fixture_lifetime();
    let fake = FakeCodex::install(FIXTURES, "answers", "subscription");
    independent_executable(&fake.dir, FakeCodex::file_name());
    let home = fake.dir.join("isolated-home");
    std::fs::create_dir_all(&home).unwrap();
    let ready =
        Ready::new(fake.adapter_with_env([("CODEX_HOME".into(), home.clone().into_os_string())]));
    let check = || {
        let mut exchange = ready.prepare(CACHED);
        let updates = run_to_end(exchange.as_mut());
        assert_eq!(updates.last(), Some(&Update::Completed), "{updates:?}");
        assert_eq!(
            state(&updates).availability,
            Availability::Available,
            "{updates:?}; invocations: {:?}",
            fake.invocations()
        );
        assert_eq!(
            state(&updates).authentication,
            Authentication::Authenticated,
            "{updates:?}; invocations: {:?}",
            fake.invocations()
        );
        state(&updates).readiness.unwrap().source
    };
    assert_eq!(check(), Source::Fresh);
    std::fs::write(home.join("history.jsonl"), "session history").unwrap();
    std::fs::create_dir(home.join("sessions")).unwrap();
    assert_eq!(check(), Source::Cached);
    std::fs::write(home.join("work.config.toml"), "model = 'other'").unwrap();
    assert_eq!(check(), Source::Fresh);
    std::fs::write(home.join("auth.json"), "new credentials").unwrap();
    assert_eq!(check(), Source::Fresh);
    assert_eq!(fake.invocations().len(), 3);
}

#[test]
fn all_providers_prepare_without_generating_then_reuse_verified_readiness() {
    let _fixture = fixture_lifetime();
    let codex = FakeCodex::install(FIXTURES, "answers", "subscription");
    let claude = FakeClaude::install(FIXTURES, "answers", "signed-in");
    let gemini = FakeGemini::install(FIXTURES);
    let grok = FakeGrok::install(FIXTURES);
    independent_executable(&codex.dir, FakeCodex::file_name());
    independent_executable(&claude.dir, FakeClaude::file_name());
    independent_executable(&gemini.dir, if cfg!(windows) { "agy.exe" } else { "agy" });
    independent_executable(&grok.dir, if cfg!(windows) { "grok.exe" } else { "grok" });
    let codex_home = codex.dir.join("isolated-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let providers = [
        Ready::new(
            codex.adapter_with_env([(OsString::from("CODEX_HOME"), codex_home.into_os_string())]),
        ),
        Ready::new(claude.adapter()),
        Ready::new(gemini.adapter()),
        Ready::new(grok.adapter()),
    ];
    for provider in &providers {
        assert!(provider.supports_preparation());
        let mut first = provider.prepare(CACHED);
        let mut peer = provider.prepare(CACHED);
        let first = run_to_end(first.as_mut());
        assert_eq!(
            state(&first).authentication,
            Authentication::Authenticated,
            "{}",
            provider.id()
        );
        assert_eq!(state(&first).readiness.unwrap().source, Source::Fresh);
        let peer = run_to_end(peer.as_mut());
        assert_eq!(
            state(&peer).readiness.unwrap().source,
            Source::Shared,
            "{}",
            provider.id()
        );
        let mut cached = provider.prepare(CACHED);
        assert_eq!(
            state(&run_to_end(cached.as_mut()))
                .readiness
                .unwrap()
                .source,
            Source::Cached,
            "{}",
            provider.id()
        );
        assert!(
            !first
                .iter()
                .any(|u| matches!(u, Update::Launched | Update::Delta(_) | Update::Session(_)))
        );
    }
    assert_eq!(codex.invocations().len(), 1);
    assert_eq!(claude.invocations().len(), 1);
    for (fake, name) in [(&gemini.dir, "agy"), (&grok.dir, "grok")] {
        assert_eq!(
            std::fs::read_to_string(fake.join(format!("{name}-probes")))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(!fake.join(format!("{name}-prompts")).exists());
    }

    for provider in &providers {
        let model = match provider.id() {
            "gemini" => Some("gemini-test"),
            "grok" => Some("grok-4.6"),
            _ => None,
        };
        let turn = |check_sign_in| Turn {
            system: None,
            messages: vec![Message {
                role: Role::User,
                text: "hello".into(),
            }],
            model: model.map(str::to_owned),
            reasoning_effort: None,
            service_tier: None,
            tools: ToolPolicy::None,
            session: SessionPolicy::Ephemeral,
            continuation: None,
            cleanup_group: None,
            check_sign_in,
        };
        let mut cached_send = provider.send_with_readiness(turn(false), CACHED);
        let updates = run_to_end(cached_send.as_mut());
        assert_eq!(
            updates.last(),
            Some(&Update::Completed),
            "{}: {updates:?}",
            provider.id()
        );
        assert_eq!(state(&updates).readiness.unwrap().source, Source::Cached);
        let status_at = updates
            .iter()
            .position(|u| matches!(u, Update::Status { .. }))
            .unwrap();
        let launched_at = updates.iter().position(|u| *u == Update::Launched).unwrap();
        assert!(status_at < launched_at);
        assert!(cached_send.probe_span().is_none());
        let mut fresh_send = provider.send_with_readiness(turn(true), CACHED);
        let updates = run_to_end(fresh_send.as_mut());
        assert_eq!(
            updates.last(),
            Some(&Update::Completed),
            "{}: {updates:?}",
            provider.id()
        );
        assert_eq!(state(&updates).readiness.unwrap().source, Source::Fresh);
        assert!(fresh_send.probe_span().is_some());
    }
    assert_eq!(
        codex
            .invocations()
            .iter()
            .filter(|line| line.starts_with("login "))
            .count(),
        2
    );
    assert_eq!(
        claude
            .invocations()
            .iter()
            .filter(|line| line.starts_with("auth "))
            .count(),
        2
    );
    for (fake, name) in [(&gemini.dir, "agy"), (&grok.dir, "grok")] {
        assert_eq!(
            std::fs::read_to_string(fake.join(format!("{name}-probes")))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}

#[test]
fn provider_conformance_covers_signed_out_unknown_unavailable_and_timeout() {
    let _fixture = fixture_lifetime();
    for scenario in ["signed-out", "broken", "hangs"] {
        let codex = FakeCodex::install(FIXTURES, "answers", scenario);
        let claude = FakeClaude::install(FIXTURES, "answers", scenario);
        let gemini = FakeGemini::install(FIXTURES);
        let grok = FakeGrok::install(FIXTURES);
        std::fs::write(gemini.dir.join("agy-readiness"), scenario).unwrap();
        std::fs::write(grok.dir.join("grok-readiness"), scenario).unwrap();
        // Classification must not depend on a cold CLI starting in 100 ms on
        // a loaded CI runner. Keep the short bound for the injected hang only.
        let timeout = if scenario == "hangs" {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(2)
        };
        let providers: [Box<dyn Provider>; 4] = [
            Box::new(codex.adapter_with(seatline_providers::codex::Limits {
                probe: timeout,
                ..support::TEST_LIMITS
            })),
            Box::new(claude.adapter_with(seatline_providers::claude::Limits {
                probe: timeout,
                ..support::CLAUDE_TEST_LIMITS
            })),
            Box::new(gemini.adapter().with_probe_timeout(timeout)),
            Box::new(grok.adapter().with_probe_timeout(timeout)),
        ];
        for provider in providers {
            let ready = Ready::boxed(provider);
            let mut check = ready.prepare(CACHED);
            let updates = run_to_end(check.as_mut());
            let status = state(&updates);
            assert_eq!(
                status.authentication,
                if scenario == "signed-out" {
                    Authentication::Unauthenticated
                } else {
                    Authentication::Unknown
                },
                "{} {scenario}: {updates:?}",
                ready.id()
            );
            assert_eq!(status.sign_in, None);
            if ready.id() == "gemini" || ready.id() == "grok" {
                assert_eq!(
                    status.availability,
                    if scenario == "signed-out" {
                        Availability::Available
                    } else {
                        Availability::Unavailable
                    }
                );
            }
            let mut again = ready.prepare(CACHED);
            assert_eq!(
                state(&run_to_end(again.as_mut())).readiness.unwrap().source,
                Source::Fresh
            );
        }
    }
}

#[test]
fn codex_classification_uses_only_known_status_phrases_and_never_exposes_output() {
    let _fixture = fixture_lifetime();
    for (scenario, expected) in [
        ("signed-in", SignInClassification::ApiKey),
        ("subscription", SignInClassification::Subscription),
        ("unknown-account-text", SignInClassification::Unknown),
    ] {
        let fake = FakeCodex::install(FIXTURES, "answers", scenario);
        let mut check = fake.adapter().status();
        let updates = run_to_end(check.as_mut());
        assert_eq!(state(&updates).sign_in, Some(expected));
        let encoded = serde_json::to_string(state(&updates)).unwrap();
        for private in [
            "sk-fake",
            "sk-live",
            "account api key",
            "someone@example.com",
        ] {
            assert!(!encoded.contains(private));
        }
    }
}

#[test]
fn credential_changes_and_executable_removal_recover_without_stale_readiness() {
    let _fixture = fixture_lifetime();
    let grok = FakeGrok::install(FIXTURES);
    let ready = Ready::new(grok.adapter());
    run_to_end(ready.prepare(CACHED).as_mut());
    std::fs::remove_file(grok.home.join(".grok/auth.json")).unwrap();
    let status = run_to_end(ready.prepare(CACHED).as_mut());
    assert_eq!(
        state(&status).authentication,
        Authentication::Unauthenticated
    );
    assert_eq!(state(&status).readiness.unwrap().source, Source::Fresh);
    std::fs::write(grok.home.join(".grok/auth.json"), "new-account").unwrap();
    assert_eq!(
        state(&run_to_end(ready.prepare(CACHED).as_mut())).authentication,
        Authentication::Authenticated
    );

    let codex = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let ready = Ready::new(codex.adapter());
    run_to_end(ready.prepare(CACHED).as_mut());
    let executable = codex.dir.join(FakeCodex::file_name());
    let moved = executable.with_extension("gone");
    std::fs::rename(&executable, &moved).unwrap();
    assert_eq!(
        state(&run_to_end(ready.prepare(CACHED).as_mut())).availability,
        Availability::NotFound
    );
    std::fs::rename(&moved, &executable).unwrap();
    assert_eq!(
        state(&run_to_end(ready.prepare(CACHED).as_mut())).availability,
        Availability::Available
    );
}

#[test]
fn claude_classification_uses_documented_auth_method_without_account_data() {
    let _fixture = fixture_lifetime();
    for (scenario, expected) in [
        ("subscription", SignInClassification::Subscription),
        ("api-key", SignInClassification::ApiKey),
        ("signed-in", SignInClassification::Unknown),
    ] {
        let fake = FakeClaude::install(FIXTURES, "answers", scenario);
        let updates = run_to_end(fake.adapter().status().as_mut());
        assert_eq!(state(&updates).sign_in, Some(expected));
        assert!(
            !serde_json::to_string(state(&updates))
                .unwrap()
                .contains("SECRET")
        );
    }
}

#[test]
fn all_providers_distinguish_missing_executables_from_an_unavailable_workspace() {
    let _fixture = fixture_lifetime();
    use seatline_core::discovery::SearchPath;
    use seatline_core::turn::Namespace;
    use seatline_providers::{claude::Claude, codex::Codex, gemini::Gemini, grok::Grok};
    let fake = FakeCodex::install(FIXTURES, "answers", "signed-in");
    let namespace = Namespace::fixed("readiness-tests").unwrap();
    let missing: [Box<dyn Provider>; 4] = [
        Box::new(Codex::new(
            SearchPath::new([]),
            fake.dir.join("missing-codex"),
        )),
        Box::new(Claude::new(
            SearchPath::new([]),
            fake.dir.join("missing-claude"),
        )),
        Box::new(Gemini::new(
            &namespace,
            SearchPath::new([]),
            fake.dir.join("missing-gemini"),
        )),
        Box::new(Grok::new(
            &namespace,
            SearchPath::new([]),
            fake.dir.join("missing-grok"),
        )),
    ];
    for provider in missing {
        let updates = run_to_end(provider.status().as_mut());
        assert_eq!(state(&updates).availability, Availability::NotFound);
        assert_eq!(state(&updates).authentication, Authentication::Unknown);
    }
    let claude = FakeClaude::install(FIXTURES, "answers", "signed-in");
    let gemini = FakeGemini::install(FIXTURES);
    let grok = FakeGrok::install(FIXTURES);
    for work in [
        fake.dir.join("work"),
        claude.dir.join("claude-work"),
        gemini.dir.join("workspace"),
        grok.dir.join("workspace"),
    ] {
        std::fs::write(work, "workspace is a file").unwrap();
    }
    let unavailable: [Box<dyn Provider>; 4] = [
        Box::new(fake.adapter()),
        Box::new(claude.adapter()),
        Box::new(gemini.adapter()),
        Box::new(grok.adapter()),
    ];
    for provider in unavailable {
        let updates = run_to_end(provider.status().as_mut());
        assert_eq!(
            state(&updates).availability,
            Availability::Unavailable,
            "{}",
            provider.id()
        );
        assert_eq!(state(&updates).authentication, Authentication::Unknown);
    }
}
