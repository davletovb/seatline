//! Codex `--version` capability probing and `FileStamp` caching against a fake
//! `codex` binary.
//!
//! These tests copy the fake executable so each cache entry has an independent
//! `FileStamp` (and replace the binary in place to test upgrade invalidation).
//! On Linux, a concurrent spawn in another test thread can inherit a temporary
//! writable copy FD across `fork()` until `exec()` closes it, making the newly
//! copied binary fail to launch with `ETXTBSY` (`PROVIDER_UNAVAILABLE`).
//! Keeping these tests in their own integration test binary behind
//! `FIXTURE_LIFETIME` prevents concurrent spawns during executable copies,
//! mirroring `claude_isolation.rs` and `readiness.rs`.

mod support;

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use seatline_core::protocol::ErrorCode;
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::codex::{Codex, Limits};
use seatline_providers::{Provider, Update};
use support::{FIXTURES, FakeCodex, TEST_LIMITS, answer_text, failure, run_to_end};

static FIXTURE_LIFETIME: Mutex<()> = Mutex::new(());

fn fixture_lifetime() -> MutexGuard<'static, ()> {
    FIXTURE_LIFETIME
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn tool_free(text: &str) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: None,
        reasoning_effort: None,
        service_tier: None,
        tools: ToolPolicy::None,
        session: SessionPolicy::Persistent,
        continuation: None,
        cleanup_group: None,
        check_sign_in: false,
    }
}

/// An adapter whose Codex home holds no configuration that could expose tools.
fn context_adapter(codex: &FakeCodex) -> Codex {
    context_adapter_with(codex, TEST_LIMITS)
}

fn context_adapter_with(codex: &FakeCodex, limits: Limits) -> Codex {
    let home = codex.dir.join("context-codex-home");
    std::fs::create_dir_all(&home).unwrap();
    codex.adapter_with(limits).with_environment([
        (OsString::from("CODEX_HOME"), home.into_os_string()),
        (
            OsString::from("PATH"),
            std::env::var_os("PATH").unwrap_or_default(),
        ),
    ])
}

/// The `codex exec` runs the fake saw, one line each.
fn execs(codex: &FakeCodex) -> Vec<String> {
    codex
        .invocations()
        .into_iter()
        .filter(|line| line.starts_with("exec "))
        .collect()
}

/// Replaces the fake `codex` binary in `codex.dir` with an independent copy
/// that carries a distinct `FileStamp` on every OS, even while an earlier
/// process is still executing the previous binary.
fn replace_codex_executable(codex: &FakeCodex, generation: u64) {
    let name = FakeCodex::file_name();
    let executable = codex.dir.join(name);
    let staged = codex.dir.join(format!("{name}.staged-{generation}"));
    std::fs::copy(FIXTURES.provider(), &staged).unwrap();
    let mtime = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(60 * (generation + 1)))
        .unwrap_or(std::time::UNIX_EPOCH);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&staged)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    if executable.exists() {
        let backup = codex.dir.join(format!("{name}.old-{generation}"));
        std::fs::rename(&executable, &backup).unwrap();
    }
    std::fs::rename(&staged, &executable).unwrap();
}

fn version_checks(codex: &FakeCodex) -> usize {
    codex
        .invocations()
        .iter()
        .filter(|line| line.starts_with("--version\t"))
        .count()
}

