//! CLI parity and process supervision without a real account or provider quota.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use seatline_platform::private_fs;
use serde_json::Value;

const HARNESS: &str = env!("CARGO_BIN_EXE_seatline-bench");
const FAKE: &str = env!("CARGO_BIN_EXE_seatline-bench-fake-provider");

struct Fixture(PathBuf);
impl Fixture {
    fn new(behavior: &str) -> Self {
        let dir =
            private_fs::unique_child(Path::new(env!("CARGO_TARGET_TMPDIR")), "claude-bench-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            FAKE,
            dir.join(if cfg!(windows) {
                "claude.exe"
            } else {
                "claude"
            }),
        )
        .unwrap();
        std::fs::write(dir.join("claude-scenario"), format!("print={behavior}\n")).unwrap();
        Self(dir)
    }
    fn exe(&self) -> PathBuf {
        self.0.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        })
    }
    #[allow(clippy::disallowed_methods)] // Launches our benchmark and a stand-in CLI.
    fn run(&self, args: &[&str], live: bool) -> Output {
        let mut command = Command::new(HARNESS);
        command
            .args(args)
            .env("CLAUDE_BIN", self.exe())
            .env("XDG_CACHE_HOME", &self.0)
            .env("LOCALAPPDATA", &self.0)
            .env("OPENAI_API_KEY", "must-not-be-inherited")
            .env("NODE_OPTIONS", "must-not-be-inherited")
            .env_remove("SEATLINE_BENCH_LIVE");
        if live {
            command.env("SEATLINE_BENCH_LIVE", "1");
        }
        command.output().unwrap()
    }
    fn rows(&self) -> Vec<Value> {
        std::fs::read_to_string(self.0.join("raw.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn successful(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn historical_python_report_is_reproduced_without_a_provider_or_opt_in() {
    let fixture = Fixture::new("exits-after-0");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let input = root.join("docs/performance-baselines/2026-10-11-claude-system-prompt-warm.jsonl");
    let output = fixture.run(
        &["claude-prompts", "--from-raw", input.to_str().unwrap()],
        false,
    );
    assert_eq!(
        successful(&output),
        include_str!("fixtures/claude-system-prompt-warm-report.txt")
    );
    assert!(!fixture.0.join("claude-invocations").exists());
    for (name, extra) in [
        ("new-workspace", None),
        ("proofreader-text", None),
        ("search", Some("--search")),
        ("instructions", Some("--prompts")),
    ] {
        let input = root.join(format!(
            "docs/performance-baselines/2026-10-11-claude-system-prompt-{name}.jsonl"
        ));
        let prompts = root.join(
            "docs/performance-baselines/2026-10-11-claude-system-prompt-instructions-prompts.txt",
        );
        let mut args = vec!["claude-prompts", "--from-raw", input.to_str().unwrap()];
        if let Some(flag) = extra {
            args.push(flag);
            if flag == "--prompts" {
                args.push(prompts.to_str().unwrap());
            }
        }
        assert!(successful(&fixture.run(&args, false)).contains("sign test p"));
    }
}

#[test]
fn live_commands_and_invalid_arguments_never_launch_without_authorization() {
    let fixture = Fixture::new("exits-after-0");
    for args in [
        vec!["claude-prompts", "1"],
        vec!["claude-exit", "natural", "1"],
        vec!["claude-prompts", "0"],
        vec!["claude-exit", "natural", "0"],
        vec!["claude-exit", "natural", "1", "--env", "wrong"],
        vec!["claude-exit", "natural", "1", "--env"],
        vec!["claude-prompts", "1", "--variants", "base,base"],
    ] {
        let output = fixture.run(&args, false);
        assert!(!output.status.success());
    }
    assert!(!fixture.0.join("claude-invocations").exists());
}

#[test]
fn all_variants_record_balanced_positions_usage_answers_and_filtered_environment() {
    let fixture = Fixture::new("exits-after-0");
    let raw = fixture.0.join("raw.jsonl");
    let answers = fixture.0.join("answers.jsonl");
    let prompts = fixture.0.join("prompts.txt");
    std::fs::write(&prompts, "Return JSON only:\\n2 + 2\n").unwrap();
    let output = fixture.run(
        &[
            "claude-prompts",
            "4",
            "--variants",
            "base,dynamic,short,padded",
            "--fresh-cwd",
            "--seed",
            "8",
            "--prompts",
            prompts.to_str().unwrap(),
            "--raw",
            raw.to_str().unwrap(),
            "--answers",
            answers.to_str().unwrap(),
        ],
        true,
    );
    let report = successful(&output);
    assert!(report.contains("over 4 rounds"));
    let rows = fixture.rows();
    assert_eq!(rows.len(), 20);
    for variant in ["base", "dynamic", "short", "padded"] {
        let positions: BTreeSet<_> = rows
            .iter()
            .filter(|r| r["variant"] == variant && r["round"] != 0)
            .map(|r| r["position"].as_u64().unwrap())
            .collect();
        assert_eq!(positions, BTreeSet::from([1, 2, 3, 4]));
    }
    for row in &rows {
        assert_eq!(row["usage"]["input_tokens"], 11);
        assert!(row["error"].is_null());
        assert!(
            row["answer"]
                .as_str()
                .unwrap()
                .contains("Return JSON only:\n2 + 2")
        );
        assert!(row["first_text_ms"].as_f64().unwrap() <= row["result_ms"].as_f64().unwrap());
    }
    assert_eq!(
        std::fs::read_to_string(answers).unwrap().lines().count(),
        16
    );
    let invocations = std::fs::read_to_string(fixture.0.join("claude-invocations")).unwrap();
    assert_eq!(invocations.lines().count(), 20);
    assert!(invocations.contains("--exclude-dynamic-system-prompt-sections"));
    assert_eq!(invocations.matches("--system-prompt-file").count(), 10);
    let envs: Vec<Value> = std::fs::read_to_string(fixture.0.join("claude-environment"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let cwds: BTreeSet<_> = envs.iter().map(|e| e["cwd"].as_str().unwrap()).collect();
    assert_eq!(cwds.len(), 20);
    for env in &envs {
        assert!(env["env"]["OPENAI_API_KEY"].is_null());
        assert!(env["env"]["NODE_OPTIONS"].is_null());
        assert_eq!(env["env"]["DISABLE_AUTOUPDATER"], "1");
        assert!(
            !Path::new(env["cwd"].as_str().unwrap()).exists(),
            "scratch directories removed after run"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(raw).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn both_pipes_are_drained_and_stderr_evidence_is_bounded() {
    let fixture = Fixture::new("stderr-exits-after-0");
    let raw = fixture.0.join("raw.jsonl");
    successful(&fixture.run(
        &[
            "claude-prompts",
            "1",
            "--variants",
            "base",
            "--timeout",
            "2",
            "--raw",
            raw.to_str().unwrap(),
        ],
        true,
    ));
    for row in fixture.rows() {
        assert_eq!(row["stderr"].as_str().unwrap().len(), 500);
        assert!(row["error"].is_null());
    }
}

#[test]
fn timeouts_and_early_exits_are_saved_as_failures_and_return_nonzero() {
    for behavior in ["hangs", "flooding", "no-result", "fails-auth"] {
        let fixture = Fixture::new(behavior);
        let raw = fixture.0.join("raw.jsonl");
        let output = fixture.run(
            &[
                "claude-prompts",
                "1",
                "--variants",
                "base",
                "--timeout",
                "1",
                "--raw",
                raw.to_str().unwrap(),
            ],
            true,
        );
        assert!(!output.status.success(), "{behavior}");
        let rows = fixture.rows();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["error"].is_string()));
        assert!(String::from_utf8_lossy(&output.stdout).contains("no completed run"));
        if behavior == "hangs" || behavior == "flooding" {
            assert!(
                rows.iter()
                    .all(|r| r["error"].as_str().unwrap().contains("timed out"))
            );
        }
    }
}

#[test]
fn exit_measurement_waits_for_natural_exit_and_accepts_explicit_environment() {
    let fixture = Fixture::new("exits-after-100");
    let report = successful(&fixture.run(
        &[
            "claude-exit",
            "natural",
            "2",
            "--env",
            "BENCH_EXPLICIT=first",
            "--env",
            "BENCH_EXPLICIT=second",
        ],
        true,
    ));
    assert!(report.contains("natural: n=2"));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("claude-exits"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let envs = std::fs::read_to_string(fixture.0.join("claude-environment")).unwrap();
    assert!(envs.contains("second"));
    assert!(!envs.contains("first"));
}

#[cfg(unix)]
#[test]
fn term_signals_at_the_result_instead_of_waiting_for_natural_exit() {
    let fixture = Fixture::new("exits-after-10000");
    successful(&fixture.run(&["claude-exit", "term", "1", "--timeout", "2"], true));
    assert!(!fixture.0.join("claude-exits").exists());
}

#[test]
fn unusable_outputs_are_rejected_before_spending_quota() {
    let fixture = Fixture::new("exits-after-0");
    let output = fixture.0.join("absent/raw.jsonl");
    assert!(
        !fixture
            .run(
                &["claude-prompts", "1", "--raw", output.to_str().unwrap()],
                true
            )
            .status
            .success()
    );
    assert!(!fixture.0.join("claude-invocations").exists());
}

#[test]
fn search_uses_only_websearch_and_a_shared_workspace_and_parses_final_line_without_newline() {
    let fixture = Fixture::new("no-final-newline");
    let raw = fixture.0.join("raw.jsonl");
    successful(&fixture.run(
        &[
            "claude-prompts",
            "1",
            "--variants",
            "base",
            "--search",
            "--raw",
            raw.to_str().unwrap(),
        ],
        true,
    ));
    for row in fixture.rows() {
        assert!(row["result_ms"].is_number());
        assert!(row["usage"].is_object());
        assert!(row["error"].is_null());
    }
    let invocations = std::fs::read_to_string(fixture.0.join("claude-invocations")).unwrap();
    assert!(
        invocations
            .lines()
            .all(|l| l.contains("--tools WebSearch --allowedTools WebSearch"))
    );
    let envs = std::fs::read_to_string(fixture.0.join("claude-environment")).unwrap();
    let cwds: BTreeSet<String> = envs
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["cwd"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(cwds.len(), 1);
}

#[test]
fn offline_mode_rejects_duplicate_rows_and_bad_input_without_launching() {
    let fixture = Fixture::new("exits-after-0");
    let raw = fixture.0.join("raw.jsonl");
    let row = serde_json::json!({"variant":"base","round":1,"ready_ms":null,"answer_begun_ms":null,"first_text_ms":100,"result_ms":120,"usage":{},"answer":"ok","error":null});
    for input in [
        String::new(),
        "bad json".into(),
        format!("{row}\n{row}\n"),
        row.to_string().replace("\"base\"", "\"unknown\""),
    ] {
        std::fs::write(&raw, input).unwrap();
        assert!(
            !fixture
                .run(
                    &["claude-prompts", "--from-raw", raw.to_str().unwrap()],
                    false
                )
                .status
                .success()
        );
    }
    assert!(!fixture.0.join("claude-invocations").exists());
}

#[test]
fn an_ignored_stop_is_killed_and_evidence_is_preserved() {
    let fixture = Fixture::new("ignores-cancel");
    let raw = fixture.0.join("raw.jsonl");
    let output = fixture.run(
        &[
            "claude-prompts",
            "1",
            "--variants",
            "base",
            "--timeout",
            "1",
            "--raw",
            raw.to_str().unwrap(),
        ],
        true,
    );
    assert!(!output.status.success());
    assert_eq!(fixture.rows().len(), 2);
    #[cfg(target_os = "linux")]
    for pid in std::fs::read_to_string(fixture.0.join("claude-pids"))
        .unwrap()
        .lines()
    {
        assert!(
            !Path::new("/proc").join(pid).exists(),
            "timed-out provider is reaped"
        );
    }
}
