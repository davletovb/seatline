//! What the test process holds while a hostile provider runs, where the
//! platform shows it (Linux): its threads and file descriptors must return to
//! where they were, and its peak memory must stay far below what the provider
//! wrote.

use std::time::Duration;

/// What the test process holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    pub threads: usize,
    pub file_descriptors: usize,
}

#[cfg(target_os = "linux")]
pub fn held() -> Option<Held> {
    let count = |dir: &str| std::fs::read_dir(dir).ok().map(Iterator::count);
    Some(Held {
        threads: count("/proc/self/task")?,
        file_descriptors: count("/proc/self/fd")?,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn held() -> Option<Held> {
    None
}

/// A field of `/proc/self/status`, in KiB.
fn status_kib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix(field)?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()
    })
}

/// Resets the peak resident memory to the current, and returns the current,
/// where the platform allows (Linux).
pub fn reset_peak_memory() -> Option<u64> {
    std::fs::write("/proc/self/clear_refs", "5").ok()?;
    status_kib("VmRSS:")
}

/// How much the peak resident memory grew since [`reset_peak_memory`] returned
/// `before`, in KiB.
pub fn peak_memory_growth(before: Option<u64>) -> Option<u64> {
    before
        .zip(status_kib("VmHWM:"))
        .map(|(before, peak)| peak.saturating_sub(before))
}

/// Waits a little for helper threads and pipes to go, then returns what the
/// process holds.
pub fn settle(baseline: Option<Held>) -> Option<Held> {
    for _ in 0..200 {
        let now = held();
        if now == baseline {
            return now;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    held()
}
