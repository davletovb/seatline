//! I-10's JSONL schema and statistics. The historical Python rows remain readable.
//! These use interpolated percentiles and the conventional median, unlike the
//! main scenario report's integer nearest-rank statistics.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Row {
    pub variant: String,
    pub round: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    pub ready_ms: Option<f64>,
    pub answer_begun_ms: Option<f64>,
    pub first_text_ms: Option<f64>,
    pub result_ms: Option<f64>,
    pub usage: Option<Value>,
    pub answer: String,
    pub error: Option<String>,
    #[serde(default)]
    pub stderr: String,
}

impl Row {
    pub fn complete(&self) -> bool {
        self.error.is_none()
            && self.usage.as_ref().is_some_and(Value::is_object)
            && self.first_text_ms.is_some()
            && self.result_ms.is_some()
    }
    fn token(&self, name: &str) -> f64 {
        self.usage
            .as_ref()
            .and_then(|u| u[name].as_f64())
            .unwrap_or(0.0)
    }
}

pub fn variants(rows: &[Row]) -> Vec<String> {
    let mut variants = Vec::new();
    for row in rows {
        if !variants.contains(&row.variant) {
            variants.push(row.variant.clone());
        }
    }
    variants
}

pub fn read(path: &str) -> io::Result<Vec<Row>> {
    let text = std::fs::read_to_string(path)?;
    let mut rows = Vec::new();
    let mut keys = BTreeSet::new();
    for (line, text) in text
        .lines()
        .enumerate()
        .filter(|(_, s)| !s.trim().is_empty())
    {
        let row: Row = serde_json::from_str(text)
            .map_err(|e| io::Error::other(format!("{path}: line {}: {e}", line + 1)))?;
        if !["base", "dynamic", "short", "padded"].contains(&row.variant.as_str())
            || !keys.insert((row.round, row.variant.clone()))
            || [
                row.ready_ms,
                row.answer_begun_ms,
                row.first_text_ms,
                row.result_ms,
            ]
            .into_iter()
            .flatten()
            .any(|n| !n.is_finite() || n < 0.0)
        {
            return Err(io::Error::other(format!(
                "{path}: line {}: invalid or duplicate row",
                line + 1
            )));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(io::Error::other("no saved rows"));
    }
    Ok(rows)
}

pub fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = (sorted.len() - 1) as f64 * fraction;
    let low = position.floor() as usize;
    let high = position.ceil() as usize;
    sorted[low] + (sorted[high] - sorted[low]) * (position - low as f64)
}
fn median(values: &[f64]) -> f64 {
    percentile(values, 0.5)
}