#[test]
fn a_codex_from_before_0_145_omits_agents_enabled_from_version_check_and_caches_it() {
    let _fixture = fixture_lifetime();
    let codex = FakeCodex::install(FIXTURES, "reject-agents-enabled-0135", "signed-in");
    replace_codex_executable(&codex, 1);
    codex.set_version("reject-agents-enabled-0135", "signed-in", "0.135.0");

    let adapter = context_adapter(&codex);

    // First restricted turn checks `--version` once, sees pre-0.145, and runs
    // `codex exec` directly without `-c agents.enabled=false` (zero failed
    // exec launches), keeping the legacy multi_agent feature gates.
    let first = run_to_end(adapter.send(tool_free("first")).as_mut());
    assert_eq!(first.last(), Some(&Update::Completed), "{first:?}");
    assert_eq!(answer_text(&first), "You asked: first");
    assert_eq!(
        first
            .iter()
            .filter(|update| **update == Update::Launched)
            .count(),
        1,
        "{first:?}"
    );
    assert_eq!(version_checks(&codex), 1);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(!runs[0].contains("agents.enabled"), "{runs:?}");
    assert!(runs[0].contains("--ignore-rules"), "{runs:?}");
    assert!(
        runs[0].contains("-c features.multi_agent=false"),
        "{runs:?}"
    );
    assert!(
        runs[0].contains("-c features.multi_agent_v2=false"),
        "{runs:?}"
    );

    // Second turn reuses the cached version decision for this FileStamp.
    let second = run_to_end(adapter.send(tool_free("second")).as_mut());
    assert_eq!(second.last(), Some(&Update::Completed), "{second:?}");
    assert_eq!(answer_text(&second), "You asked: second");
    assert_eq!(version_checks(&codex), 1);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(!runs[1].contains("agents.enabled"), "{runs:?}");
    assert!(runs[1].contains("--ignore-rules"), "{runs:?}");

    // Invalidating readiness forgets the cached version decision.
    adapter.invalidate_readiness();
    let third = run_to_end(adapter.send(tool_free("third")).as_mut());
    assert_eq!(third.last(), Some(&Update::Completed), "{third:?}");
    assert_eq!(version_checks(&codex), 2);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert!(!runs[2].contains("agents.enabled"), "{runs:?}");
    assert!(runs[2].contains("--ignore-rules"), "{runs:?}");

    // Upgrading the binary invalidates the FileStamp entry and re-enables
    // `-c agents.enabled=false`.
    codex.set_version("answers", "signed-in", "0.171.0");
    replace_codex_executable(&codex, 2);
    let fourth = run_to_end(adapter.send(tool_free("fourth")).as_mut());
    assert_eq!(fourth.last(), Some(&Update::Completed), "{fourth:?}");
    assert_eq!(version_checks(&codex), 3);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert!(runs[3].contains("-c agents.enabled=false"), "{runs:?}");
    assert!(runs[3].contains("--ignore-rules"), "{runs:?}");

    // A pre-0.122 Codex omits both `-c agents.enabled=false` and `--ignore-rules`.
    codex.set_version("answers", "signed-in", "0.121.0");
    replace_codex_executable(&codex, 3);
    let legacy = run_to_end(adapter.send(tool_free("legacy")).as_mut());
    assert_eq!(legacy.last(), Some(&Update::Completed), "{legacy:?}");
    assert_eq!(version_checks(&codex), 4);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 5, "{runs:?}");
    assert!(!runs[4].contains("agents.enabled"), "{runs:?}");
    assert!(!runs[4].contains("--ignore-rules"), "{runs:?}");

    // ProviderDefault turns never run `--version`, never pass `agents.enabled`,
    // and never pass `--ignore-rules`.
    adapter.invalidate_readiness();
    let default_turn = run_to_end(
        adapter
            .send(Turn {
                tools: ToolPolicy::ProviderDefault,
                ..tool_free("default")
            })
            .as_mut(),
    );
    assert_eq!(
        default_turn.last(),
        Some(&Update::Completed),
        "{default_turn:?}"
    );
    assert_eq!(version_checks(&codex), 4);
    assert!(!execs(&codex)[5].contains("agents.enabled"));
    assert!(!execs(&codex)[5].contains("--ignore-rules"));
}

#[test]
fn a_modern_codex_with_a_malformed_user_agent_role_never_drops_agents_enabled() {
    let _fixture = fixture_lifetime();
    let codex = FakeCodex::install(FIXTURES, "malformed-user-agent-role", "signed-in");
    replace_codex_executable(&codex, 1);
    codex.set_version("malformed-user-agent-role", "signed-in", "0.171.0");

    let adapter = context_adapter(&codex);

    // On a modern Codex (>= 0.145), a malformed user `[agents]` role fails
    // with the exact same `expected struct AgentRoleToml in agents` diagnostic;
    // the adapter must fail normally and never retry without `agents.enabled=false`.
    let failed = run_to_end(adapter.send(tool_free("broken")).as_mut());
    assert_eq!(
        failure(&failed),
        (ErrorCode::ProviderFailed, "PROCESS_EXITED")
    );
    let runs = execs(&codex);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(runs[0].contains("-c agents.enabled=false"), "{runs:?}");

    // Once the user fixes config.toml, the next restricted turn still passes
    // `-c agents.enabled=false` (and reuses the cached version result).
    codex.set_version("answers", "signed-in", "0.171.0");
    let recovered = run_to_end(adapter.send(tool_free("fixed")).as_mut());
    assert_eq!(recovered.last(), Some(&Update::Completed), "{recovered:?}");
    assert_eq!(version_checks(&codex), 1);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs[1].contains("-c agents.enabled=false"), "{runs:?}");
}

