//! The harness end to end (B-02): real broker, real client, fake provider.
//!
//! These tests check what the harness reports, not how fast anything is: the
//! counts must agree with what the fake provider recorded, states must be
//! named, nothing unsupported may be dressed up as measured, and the evidence
//! must carry no path or credential. No test asserts a wall-clock threshold.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const HARNESS: &str = env!("CARGO_BIN_EXE_seatline-bench");
const FAKE: &str = env!("CARGO_BIN_EXE_seatline-bench-fake-provider");
const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");

/// The companion, built into the same directory as the harness. A plain
/// `cargo test --workspace` builds it for the companion's own tests; a run of
/// this package alone does not, so build it then.
#[allow(clippy::disallowed_methods)] // Builds the companion this test measures; never provider execution.
fn companion() -> PathBuf {
    let name = if cfg!(windows) {
        "seatline-companion.exe"
    } else {
        "seatline-companion"
    };
    let path = Path::new(HARNESS).with_file_name(name);
    if !path.is_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut build = Command::new(env!("CARGO"));
        build
            .current_dir(root)
            .args(["build", "--locked", "-p", "seatline-companion"]);
        if !cfg!(debug_assertions) {
            build.arg("--release");
        }
        assert!(
            build.status().unwrap().success(),
            "could not build the companion"
        );
    }
    path
}

#[allow(clippy::disallowed_methods)] // Runs the harness binary this package builds.
fn harness(args: &[&str]) -> Output {
    Command::new(HARNESS)
        .args(args)
        .args(["--fake-provider", FAKE, "--scratch", SCRATCH])
        .arg("--companion")
        .arg(companion())
        .env_remove("SEATLINE_BENCH_LIVE")
        .output()
        .unwrap()
}

fn report(args: &[&str]) -> (Value, String) {
    let file = Path::new(SCRATCH).join(format!("bench-{}.json", std::process::id()));
    let mut full = args.to_vec();
    let path = file.to_str().unwrap().to_owned();
    full.extend(["--output", &path]);
    let output = harness(&full);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(&file).unwrap();
    let _ = std::fs::remove_file(&file);
    (serde_json::from_str(&text).unwrap(), text)
}

fn scenario<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scenario| scenario["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"))
}

/// Metric summaries, by name, of an application's result.
fn metric<'a>(app: &'a Value, name: &str) -> &'a Value {
    &app["metrics_us"][name]
}

#[test]
fn warm_scenarios_count_processes_the_way_the_fake_provider_did() {
    let (report, text) = report(&[
        "run",
        "--scenario",
        "warm-send",
        "--scenario",
        "warm-send-adapter",
        "--scenario",
        "warm-send-probe",
        "--scenario",
        "warm-status",
        "--samples",
        "3",
        "--warmup",
        "1",
    ]);

    // Without a probe: one provider process per request, and no probes.
    let send = scenario(&report, "warm-send");
    assert_eq!(send["state"], "warm_broker_fresh_provider");
    assert_eq!(send["status"], "measured");
    assert_eq!(send["counts"]["requests_total"], 4);
    assert_eq!(send["counts"]["fake_turns"], 4, "the fake saw every turn");
    assert_eq!(send["counts"]["fake_probes"], 0);
    assert_eq!(send["counts"]["broker_launches"], 3, "the measured ones");
    assert_eq!(send["counts"]["broker_probes"], 0);
    let app = &send["apps"][0];
    assert_eq!(
        (app["completed"].as_u64(), app["failed"].as_u64()),
        (Some(3), Some(0))
    );
    for name in [
        "client_prepare_us",
        "client_submit_to_first_text_us",
        "client_submit_to_complete_us",
        "broker_queue_wait_us",
        "broker_provider_init_us",
        "broker_first_text_us",
        "broker_completion_us",
        "broker_cleanup_us",
        "broker_handshake_us",
    ] {
        assert_eq!(metric(app, name)["n"], 3, "{name} in {app}");
    }
    assert!(metric(app, "broker_sign_in_probe_us").is_null());

    // Through the shipped adapter: the same work, but connecting and the
    // handshake cannot be told apart from outside, so they are not reported.
    let adapter = scenario(&report, "warm-send-adapter");
    assert_eq!(adapter["counts"]["fake_turns"], 4);
    assert_eq!(adapter["counts"]["broker_launches"], 3, "joined by order");
    let app = &adapter["apps"][0];
    assert_eq!(app["completed"], 3);
    assert!(metric(app, "client_prepare_us").is_null());
    assert_eq!(metric(app, "client_submit_to_first_text_us")["n"], 3);
    assert_eq!(metric(app, "broker_provider_init_us")["n"], 3);

    // With one: the probe is a second process, and the broker says so.
    let probe = scenario(&report, "warm-send-probe");
    assert_eq!(probe["counts"]["fake_probes"], 4);
    assert_eq!(probe["counts"]["fake_turns"], 4);
    assert_eq!(probe["counts"]["broker_probes"], 3);
    assert_eq!(metric(&probe["apps"][0], "broker_sign_in_probe_us")["n"], 3);

    // A status check runs the probe and no turn, and has no text to wait for.
    let status = scenario(&report, "warm-status");
    assert_eq!(status["counts"]["fake_turns"], 0);
    assert_eq!(status["counts"]["fake_probes"], 4);
    assert!(metric(&status["apps"][0], "client_submit_to_first_text_us").is_null());

    // The broker said what it ran with.
    assert_eq!(report["broker"]["limits"]["max_provider_running"], 2);
    assert_eq!(report["mode"], "fake");

    // The evidence names no path, and nothing that could be a credential.
    for private in [SCRATCH, env!("CARGO_MANIFEST_DIR"), "token"] {
        assert!(!text.contains(private), "{private} is in the report");
    }
}

