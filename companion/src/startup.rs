//! Finding the broker, or starting one, when several clients ask at once (D-02).
//!
//! A client that finds no broker used to start the companion itself, so every
//! client that arrived together started one: all but one found the broker lock
//! taken and exited, after the process had been made. Now exactly one client
//! at a time holds the **start claim**, a lock on `start.lock` in the data
//! directory, and only it starts the companion. The others keep trying to
//! connect, and one of them takes the claim over if its holder goes away
//! without a broker listening:
//!
//! - **Stale claims.** The claim is a file lock the operating system releases
//!   when its holder exits, however it exits, so no claim outlives its client
//!   and no file has to be cleaned up.
//! - **Failed starts.** A companion that exits without listening frees its
//!   client to start one more, and a client starts at most
//!   [`Startup::max_starts`]; one that cannot be started at all (the file is
//!   missing) is reported at once.
//! - **A holder that never finishes.** A client that has waited
//!   [`Startup::takeover_after`] without a broker, and has started none, starts
//!   one without the claim. The broker's own lock still keeps one serving, so
//!   this costs at most a process that exits at once, once per waiting client.
//! - **Timeouts.** Every attempt ends within [`Startup::budget`], and so does
//!   every try to reach the broker inside it: a try that never finishes is cut
//!   off at [`Startup::attempt_limit`] (or the budget, if sooner) and the loop
//!   goes on, so a hung connection cannot keep a client from starting a broker,
//!   and one that finishes after the budget is not accepted.
//! - **Upgrades.** The claim names no version: whichever client holds it starts
//!   the companion the installation registers now. A broker that was already
//!   running keeps serving until it is idle and leaves, or until `install` or
//!   `seatline-companion stop` ends it ([`crate::control`]).
//!
//! The client looks for the broker every millisecond at first, backing off to
//! a hundred: a companion listens about two milliseconds after it is started,
//! so the wait for it is now that, and not the length of a backoff step.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio::time::Instant;

use crate::config;

/// How a client finds the broker, or has one started.
#[derive(Debug, Clone)]
pub struct Startup {
    /// The companion to start. `None` is `SEATLINE_COMPANION_BIN`, the
    /// installed companion, or the one beside this program, in that order.
    pub executable: Option<PathBuf>,
    /// How long finding or starting the broker may take in all.
    pub budget: Duration,
    /// How long a client waits on another's start before starting one itself.
    pub takeover_after: Duration,
    /// How many times one client starts the companion.
    pub max_starts: usize,
    /// The longest one try to reach the broker may take before it is given up
    /// on and the next step (a start, another try) goes ahead. Every try is
    /// also cut off at the budget.
    pub attempt_limit: Duration,
    /// Variables the companion is started with, in addition to the data
    /// directory: for a test that wants the broker it starts to leave soon.
    pub environment: Vec<(OsString, OsString)>,
}

impl Default for Startup {
    fn default() -> Self {
        Self {
            executable: None,
            budget: Duration::from_secs(5),
            takeover_after: Duration::from_secs(2),
            max_starts: 2,
            attempt_limit: Duration::from_secs(1),
            environment: Vec::new(),
        }
    }
}

/// The first step of the polling backoff, and its ceiling.
const FIRST_DELAY: Duration = Duration::from_millis(1);
const LAST_DELAY: Duration = Duration::from_millis(100);

/// The name of the claim's lock file.
pub const CLAIM_FILE: &str = "start.lock";