#[test]
fn a_mid_probe_codex_upgrade_never_caches_the_old_version_for_the_new_binary() {
    let _fixture = fixture_lifetime();
    let codex = FakeCodex::install(FIXTURES, "reject-agents-enabled-0135", "signed-in");
    replace_codex_executable(&codex, 1);
    codex.set_version("reject-agents-enabled-0135", "signed-in", "slow-0.135.0");

    let adapter = context_adapter(&codex);

    let mut exchange = adapter.send(tool_free("upgrade"));
    let give_up = Instant::now() + Duration::from_secs(5);
    while version_checks(&codex) == 0 {
        assert!(
            Instant::now() < give_up,
            "fake codex --version never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Upgrade the binary while the pre-0.145 `--version` probe is still running.
    codex.set_version("answers", "signed-in", "0.171.0");
    replace_codex_executable(&codex, 2);

    let first = run_to_end(exchange.as_mut());
    assert_eq!(first.last(), Some(&Update::Completed), "{first:?}");
    assert_eq!(answer_text(&first), "You asked: upgrade");
    // The adapter discarded the stale 0.135.0 result, re-probed the new binary,
    // and launched `exec` with `-c agents.enabled=false`.
    assert_eq!(version_checks(&codex), 2);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(runs[0].contains("-c agents.enabled=false"), "{runs:?}");

    // The new binary's `true` result was cached; a follow-up needs no probe.
    let second = run_to_end(adapter.send(tool_free("after")).as_mut());
    assert_eq!(second.last(), Some(&Update::Completed), "{second:?}");
    assert_eq!(version_checks(&codex), 2);
    let runs = execs(&codex);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs[1].contains("-c agents.enabled=false"), "{runs:?}");
}

#[test]
fn failed_unparseable_or_timed_out_codex_version_fails_closed_without_caching() {
    let _fixture = fixture_lifetime();
    let short_probe = Limits {
        probe: Duration::from_millis(150),
        ..TEST_LIMITS
    };
    for (behavior, limits) in [
        ("fails", TEST_LIMITS),
        ("garbled", TEST_LIMITS),
        ("hangs", short_probe),
    ] {
        let codex = FakeCodex::install(FIXTURES, "answers", "signed-in");
        replace_codex_executable(&codex, 1);
        codex.set_version("answers", "signed-in", behavior);

        let adapter = context_adapter_with(&codex, limits);

        // Unknown `--version` result fails closed (passes `agents.enabled=false`,
        // omits `--ignore-rules` so older CLIs never choke on an unknown flag).
        let first = run_to_end(adapter.send(tool_free("first")).as_mut());
        assert_eq!(
            first.last(),
            Some(&Update::Completed),
            "{behavior}: {first:?}"
        );
        assert_eq!(version_checks(&codex), 1, "{behavior}");
        assert!(
            execs(&codex)[0].contains("-c agents.enabled=false"),
            "{behavior}: {:?}",
            execs(&codex)
        );
        assert!(
            !execs(&codex)[0].contains("--ignore-rules"),
            "{behavior}: {:?}",
            execs(&codex)
        );

        // Because the inconclusive probe was not cached, a subsequent turn
        // re-probes the same executable.
        codex.set_version("reject-agents-enabled-0135", "signed-in", "0.135.0");
        let second = run_to_end(adapter.send(tool_free("second")).as_mut());
        assert_eq!(
            second.last(),
            Some(&Update::Completed),
            "{behavior}: {second:?}"
        );
        assert_eq!(version_checks(&codex), 2, "{behavior}");
        assert!(
            !execs(&codex)[1].contains("agents.enabled"),
            "{behavior}: {:?}",
            execs(&codex)
        );
        assert!(
            execs(&codex)[1].contains("--ignore-rules"),
            "{behavior}: {:?}",
            execs(&codex)
        );
    }
}
