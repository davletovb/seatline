//! The result of a run, as JSON that can be checked in and compared, and as
//! the markdown tables a methodology document quotes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::environment::Environment;
use crate::stats::Summary;
use crate::workload::Sample;

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub tool_version: String,
    /// `fake`, or `live:<provider>`.
    pub mode: String,
    pub label: String,
    pub environment: Environment,
    /// What the broker said it ran with: version, protocol and limits.
    pub broker: Option<Value>,
    /// What the providers were: the fake's behavior, or a live CLI's version.
    pub providers: Value,
    pub parameters: Parameters,
    pub scenarios: Vec<Scenario>,
    /// What these numbers cannot show. Read before quoting any of them.
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameters {
    /// Measured requests per application in each scenario.
    pub samples: usize,
    /// Requests per application made first and not counted.
    pub warmup: usize,
    /// Pause between a paced application's requests.
    pub gap_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    /// What was warm and what was fresh: the state the measurement is of.
    pub state: String,
    /// `measured`, or `unsupported` with a reason, never silently absent.
    pub status: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub apps: Vec<AppResult>,
    pub counts: Counts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppResult {
    pub app: String,
    /// What the application was doing: `single`, `short` or `long`.
    pub role: String,
    /// Measured requests that completed, and measured ones that did not.
    pub completed: usize,
    pub failed: usize,
    /// Microseconds, by metric. `client_*` metrics are what the application
    /// saw; `broker_*` metrics are the broker's own phases.
    pub metrics_us: BTreeMap<String, Summary>,
    /// Every measured request, so that the summaries can be recomputed.
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    /// Requests made, warm-up included.
    pub requests_total: u64,
    /// What the fake provider recorded over the whole scenario, warm-up
    /// included: sign-in probes and turns it was asked to run. Absent for a
    /// live provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fake_probes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fake_turns: Option<u64>,
    /// What the broker's telemetry counted for the measured requests.
    pub broker_probes: u64,
    pub broker_launches: u64,
    /// How many times applications started the companion, for a scenario that
    /// counts them.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub companion_starts: Option<u64>,
}

/// A summary line for a metric, in milliseconds with one decimal.
fn ms(us: u64) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}

const SHOWN: [(&str, &str); 10] = [
    ("client_prepare_us", "prepare"),
    ("client_submit_to_first_text_us", "submit→text"),
    ("client_start_to_first_text_us", "start→text"),
    ("client_submit_to_complete_us", "submit→done"),
    ("broker_queue_wait_us", "queue wait"),
    ("broker_sign_in_probe_us", "probe"),
    ("broker_provider_init_us", "init"),
    ("broker_first_text_us", "text wait"),
    ("broker_completion_us", "completion"),
    ("broker_cleanup_us", "cleanup"),
];