#[test]
fn resumed_context_is_a_state_of_its_own_and_reused_process_is_not_faked() {
    let (report, _) = report(&[
        "run",
        "--scenario",
        "resumed-context",
        "--scenario",
        "reused-process",
        "--samples",
        "2",
        "--warmup",
        "1",
    ]);
    let resumed = scenario(&report, "resumed-context");
    assert_eq!(resumed["state"], "resumed_context");
    // One request to start the conversation, then the warm-up and the samples.
    assert_eq!(resumed["counts"]["requests_total"], 4);
    assert_eq!(resumed["counts"]["fake_turns"], 4);
    assert_eq!(resumed["apps"][0]["completed"], 2);

    let reused = scenario(&report, "reused-process");
    assert_eq!(reused["status"], "unsupported");
    assert!(reused["reason"].as_str().unwrap().contains("E-02"));
    assert!(reused["apps"].as_array().unwrap().is_empty());
}

#[test]
fn a_cold_start_goes_through_the_clients_own_broker_start() {
    let (report, _) = report(&[
        "run",
        "--scenario",
        "cold-broker",
        "--samples",
        "1",
        "--warmup",
        "0",
    ]);
    let cold = scenario(&report, "cold-broker");
    assert_eq!(cold["state"], "cold_broker_fresh_provider");
    let app = &cold["apps"][0];
    assert_eq!(app["completed"], 1);
    // The client had to start the broker, so connecting is where the time went:
    // the connect is reported apart from the handshake that follows it.
    let sample = &app["samples"][0];
    assert!(sample["connect_us"].as_u64().unwrap() > 0);
    assert!(sample["handshake_us"].as_u64().unwrap() > 0);
    assert!(sample["prepare_us"].as_u64().unwrap() >= sample["connect_us"].as_u64().unwrap());
    assert!(
        sample["broker"]["handshake_us"].is_u64(),
        "the broker's record joined"
    );
}

#[test]
fn three_applications_compete_for_the_providers_slots() {
    let (report, _) = report(&[
        "run",
        "--scenario",
        "three-app-short",
        "--scenario",
        "short-contended",
        "--samples",
        "3",
        "--warmup",
        "1",
        "--gap-ms",
        "5",
    ]);
    let together = scenario(&report, "three-app-short");
    let apps: Vec<&str> = together["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| app["app"].as_str().unwrap())
        .collect();
    assert_eq!(apps, ["bench-a", "bench-b", "bench-c"]);
    assert!(
        together["apps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|app| app["completed"] == 3)
    );

    let contended = scenario(&report, "short-contended");
    let roles: Vec<&str> = contended["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| app["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["short", "long", "long"]);
    for app in contended["apps"].as_array().unwrap() {
        assert_eq!(app["failed"], 0, "{app}");
    }
}

#[test]
fn a_live_run_needs_a_second_confirmation_and_starts_nothing_without_it() {
    let output = harness(&["run", "--live", "codex"]);
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("SEATLINE_BENCH_LIVE=1"), "{message}");
}

#[test]
fn unknown_scenarios_and_options_are_refused() {
    for args in [
        &["run", "--scenario", "nonsense"][..],
        &["run", "--samples", "zero"][..],
        &["run", "--samples", "0"][..],
        &["run", "--bogus"][..],
        &["run", "--live", "nonsense"][..],
    ] {
        assert!(!harness(args).status.success(), "{args:?}");
    }
}

#[test]
fn a_saved_report_can_be_rendered_and_compared() {
    let (_, text) = report(&[
        "run",
        "--scenario",
        "warm-status",
        "--samples",
        "2",
        "--warmup",
        "0",
    ]);
    let file = Path::new(SCRATCH).join(format!("bench-compare-{}.json", std::process::id()));
    std::fs::write(&file, text).unwrap();
    let path = file.to_str().unwrap();
    #[allow(clippy::disallowed_methods)] // Runs the harness binary this package builds.
    let run = |args: &[&str]| Command::new(HARNESS).args(args).output().unwrap();
    let rendered = run(&["report", path]);
    assert!(rendered.status.success());
    assert!(String::from_utf8_lossy(&rendered.stdout).contains("### `warm-status`"));
    let compared = run(&["compare", path, path]);
    assert!(compared.status.success());
    let text = String::from_utf8_lossy(&compared.stdout);
    assert!(text.contains("| warm-status | bench-a |"), "{text}");
    assert!(!text.contains("not a like-for-like"));
    let _ = std::fs::remove_file(file);
}

#[test]
#[allow(clippy::disallowed_methods)] // Runs the harness binary this package builds.
fn the_scheduler_overhead_measurement_runs() {
    let output = Command::new(HARNESS)
        .args([
            "overhead",
            "--turns",
            "5",
            "--updates",
            "20",
            "--rounds",
            "2",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["updates_per_turn"], 20);
    assert!(report["ns_per_update"]["timing_off"]["n"].as_u64().unwrap() >= 2);
}
