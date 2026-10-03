//! Summary statistics. Percentiles use the nearest-rank method, which always
//! returns a value that was measured: with fewer than 20 samples the 95th
//! percentile is the maximum, and the report says so.

use serde::{Deserialize, Serialize};

/// What a list of measurements looks like, in the unit they were taken in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub n: usize,
    pub min: u64,
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
    pub mean: u64,
}

/// The nearest-rank percentile of `sorted`, which must be sorted ascending and
/// not empty: the smallest value that at least `percent` percent of the
/// measurements do not exceed.
pub fn percentile(sorted: &[u64], percent: usize) -> u64 {
    assert!(!sorted.is_empty(), "a percentile needs a measurement");
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Summarizes `values`, or `None` when there are none.
pub fn summarize(values: &[u64]) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let sum: u128 = sorted.iter().map(|value| u128::from(*value)).sum();
    Some(Summary {
        n: sorted.len(),
        min: sorted[0],
        p50: percentile(&sorted, 50),
        p95: percentile(&sorted, 95),
        max: sorted[sorted.len() - 1],
        mean: u64::try_from(sum / sorted.len() as u128).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_returns_a_measured_value() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&values, 100), 100);
        // 4 samples: the median is the 2nd, the 95th percentile the 4th.
        assert_eq!(percentile(&[10, 20, 30, 40], 50), 20);
        assert_eq!(percentile(&[10, 20, 30, 40], 95), 40);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 95), 7);
    }

    #[test]
    fn a_summary_does_not_depend_on_input_order() {
        let shuffled = [30, 10, 50, 20, 40];
        let summary = summarize(&shuffled).unwrap();
        assert_eq!(
            summary,
            Summary {
                n: 5,
                min: 10,
                p50: 30,
                p95: 50,
                max: 50,
                mean: 30
            }
        );
        assert_eq!(summarize(&[]), None);
    }
}