/// Binomial tail in log space: no integer overflow or 2^n overflow on long runs.
pub fn sign_test(differences: &[f64]) -> f64 {
    let slower = differences.iter().filter(|&&d| d > 0.0).count();
    let faster = differences.iter().filter(|&&d| d < 0.0).count();
    let n = slower + faster;
    if n == 0 {
        return 1.0;
    }
    let mut term = -(n as f64) * 2.0_f64.ln();
    let mut log_sum = term;
    for i in 1..=slower.min(faster) {
        term += ((n - i + 1) as f64).ln() - (i as f64).ln();
        let high = term.max(log_sum);
        log_sum = high + ((term - high).exp() + (log_sum - high).exp()).ln();
    }
    (2.0 * log_sum.exp()).min(1.0)
}
fn alike(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn report(rows: &[Row], variants: &[String], prompt_count: Option<usize>) -> String {
    let mut text = String::new();
    let mut by: BTreeMap<&str, BTreeMap<usize, &Row>> = BTreeMap::new();
    for variant in variants {
        by.insert(variant, BTreeMap::new());
    }
    for row in rows.iter().filter(|r| r.round > 0 && r.complete()) {
        by.entry(&row.variant).or_default().insert(row.round, row);
    }
    writeln!(
        text,
        "{:8} {:>3} | {:>6} {:>11} {:>10} | {:>15} {:>11} {:>6} | {:>9}",
        "variant",
        "n",
        "input",
        "cache write",
        "cache read",
        "first text (ms)",
        "p25-p75",
        "p95",
        "to result"
    )
    .unwrap();
    for variant in variants {
        let rs: Vec<_> = by[variant.as_str()].values().copied().collect();
        if rs.is_empty() {
            writeln!(text, "{variant:8}   0 | no completed run").unwrap();
            continue;
        }
        let total: Vec<_> = rs
            .iter()
            .map(|r| {
                r.token("input_tokens")
                    + r.token("cache_creation_input_tokens")
                    + r.token("cache_read_input_tokens")
            })
            .collect();
        let write: Vec<_> = rs
            .iter()
            .map(|r| r.token("cache_creation_input_tokens"))
            .collect();
        let read: Vec<_> = rs
            .iter()
            .map(|r| r.token("cache_read_input_tokens"))
            .collect();
        let first: Vec<_> = rs.iter().filter_map(|r| r.first_text_ms).collect();
        let result: Vec<_> = rs.iter().filter_map(|r| r.result_ms).collect();
        writeln!(
            text,
            "{variant:8} {:3} | {:6.0} {:11.0} {:10.0} | {:15.0} {:5.0}-{:<5.0} {:6.0} | {:9.0}",
            rs.len(),
            median(&total),
            median(&write),
            median(&read),
            median(&first),
            percentile(&first, 0.25),
            percentile(&first, 0.75),
            percentile(&first, 0.95),
            median(&result)
        )
        .unwrap();
    }
    writeln!(
        text,
        "\n{:8} | {:>46}\n{:8} | {:>9} {:>13} {:>11} {:>9}",
        "variant",
        "where the run was (median ms from launch)",
        "",
        "ready",
        "answer begun",
        "first text",
        "result"
    )
    .unwrap();
    for variant in variants {
        let rs: Vec<_> = by[variant.as_str()].values().copied().collect();
        if rs.is_empty() {
            continue;
        }
        let columns: Vec<_> = (0..4)
            .map(|i| {
                let seen: Vec<_> = rs
                    .iter()
                    .filter_map(|r| {
                        [r.ready_ms, r.answer_begun_ms, r.first_text_ms, r.result_ms][i]
                    })
                    .collect();
                if seen.is_empty() {
                    "-".to_owned()
                } else {
                    format!("{:.0}", median(&seen))
                }
            })
            .collect();
        writeln!(
            text,
            "{variant:8} | {:>9} {:>13} {:>11} {:>9}",
            columns[0], columns[1], columns[2], columns[3]
        )
        .unwrap();
    }
    text.push('\n');
    if let Some(base) = by.get("base") {
        for variant in variants.iter().filter(|v| v.as_str() != "base") {
            for (result, what) in [(false, "time to first text"), (true, "time to the result")] {
                let differences: Vec<_> = base
                    .iter()
                    .filter_map(|(n, b)| {
                        let r = by[variant.as_str()].get(n)?;
                        if result {
                            Some(r.result_ms? - b.result_ms?)
                        } else {
                            Some(r.first_text_ms? - b.first_text_ms?)
                        }
                    })
                    .collect();
                if !differences.is_empty() {
                    writeln!(text, "{variant} - base, {what}: median paired difference {:+.0} ms over {} rounds, slower in {}, sign test p = {:.2}",
                        median(&differences), differences.len(), differences.iter().filter(|&&d| d > 0.0).count(), sign_test(&differences)).unwrap();
                }
            }
        }
    }
    text.push('\n');
    let mut given: BTreeMap<(usize, String), usize> = BTreeMap::new();
    if let Some(count) = prompt_count {
        for row in rows
            .iter()
            .filter(|r| r.round > 0 && r.variant == "base" && !r.answer.is_empty() && r.complete())
        {
            *given
                .entry((row.round % count, alike(&row.answer)))
                .or_default() += 1;
        }
    }
    writeln!(
        text,
        "{:8} | {:>26} | {:>31} | thought first",
        "variant", "answers base did not give", "output tokens (median, largest)"
    )
    .unwrap();
    for variant in variants {
        let answered: Vec<_> = rows
            .iter()
            .filter(|r| {
                r.round > 0 && r.variant == *variant && !r.answer.is_empty() && r.complete()
            })
            .collect();
        if answered.is_empty() {
            continue;
        }
        let novel = prompt_count.map_or("-".to_owned(), |count| {
            let others = answered
                .iter()
                .filter(|r| {
                    given
                        .get(&(r.round % count, alike(&r.answer)))
                        .copied()
                        .unwrap_or(0)
                        <= usize::from(variant == "base")
                })
                .count();
            format!("{others} of {}", answered.len())
        });
        let out: Vec<_> = answered.iter().map(|r| r.token("output_tokens")).collect();
        let thought = answered
            .iter()
            .filter(|r| {
                r.usage.as_ref().is_some_and(|u| {
                    u["output_tokens_details"]["thinking_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                        > 0
                })
            })
            .count();
        writeln!(
            text,
            "{variant:8} | {novel:>26} | {:>22.0}, {:>7.0} | {thought} of {} runs",
            median(&out),
            out.iter().copied().reduce(f64::max).unwrap(),
            answered.len()
        )
        .unwrap();
    }
    text.push('\n');
    let placed: Vec<_> = rows
        .iter()
        .filter(|r| r.round > 0 && r.position.is_some())
        .collect();
    if !placed.is_empty() {
        writeln!(
            text,
            "{:8} | {:>36} | median first text (ms) at each place",
            "variant", "counted rounds taken 1st, 2nd, ..."
        )
        .unwrap();
        for variant in variants {
            let mut counts = Vec::new();
            let mut times = Vec::new();
            for place in 1..=variants.len() {
                counts.push(format!(
                    "{:>3}",
                    placed
                        .iter()
                        .filter(|r| r.variant == *variant && r.position == Some(place))
                        .count()
                ));
                let seen: Vec<_> = placed
                    .iter()
                    .filter(|r| r.variant == *variant && r.position == Some(place) && r.complete())
                    .filter_map(|r| r.first_text_ms)
                    .collect();
                times.push(if seen.is_empty() {
                    "    -".to_owned()
                } else {
                    format!("{:5.0}", median(&seen))
                });
            }
            writeln!(
                text,
                "{variant:8} | {:>36} | {}",
                counts.join(", "),
                times.join(", ")
            )
            .unwrap();
        }
    }
    let failures = rows.iter().filter(|r| r.round > 0 && !r.complete()).count();
    if failures > 0 {
        writeln!(text, "\n{failures} incomplete or failed measured runs excluded from timing and answer statistics.").unwrap();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interpolated_statistics_and_binomial_ties() {
        assert_eq!(percentile(&[10.0, 20.0, 30.0, 40.0], 0.5), 25.0);
        assert_eq!(percentile(&[10.0, 20.0, 30.0, 40.0], 0.95), 38.5);
        assert_eq!(sign_test(&[0.0; 5]), 1.0);
        assert!((sign_test(&[-1.0; 4]) - 0.125).abs() < 1e-12);
        assert!((sign_test(&[-1.0, 1.0, 1.0, 1.0]) - 0.625).abs() < 1e-12);
        assert!((sign_test(&[1.0; 2000]) - 0.0).abs() < 1e-12);
        let balanced: Vec<_> = (0..2000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        assert!((sign_test(&balanced) - 1.0).abs() < 1e-10);
    }
    #[test]
    fn pairs_join_by_round_exclude_warmup_and_failures_and_handle_missing_result() {
        let row = |variant: &str, round, first, result| Row {
            variant: variant.into(),
            round,
            first_text_ms: Some(first),
            result_ms: result,
            usage: Some(serde_json::json!({"input_tokens":10,"output_tokens":2})),
            answer: "ok".into(),
            ..Row::default()
        };
        let rows = [
            row("base", 0, 9999.0, Some(9999.0)),
            row("base", 1, 100.0, Some(120.0)),
            row("short", 2, 5.0, Some(8.0)),
            row("short", 1, 80.0, Some(110.0)),
            row("short", 3, 1.0, None),
        ];
        let report = report(&rows, &variants(&rows), Some(1));
        assert!(report.contains("difference -20 ms over 1 rounds"));
        assert!(report.contains("difference -10 ms over 1 rounds"));
        assert!(report.contains("1 incomplete or failed"));
        assert!(!report.contains("9999"));
    }
}