/// The right to start the companion, held until dropped.
pub struct Claim(#[allow(dead_code)] std::fs::File);

/// Tries to take the start claim: `None` when another client holds it.
pub fn claim(root: &Path) -> io::Result<Option<Claim>> {
    match config::lock(root, CLAIM_FILE) {
        Ok(file) => Ok(Some(Claim(file))),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// Starts the companion, detached from this program's stdio. It is told the
/// data directory this client uses, so the broker it starts is the one the
/// client then looks for.
#[allow(clippy::disallowed_methods)] // Starts only the locally configured companion, never a request-supplied executable.
pub fn start_companion(root: &Path, startup: &Startup) -> io::Result<Child> {
    let executable = match &startup.executable {
        Some(executable) => executable.clone(),
        None => std::env::var_os("SEATLINE_COMPANION_BIN")
            .map(PathBuf::from)
            .or(crate::install::executable(root)?)
            .unwrap_or(std::env::current_exe()?.with_file_name(if cfg!(windows) {
                "seatline-companion.exe"
            } else {
                "seatline-companion"
            })),
    };
    Command::new(executable)
        .arg("serve")
        .env("SEATLINE_DATA_DIR", root)
        .envs(
            startup
                .environment
                .iter()
                .map(|(name, value)| (name, value)),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Whether a companion this client started has exited. One that has is
/// reaped by asking, so it does not linger as a zombie.
pub fn exited(child: &mut Child) -> bool {
    child.try_wait().map_or(true, |status| status.is_some())
}

/// One try to reach the broker, cut off at `limit` or the deadline, whichever
/// is sooner, so that a try that hangs neither outlives the budget nor keeps
/// the loop from its other steps, and a connection that arrives late is not
/// accepted.
async fn try_once<T, Fut>(
    connect: &mut impl FnMut() -> Fut,
    deadline: Instant,
    limit: Duration,
) -> io::Result<T>
where
    Fut: Future<Output = io::Result<T>>,
{
    let allowed = deadline
        .saturating_duration_since(Instant::now())
        .min(limit);
    tokio::time::timeout(allowed, connect())
        .await
        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

/// Connects, starting the companion if need be and nobody else is.
///
/// `connect` is one attempt to reach the broker, `claim` the attempt to take
/// the start claim, `start` starts a companion and `exited` says whether one
/// it started has gone. They are arguments so that the coordination can be
/// tested without processes and without waiting.
pub async fn start_or_wait<T, Held, Started, Fut>(
    startup: &Startup,
    mut connect: impl FnMut() -> Fut,
    mut claim: impl FnMut() -> io::Result<Option<Held>>,
    mut start: impl FnMut() -> io::Result<Started>,
    mut exited: impl FnMut(&mut Started) -> bool,
) -> io::Result<T>
where
    Fut: Future<Output = io::Result<T>>,
{
    let began = Instant::now();
    let deadline = began + startup.budget;
    let mut delay = FIRST_DELAY;
    // The claim is kept until this returns, which is once the broker is
    // reachable (or we give up), so nobody else starts one meanwhile.
    let mut held: Option<Held> = None;
    let mut started: Option<Started> = None;
    let mut starts = 0;
    loop {
        if let Ok(value) = try_once(&mut connect, deadline, startup.attempt_limit).await {
            return Ok(value);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::other("Seatline companion did not start"));
        }
        // A companion this client started that has gone without listening
        // frees it to start another, if it may.
        if started.as_mut().is_some_and(&mut exited) {
            started = None;
            held = None;
        }
        if started.is_none() && starts < startup.max_starts {
            let mut may_start = held.is_some();
            if held.is_none() {
                if let Some(claim) = claim()? {
                    held = Some(claim);
                    // The client that held it before may have just finished.
                    if let Ok(value) = try_once(&mut connect, deadline, startup.attempt_limit).await
                    {
                        return Ok(value);
                    }
                    may_start = true;
                }
            }
            // Someone else holds the claim. Normally that client is starting
            // the broker; if it has not by now, this one does.
            if may_start || now - began >= startup.takeover_after {
                starts += 1;
                started = Some(start()?);
                delay = FIRST_DELAY;
            }
        }
        tokio::time::sleep(delay.min(deadline - now)).await;
        delay = (delay * 2).min(LAST_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;

    /// A broker that comes up `after` its start, shared by the clients of a
    /// test, and the claim they contend for.
    #[derive(Default)]
    struct World {
        /// How long after it is started the broker listens.
        listens_after: Duration,
        /// When a start happened, which is when the broker listens from.
        listening_from: Cell<Option<Instant>>,
        starts: Cell<usize>,
        claimed: Cell<bool>,
        /// Whether a started companion exits at once, without listening.
        start_dies: Cell<bool>,
        start_fails: Cell<bool>,
        connects: RefCell<Vec<Duration>>,
    }

    struct Guard(Rc<World>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.claimed.set(false);
        }
    }

    impl World {
        fn up(&self) -> bool {
            self.listening_from
                .get()
                .is_some_and(|from| Instant::now() >= from + self.listens_after)
        }
    }

    async fn client(world: Rc<World>, startup: Startup, begun: Instant) -> io::Result<()> {
        start_or_wait(
            &startup,
            || {
                world.connects.borrow_mut().push(begun.elapsed());
                std::future::ready(if world.up() {
                    Ok(())
                } else {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                })
            },
            || {
                Ok(if world.claimed.replace(true) {
                    None
                } else {
                    Some(Guard(world.clone()))
                })
            },
            || {
                world.starts.set(world.starts.get() + 1);
                if world.start_fails.get() {
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                }
                if !world.start_dies.get() {
                    world.listening_from.set(Some(Instant::now()));
                }
                Ok(world.start_dies.get())
            },
            |dead| *dead,
        )
        .await
    }

    fn world(listens_after_ms: u64) -> Rc<World> {
        Rc::new(World {
            listens_after: Duration::from_millis(listens_after_ms),
            ..World::default()
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_broker_is_connected_without_a_start_or_a_sleep() {
        let world = world(0);
        world.listening_from.set(Some(Instant::now()));
        let begun = Instant::now();
        client(world.clone(), Startup::default(), begun)
            .await
            .unwrap();
        assert_eq!(world.starts.get(), 0);
        assert_eq!(begun.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_client_that_holds_the_claim_starts_once_and_connects_when_the_broker_listens() {
        let world = world(2);
        let begun = Instant::now();
        client(world.clone(), Startup::default(), begun)
            .await
            .unwrap();
        assert_eq!(world.starts.get(), 1);
        // The broker listens 2 ms after it is started, and the client finds
        // it at the next 1 ms step, not at the next 10 ms one.
        assert!(
            begun.elapsed() <= Duration::from_millis(4),
            "{:?}",
            begun.elapsed()
        );
        assert!(!world.claimed.get(), "the claim is released once connected");
    }

    #[tokio::test(start_paused = true)]
    async fn clients_that_start_together_start_the_companion_once() {
        let world = world(2);
        let begun = Instant::now();
        let startup = Startup::default();
        let clients: Vec<_> = (0..16)
            .map(|_| client(world.clone(), startup.clone(), begun))
            .collect();
        for result in futures_join(clients).await {
            result.unwrap();
        }
        assert_eq!(world.starts.get(), 1, "every client but one starts nothing");
        assert!(begun.elapsed() <= Duration::from_millis(6));
    }

    /// Runs the futures to completion together, as clients of one program do.
    async fn futures_join<F: Future<Output = io::Result<()>> + 'static>(
        futures: Vec<F>,
    ) -> Vec<io::Result<()>> {
        let local = tokio::task::LocalSet::new();
        let handles: Vec<_> = futures
            .into_iter()
            .map(|future| local.spawn_local(future))
            .collect();
        local
            .run_until(async {
                let mut results = Vec::new();
                for handle in handles {
                    results.push(handle.await.unwrap());
                }
                results
            })
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_does_not_hold_the_claim_never_starts_while_its_holder_does() {
        let world = world(5);
        // Someone else holds the claim and has started the broker.
        world.claimed.set(true);
        world.listening_from.set(Some(Instant::now()));
        client(world.clone(), Startup::default(), Instant::now())
            .await
            .unwrap();
        assert_eq!(world.starts.get(), 0);
        assert!(world.claimed.get(), "it was not this client's to release");
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_whose_holder_goes_away_is_taken_over_by_a_waiting_client() {
        let world = world(1);
        world.claimed.set(true);
        let begun = Instant::now();
        tokio::task::LocalSet::new()
            .run_until(async {
                let waiting =
                    tokio::task::spawn_local(client(world.clone(), Startup::default(), begun));
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert_eq!(world.starts.get(), 0, "it waited while the claim was held");
                // The holder exits without a broker: its lock is released.
                world.claimed.set(false);
                waiting.await.unwrap().unwrap();
            })
            .await;
        assert_eq!(world.starts.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_holder_that_never_finishes_is_bypassed_once_after_the_takeover_delay() {
        let world = world(1);
        world.claimed.set(true);
        let begun = Instant::now();
        client(world.clone(), Startup::default(), begun)
            .await
            .unwrap();
        assert_eq!(world.starts.get(), 1);
        assert!(begun.elapsed() >= Startup::default().takeover_after);
        // Noticed at the first poll after the delay, which is at most one step.
        assert!(
            begun.elapsed()
                <= Startup::default().takeover_after + LAST_DELAY + Duration::from_millis(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_companion_that_exits_without_listening_is_replaced_once_and_then_given_up_on() {
        let world = world(0);
        world.start_dies.set(true);
        let begun = Instant::now();
        let error = client(world.clone(), Startup::default(), begun)
            .await
            .unwrap_err();
        assert_eq!(world.starts.get(), 2, "bounded by the client's start limit");
        assert_eq!(begun.elapsed(), Startup::default().budget);
        assert!(error.to_string().contains("did not start"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_companion_that_cannot_be_started_is_reported_at_once() {
        let world = world(0);
        world.start_fails.set(true);
        let begun = Instant::now();
        let error = client(world.clone(), Startup::default(), begun)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(world.starts.get(), 1);
        assert_eq!(begun.elapsed(), Duration::ZERO);
        assert!(!world.claimed.get());
    }

    #[tokio::test(start_paused = true)]
    async fn a_broker_another_client_finished_starting_is_used_when_the_claim_is_taken() {
        // The broker comes up in the instant between this client's first try
        // and its taking the claim: it must not start another.
        let world = world(0);
        let begun = Instant::now();
        let attempts = Cell::new(0);
        let result = start_or_wait(
            &Startup::default(),
            || {
                attempts.set(attempts.get() + 1);
                std::future::ready(if attempts.get() >= 2 {
                    Ok(())
                } else {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                })
            },
            || Ok(Some(Guard(world.clone()))),
            || -> io::Result<()> { panic!("started a companion that was already up") },
            |_| false,
        )
        .await;
        result.unwrap();
        assert_eq!(begun.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_polling_backs_off_from_one_millisecond_to_a_hundred() {
        let world = world(1_000);
        let begun = Instant::now();
        client(world.clone(), Startup::default(), begun)
            .await
            .unwrap();
        let attempts = world.connects.borrow().clone();
        // The second try is the look after taking the claim.
        assert_eq!(
            attempts[..6],
            [0, 0, 1, 3, 7, 15].map(Duration::from_millis)
        );
        assert!(
            attempts
                .windows(2)
                .all(|pair| pair[1] - pair[0] <= LAST_DELAY + Duration::from_millis(1)),
            "{attempts:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_attempt_that_never_finishes_ends_within_the_budget() {
        let startup = Startup {
            budget: Duration::from_millis(5),
            ..Startup::default()
        };
        let began = Instant::now();
        // An outer timeout longer than the budget: the loop must end on its own.
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            start_or_wait::<(), (), (), _>(
                &startup,
                std::future::pending::<io::Result<()>>,
                || Ok(None),
                || Ok(()),
                |_| false,
            ),
        )
        .await
        .expect("the loop outlived its budget");
        assert!(result.unwrap_err().to_string().contains("did not start"));
        assert_eq!(began.elapsed(), Duration::from_millis(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_arrives_after_the_budget_is_not_accepted() {
        let startup = Startup {
            budget: Duration::from_millis(5),
            ..Startup::default()
        };
        let began = Instant::now();
        let result = start_or_wait::<(), (), (), _>(
            &startup,
            || async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(())
            },
            || Ok(None),
            || Ok(()),
            |_| false,
        )
        .await;
        assert!(result.is_err(), "a late connection was accepted");
        assert_eq!(began.elapsed(), Duration::from_millis(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_attempt_does_not_keep_the_client_from_starting_the_companion() {
        // The first tries hang, as one to a socket nobody serves may; the
        // client gives each up at the attempt limit, takes the claim, starts
        // the companion and connects.
        let world = world(1);
        let startup = Startup {
            attempt_limit: Duration::from_millis(50),
            ..Startup::default()
        };
        let begun = Instant::now();
        start_or_wait(
            &startup,
            || {
                let up = world.up();
                async move {
                    if up {
                        Ok(())
                    } else {
                        std::future::pending().await
                    }
                }
            },
            || {
                Ok(if world.claimed.replace(true) {
                    None
                } else {
                    Some(Guard(world.clone()))
                })
            },
            || {
                world.starts.set(world.starts.get() + 1);
                world.listening_from.set(Some(Instant::now()));
                Ok(())
            },
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(world.starts.get(), 1);
        assert!(begun.elapsed() < startup.budget);
    }

    #[tokio::test(start_paused = true)]
    async fn a_broker_that_never_listens_ends_within_the_budget() {
        let world = world(10_000_000);
        let begun = Instant::now();
        let error = client(world.clone(), Startup::default(), begun)
            .await
            .unwrap_err();
        assert_eq!(begun.elapsed(), Startup::default().budget);
        assert!(error.to_string().contains("did not start"));
        assert_eq!(
            world.starts.get(),
            1,
            "a companion still starting is waited for"
        );
    }
}
