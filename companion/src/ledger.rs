//! One bounded, serial writer owns durable mutations. Neither serialization,
//! fsync nor replacement runs on the hub. Failed mutations restore the writer's
//! indexes before it accepts the next mutation; no-op removals do not write.
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};

use crate::{
    config,
    sessions::{Session, Sessions},
};

pub(crate) const CAPACITY: usize = 32;

pub(crate) enum Mutation {
    Insert(String, Session),
    Remove(Vec<String>),
}

struct Job {
    mutation: Mutation,
    reply: SyncSender<io::Result<()>>,
}

pub(crate) struct Ledger {
    jobs: SyncSender<Job>,
}

impl Ledger {
    pub fn new(root: PathBuf, sessions: Sessions) -> io::Result<Self> {
        Self::with_writer(sessions, move |sessions| {
            config::write_private(&root.join("sessions.json"), &serde_json::to_vec(sessions)?)
        })
    }

    pub(crate) fn with_writer(
        mut sessions: Sessions,
        mut write: impl FnMut(&Sessions) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let (jobs, input) = mpsc::sync_channel::<Job>(CAPACITY);
        std::thread::Builder::new()
            .name("seatline-ledger".into())
            .spawn(move || {
                while let Ok(first) = input.recv() {
                    let mut batch = vec![first];
                    batch.extend(input.try_iter().take(CAPACITY - 1));
                    let mut undo = Vec::new();
                    let mut replies = Vec::new();
                    for job in batch {
                        match job.mutation {
                            Mutation::Insert(token, session) => {
                                if sessions.get(&token) != Some(&session) {
                                    let old = sessions.insert(token.clone(), session);
                                    undo.push((token, old));
                                }
                            }
                            Mutation::Remove(tokens) => {
                                for token in tokens {
                                    if let Some(session) = sessions.remove(&token) {
                                        undo.push((token, Some(session)));
                                    }
                                }
                            }
                        }
                        replies.push(job.reply);
                    }
                    let result = if undo.is_empty() {
                        Ok(())
                    } else {
                        write(&sessions)
                    };
                    if result.is_err() {
                        for (token, old) in undo.into_iter().rev() {
                            sessions.remove(&token);
                            if let Some(session) = old {
                                sessions.insert(token, session);
                            }
                        }
                    }
                    for reply in replies {
                        let result = match &result {
                            Err(error) => {
                                Err(io::Error::new(error.kind(), "session persistence failed"))
                            }
                            _ => Ok(()),
                        };
                        let _ = reply.try_send(result);
                    }
                }
            })?;
        Ok(Self { jobs })
    }

    pub fn submit(&self, mutation: Mutation) -> io::Result<Receiver<io::Result<()>>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.jobs
            .try_send(Job { mutation, reply })
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "ledger worker unavailable"))?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn session(app: &str, native: &str) -> Session {
        Session {
            app: app.into(),
            provider: "codex".into(),
            native: native.into(),
        }
    }

    #[test]
    fn failed_insert_and_remove_roll_back_before_the_next_durable_mutation() {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let copy = snapshots.clone();
        let mut n = 0;
        let worker = Ledger::with_writer(Sessions::default(), move |sessions| {
            n += 1;
            copy.lock()
                .unwrap()
                .push(serde_json::to_value(sessions).unwrap());
            if n == 1 || n == 3 {
                Err(io::Error::other("injected I/O failure"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        let wait = |mutation| {
            worker
                .submit(mutation)
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
        };
        assert!(wait(Mutation::Insert("bad".into(), session("first", "bad"))).is_err());
        assert!(wait(Mutation::Insert("good".into(), session("second", "good"))).is_ok());
        assert!(wait(Mutation::Remove(vec!["good".into()])).is_err());
        assert!(wait(Mutation::Insert("next".into(), session("third", "next"))).is_ok());
        // Removing something absent and reinserting an identical record are no-ops.
        assert!(wait(Mutation::Remove(vec!["absent".into()])).is_ok());
        assert!(wait(Mutation::Insert("next".into(), session("third", "next"))).is_ok());
        let snapshots = snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 4);
        assert!(snapshots[1].get("bad").is_none());
        assert!(snapshots[3].get("good").is_some());
        assert!(snapshots[3].get("next").is_some());
    }

    #[test]
    fn a_stalled_writer_is_bounded_and_dropped_waiters_do_not_lose_mutations() {
        let (release, blocked) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        let mut first = true;
        let worker = Ledger::with_writer(Sessions::default(), move |_| {
            if first {
                first = false;
                entered.send(()).unwrap();
                blocked.recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
        let result = worker
            .submit(Mutation::Insert("first".into(), session("a", "n")))
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(result);
        let mut results = Vec::new();
        for n in 0..CAPACITY {
            results.push(
                worker
                    .submit(Mutation::Insert(
                        n.to_string(),
                        session("a", &n.to_string()),
                    ))
                    .unwrap(),
            );
        }
        assert!(worker.submit(Mutation::Remove(Vec::new())).is_err());
        release.send(()).unwrap();
        for result in results {
            assert!(result.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        }
    }
}