/// The report as markdown tables: one per scenario, p50 and p95 in
/// milliseconds for each metric the scenario measured.
pub fn markdown(report: &Report) -> String {
    let mut out = String::new();
    let env = &report.environment;
    let _ = writeln!(
        out,
        "{} — mode `{}`, {} {}, {} CPUs, {} builds, revision {}{}\n",
        report.label,
        report.mode,
        env.os,
        env.arch,
        env.cpus,
        builds(env),
        env.revision
            .as_deref()
            .map_or("unknown", |r| &r[..r.len().min(12)]),
        if env.dirty == Some(true) {
            " (uncommitted changes)"
        } else {
            ""
        },
    );
    let _ = writeln!(
        out,
        "{} measured requests per application after {} warm-up; p50 / p95 in milliseconds.\n",
        report.parameters.samples, report.parameters.warmup
    );
    for scenario in &report.scenarios {
        let _ = writeln!(out, "### `{}` — {}\n", scenario.name, scenario.state);
        let _ = writeln!(out, "{}\n", scenario.description);
        if scenario.status != "measured" {
            let _ = writeln!(
                out,
                "**{}**: {}\n",
                scenario.status,
                scenario.reason.as_deref().unwrap_or("")
            );
            continue;
        }
        let present: Vec<&(&str, &str)> = SHOWN
            .iter()
            .filter(|(key, _)| {
                scenario
                    .apps
                    .iter()
                    .any(|app| app.metrics_us.contains_key(*key))
            })
            .collect();
        let _ = write!(out, "| app | role | ok / failed |");
        for (_, title) in &present {
            let _ = write!(out, " {title} |");
        }
        let _ = writeln!(out);
        let _ = write!(out, "|---|---|---|");
        for _ in &present {
            let _ = write!(out, "---|");
        }
        let _ = writeln!(out);
        for app in &scenario.apps {
            let _ = write!(
                out,
                "| {} | {} | {} / {} |",
                app.app, app.role, app.completed, app.failed
            );
            for (key, _) in &present {
                match app.metrics_us.get(*key) {
                    Some(summary) => {
                        let _ = write!(out, " {} / {} |", ms(summary.p50), ms(summary.p95));
                    }
                    None => {
                        let _ = write!(out, " — |");
                    }
                }
            }
            let _ = writeln!(out);
        }
        let counts = &scenario.counts;
        let starts = counts.companion_starts.map_or_else(String::new, |starts| {
            format!("; applications started the companion {starts} times")
        });
        let _ = writeln!(
            out,
            "\nRequests {}; broker counted {} sign-in probes and {} provider launches for the measured ones{}{starts}.\n",
            counts.requests_total,
            counts.broker_probes,
            counts.broker_launches,
            match (counts.fake_probes, counts.fake_turns) {
                (Some(probes), Some(turns)) => format!(
                    "; the fake provider saw {probes} probe and {turns} turn processes in all"
                ),
                _ => String::new(),
            },
        );
    }
    let _ = writeln!(out, "### Limitations\n");
    for limitation in &report.limitations {
        let _ = writeln!(out, "- {limitation}");
    }
    out
}

/// Two reports of the same scenarios side by side: the change in p50 and p95 of
/// every metric both measured. A positive change is slower.
pub fn compare(before: &Report, after: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "before: {} (`{}`, {})  \nafter: {} (`{}`, {})\n",
        before.label,
        before
            .environment
            .revision
            .as_deref()
            .map_or("unknown", |r| &r[..r.len().min(12)]),
        builds(&before.environment),
        after.label,
        after
            .environment
            .revision
            .as_deref()
            .map_or("unknown", |r| &r[..r.len().min(12)]),
        builds(&after.environment),
    );
    if before.environment.os != after.environment.os
        || before.environment.arch != after.environment.arch
        || before.environment.cpus != after.environment.cpus
        || before.environment.harness_profile != after.environment.harness_profile
        // The broker is what is timed, so its build matters at least as much as
        // the harness's. A profile known on one side and not the other is a
        // difference; unknown on both is nothing to compare.
        || before.environment.companion_profile != after.environment.companion_profile
        || before.mode != after.mode
    {
        let _ = writeln!(
            out,
            "**These were measured on different platforms, builds or modes; the changes below are not a like-for-like comparison.**\n"
        );
    }
    let _ = writeln!(
        out,
        "| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|");
    for scenario in &before.scenarios {
        let Some(other) = after.scenarios.iter().find(|s| s.name == scenario.name) else {
            continue;
        };
        for app in &scenario.apps {
            let Some(other_app) = other.apps.iter().find(|a| a.app == app.app) else {
                continue;
            };
            for (key, title) in SHOWN {
                let (Some(a), Some(b)) = (app.metrics_us.get(key), other_app.metrics_us.get(key))
                else {
                    continue;
                };
                let delta = |x: u64, y: u64| {
                    let change = y as i64 - x as i64;
                    format!("{}{}", if change > 0 { "+" } else { "" }, ms_signed(change))
                };
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} → {} | {} | {} → {} | {} |",
                    scenario.name,
                    app.app,
                    title,
                    ms(a.p50),
                    ms(b.p50),
                    delta(a.p50, b.p50),
                    ms(a.p95),
                    ms(b.p95),
                    delta(a.p95, b.p95),
                );
            }
        }
    }
    out
}

