//! Bounded filesystem work. Reservations include queued, running, and future
//! jobs, so a provider can reserve cleanup before launching a child.
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};

type Work = Box<dyn FnOnce() -> io::Result<()> + Send>;
struct Job {
    work: Work,
    reply: SyncSender<io::Result<()>>,
    _permit: Permit,
}

pub struct Worker {
    jobs: SyncSender<Job>,
    outstanding: Arc<AtomicUsize>,
    capacity: usize,
}
pub struct Permit {
    jobs: SyncSender<Job>,
    outstanding: Arc<AtomicUsize>,
}

impl Worker {
    pub fn new(threads: usize, capacity: usize) -> io::Result<Self> {
        if threads == 0 || capacity == 0 {
            return Err(io::Error::other("empty worker"));
        }
        let (jobs, input) = mpsc::sync_channel::<Job>(capacity);
        let input = Arc::new(Mutex::new(input));
        for index in 0..threads {
            let input = input.clone();
            std::thread::Builder::new()
                .name(format!("seatline-fs-{index}"))
                .spawn(move || {
                    loop {
                        let Ok(job) = input.lock().unwrap().recv() else {
                            break;
                        };
                        let Job {
                            work,
                            reply,
                            _permit: permit,
                        } = job;
                        let result = catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| {
                            Err(io::Error::other("filesystem worker panicked"))
                        });
                        drop(permit);
                        let _ = reply.try_send(result);
                        // Dropping the permit releases this reservation even when
                        // the result receiver disappeared or the work panicked.
                    }
                })?;
        }
        Ok(Self {
            jobs,
            outstanding: Arc::new(AtomicUsize::new(0)),
            capacity,
        })
    }

    /// Shared bounded pool for provider/companion cleanup, initialized lazily.
    pub fn cleanup() -> io::Result<&'static Self> {
        static POOL: OnceLock<Worker> = OnceLock::new();
        static INIT: Mutex<()> = Mutex::new(());
        initialize(&POOL, &INIT, || Self::new(2, 32))
    }

    pub fn reserve(&self) -> io::Result<Permit> {
        let mut outstanding = self.outstanding.load(Ordering::Acquire);
        loop {
            if outstanding >= self.capacity {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "cleanup capacity exhausted",
                ));
            }
            match self.outstanding.compare_exchange_weak(
                outstanding,
                outstanding + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => outstanding = current,
            }
        }
        Ok(Permit {
            jobs: self.jobs.clone(),
            outstanding: self.outstanding.clone(),
        })
    }
}

// Rust 1.85 has no stable get_or_try_init. Serialize initialization and cache
// only success, so a transient thread-spawn failure can be retried.
fn initialize<'a>(
    pool: &'a OnceLock<Worker>,
    gate: &Mutex<()>,
    create: impl FnOnce() -> io::Result<Worker>,
) -> io::Result<&'a Worker> {
    if let Some(worker) = pool.get() {
        return Ok(worker);
    }
    let _guard = gate
        .lock()
        .map_err(|_| io::Error::other("worker initialization panicked"))?;
    if let Some(worker) = pool.get() {
        return Ok(worker);
    }
    let worker = create()?;
    Ok(pool.get_or_init(|| worker))
}

impl Permit {
    pub fn submit(
        self,
        work: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<Receiver<io::Result<()>>> {
        let (reply, result) = mpsc::sync_channel(1);
        let jobs = self.jobs.clone();
        jobs.try_send(Job {
            work: Box::new(work),
            reply,
            _permit: self,
        })
        .map_err(|_| io::Error::other("cleanup worker stopped"))?;
        Ok(result)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn failed_pool_initialization_is_retried_and_success_is_shared() {
        let pool = OnceLock::new();
        let gate = Mutex::new(());
        assert!(initialize(&pool, &gate, || Err(io::Error::other("spawn failed"))).is_err());
        let first = initialize(&pool, &gate, || Worker::new(1, 1)).unwrap();
        let second = initialize(&pool, &gate, || panic!("initialized twice")).unwrap();
        assert!(std::ptr::eq(first, second));
        assert!(
            first
                .reserve()
                .unwrap()
                .submit(|| Ok(()))
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn capacity_includes_reserved_and_running_cleanup_and_returns_after_panic() {
        let pool = Worker::new(1, 2).unwrap();
        let first = pool.reserve().unwrap();
        let second = pool.reserve().unwrap();
        assert!(pool.reserve().is_err());
        drop(second);
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let result = first
            .submit(move || {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            })
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = pool.reserve().unwrap();
        assert!(pool.reserve().is_err());
        drop(result);
        let failed = second.submit(|| panic!("injected cleanup panic")).unwrap();
        release.send(()).unwrap();
        assert!(
            failed
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        let later = pool.reserve().unwrap().submit(|| Ok(())).unwrap();
        assert!(later.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
    }

    #[test]
    fn a_second_worker_services_another_apps_cleanup_during_slow_io() {
        let pool = Worker::new(2, 2).unwrap();
        let (release, wait) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        let slow = pool
            .reserve()
            .unwrap()
            .submit(move || {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            })
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let other = pool.reserve().unwrap().submit(|| Ok(())).unwrap();
        assert!(other.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        release.send(()).unwrap();
        assert!(slow.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
    }
}
