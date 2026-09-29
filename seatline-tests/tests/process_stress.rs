//! Repeated spawn and cancel (NAT-04). Every round ends with the process
//! reaped, and afterwards the test holds no more pipes or threads than it did
//! before. This file holds one test, so no other test shares its process.

use std::time::{Duration, Instant};

use seatline_core::process::{Event, Process, ProcessSpec};

const PROVIDER: &str = env!("CARGO_BIN_EXE_seatline-fake-provider");
const ROUNDS: usize = 120;
const GRACE: Duration = Duration::from_millis(50);
const DEADLINE: Duration = Duration::from_secs(10);

/// Pipes and threads the test process holds, where the platform can list them.
#[derive(Debug, PartialEq, Eq)]
struct Resources {
    descriptors: Option<usize>,
    threads: Option<usize>,
}

impl Resources {
    fn now() -> Self {
        let count = |directory: &str| std::fs::read_dir(directory).ok().map(Iterator::count);
        Self {
            descriptors: count("/dev/fd"),
            threads: count("/proc/self/task"),
        }
    }
}

#[test]
fn repeated_spawn_and_cancel_leaves_nothing_behind() {
    let before = Resources::now();

    // The moduli 4, 3, and 5 share no factor, so every 60 rounds try every
    // mode with every starting point and every way of stopping.
    for round in 0..ROUNDS {
        let mode = ["hang", "ignore-cancel", "slow", "normal"][round % 4];
        let mut process = Process::spawn(&ProcessSpec::new(PROVIDER).args(["--mode", mode]))
            .expect("spawn the fake provider");

        // Stop some processes before they are up and others once they have
        // written something.
        if round % 3 != 0 {
            let deadline = Instant::now() + DEADLINE;
            while let Some(event) = process.next_event(deadline) {
                if matches!(event, Event::Stdout(_) | Event::Exited(_)) {
                    break;
                }
            }
        }

        let pid = process.id();
        let exit = match round % 5 {
            0 | 1 => Some(process.terminate(GRACE)),
            2 | 3 => Some(process.kill()),
            _ => None,
        };
        if let Some(exit) = exit {
            assert!(exit.status.is_some(), "round {round} ({mode}): {exit:?}");
            assert!(exit.output_closed, "round {round} ({mode}): {exit:?}");
        }
        drop(process);
        assert_reaped(pid);
    }

    // Helper threads are joined as each process finishes; allow a moment for
    // the last of them to leave the thread list.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after = Resources::now();
    while after != before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        after = Resources::now();
    }
    assert_eq!(after, before);
}

/// A reaped process no longer exists; an unreaped one lingers as a zombie.
#[cfg(unix)]
fn assert_reaped(pid: u32) {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let pid = Pid::from_raw(i32::try_from(pid).expect("pid fits in pid_t"));
    assert_eq!(kill(pid, None), Err(Errno::ESRCH));
}

#[cfg(not(unix))]
fn assert_reaped(_pid: u32) {}