/// How the harness and the broker were built, as a report names them.
fn builds(environment: &Environment) -> String {
    format!(
        "harness {}, companion {}",
        environment.harness_profile,
        environment
            .companion_profile
            .as_deref()
            .unwrap_or("unknown")
    )
}

fn ms_signed(us: i64) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::summarize;

    fn report(prepare: &[u64]) -> Report {
        let mut metrics = BTreeMap::new();
        metrics.insert("client_prepare_us".to_owned(), summarize(prepare).unwrap());
        Report {
            schema: SCHEMA,
            tool_version: "test".into(),
            mode: "fake".into(),
            label: "test".into(),
            environment: Environment {
                os: "linux".into(),
                arch: "x86_64".into(),
                cpus: 4,
                harness_profile: "release".into(),
                rustc: None,
                revision: Some("0123456789abcdef".into()),
                dirty: Some(false),
                companion_version: None,
                companion_profile: None,
            },
            broker: None,
            providers: Value::Null,
            parameters: Parameters {
                samples: prepare.len(),
                warmup: 0,
                gap_ms: 0,
            },
            scenarios: vec![Scenario {
                name: "cold-broker".into(),
                state: "cold_broker_fresh_provider".into(),
                status: "measured".into(),
                description: "d".into(),
                reason: None,
                apps: vec![AppResult {
                    app: "bench-a".into(),
                    role: "single".into(),
                    completed: prepare.len(),
                    failed: 0,
                    metrics_us: metrics,
                    samples: Vec::new(),
                }],
                counts: Counts::default(),
            }],
            limitations: vec!["only a test".into()],
        }
    }

    #[test]
    fn a_comparison_shows_the_signed_change_in_milliseconds() {
        let text = compare(&report(&[100_000, 110_000]), &report(&[10_000, 12_000]));
        assert!(
            text.contains("| cold-broker | bench-a | prepare | 100.0 → 10.0 | -90.0 |"),
            "{text}"
        );
        assert!(!text.contains("different platforms"));
    }

    #[test]
    fn a_comparison_across_companion_builds_warns_even_when_the_harness_matches() {
        // The broker is what is being timed: a debug broker against a release
        // one would read as a speedup that is only optimization.
        let mut debug_broker = report(&[1_000]);
        debug_broker.environment.companion_profile = Some("debug".into());
        let mut release_broker = report(&[1_000]);
        release_broker.environment.companion_profile = Some("release".into());
        assert_eq!(
            debug_broker.environment.harness_profile,
            release_broker.environment.harness_profile
        );
        let text = compare(&debug_broker, &release_broker);
        assert!(text.contains("not a like-for-like"), "{text}");
        assert!(
            text.contains("companion debug") && text.contains("companion release"),
            "{text}"
        );
        // A profile known on one side and not the other is a difference too; not
        // known on either side leaves nothing to compare.
        let unknown = report(&[1_000]);
        assert!(compare(&release_broker, &unknown).contains("not a like-for-like"));
        assert!(!compare(&unknown, &unknown).contains("not a like-for-like"));
        // The same profile on both sides is a like-for-like comparison.
        assert!(!compare(&release_broker, &release_broker).contains("not a like-for-like"));
    }

    #[test]
    fn a_comparison_across_builds_warns_that_it_is_not_like_for_like() {
        let mut other = report(&[1_000]);
        other.environment.harness_profile = "debug".into();
        assert!(compare(&report(&[1_000]), &other).contains("not a like-for-like"));
    }

    #[test]
    fn the_markdown_names_the_scenario_and_its_limits() {
        let text = markdown(&report(&[1_000, 2_000]));
        assert!(text.contains("### `cold-broker`"));
        assert!(text.contains("| bench-a | single | 2 / 0 |"));
        assert!(text.contains("only a test"));
        // Starts of the companion are only said for a scenario that counts them.
        assert!(!text.contains("started the companion"));
        let mut counted = report(&[1_000]);
        counted.scenarios[0].counts.companion_starts = Some(7);
        assert!(markdown(&counted).contains("applications started the companion 7 times"));
    }
}
