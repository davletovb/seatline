//! Threaded entry point for async/server consumers.
//!
//! The service owns its supervisor on a dedicated thread. A channel closing
//! before a terminal event is converted by the turn handle into one synthetic
//! runtime-loss ending, preserving the exactly-once consumer contract.
//!
//! A turn's events wait in a queue until its handle reads them. `Update::Activity`
//! says only that the provider is still working, which the scheduler has already
//! counted, so a turn queues at most one until the handle reads it: a provider
//! that floods progress cannot fill memory behind a consumer that reads slowly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use seatline_core::exchange::{Exchange, Timeouts, Update};
use seatline_core::turn::{Namespace, Turn as TurnRequest};
use seatline_scheduler::{EndReason, Event, Supervisor, TurnId};

pub trait TurnFactory: 'static {
    /// Construct one provider exchange. This runs on the service polling
    /// thread and must return promptly; slow provider discovery or sign-in
    /// probes belong inside the returned exchange so other turns keep moving.
    fn start(
        &mut self,
        request: TurnRequest,
    ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String>;
}

/// What a caller gets for a turn it started.
struct Started {
    id: TurnId,
    events: Receiver<Event>,
    activity_queued: Arc<AtomicBool>,
}

/// A turn's queue of events, as the service side sees it.
struct Output {
    events: Sender<Event>,
    /// Whether an `Update::Activity` is queued and unread.
    activity_queued: Arc<AtomicBool>,
}

enum Command {
    Start {
        request: TurnRequest,
        reply: Sender<Result<Started, String>>,
    },
    Cancel(TurnId),
    Stop,
}

pub struct Runtime {
    commands: Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Runtime {
    pub fn start<F>(namespace: Namespace, factory: F) -> Self
    where
        F: FnOnce() -> Box<dyn TurnFactory> + Send + 'static,
    {
        let (commands, receiver) = mpsc::channel();
        let thread = thread::spawn(move || service_loop(namespace, factory(), receiver));
        Self {
            commands,
            thread: Some(thread),
        }
    }

    pub fn start_turn(&self, request: TurnRequest) -> Result<Turn, String> {
        request
            .validate()
            .map_err(|error| format!("invalid turn: {error:?}"))?;
        let (reply, answer) = mpsc::channel();
        self.commands
            .send(Command::Start { request, reply })
            .map_err(|_| "runtime service is not running".to_owned())?;
        let Started {
            id,
            events,
            activity_queued,
        } = answer
            .recv()
            .map_err(|_| "runtime service stopped while starting a turn".to_owned())??;
        Ok(Turn {
            id,
            events,
            activity_queued,
            commands: self.commands.clone(),
            ended: false,
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Turn {
    id: TurnId,
    events: Receiver<Event>,
    activity_queued: Arc<AtomicBool>,
    commands: Sender<Command>,
    ended: bool,
}

impl Turn {
    pub fn id(&self) -> TurnId {
        self.id
    }

    pub fn cancel(&self) {
        if !self.ended {
            let _ = self.commands.send(Command::Cancel(self.id));
        }
    }

    pub fn recv(&mut self) -> Option<Event> {
        if self.ended {
            return None;
        }
        match self.events.recv() {
            Ok(event @ Event::Ended { .. }) => {
                self.ended = true;
                Some(event)
            }
            Ok(event) => {
                if matches!(
                    event,
                    Event::Update {
                        update: Update::Activity,
                        ..
                    }
                ) {
                    self.activity_queued.store(false, Ordering::SeqCst);
                }
                Some(event)
            }
            Err(_) => {
                self.ended = true;
                Some(Event::Ended {
                    turn_id: self.id,
                    reason: EndReason::SchedulerPanicked {
                        maybe_started: true,
                    },
                })
            }
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.commands.send(Command::Cancel(self.id));
        }
    }
}

fn service_loop(
    _namespace: Namespace,
    mut factory: Box<dyn TurnFactory>,
    commands: Receiver<Command>,
) {
    let mut supervisor = Supervisor::new();
    let mut outputs: HashMap<TurnId, Output> = HashMap::new();

    loop {
        let command = if supervisor.is_empty() {
            match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => {
                    stop_service(&mut supervisor, &mut outputs);
                    return;
                }
            }
        } else {
            match commands.recv_timeout(Duration::from_millis(10)) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stop_service(&mut supervisor, &mut outputs);
                    return;
                }
            }
        };

        match command {
            Some(Command::Start { request, reply }) => {
                let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    factory.start(request)
                }));
                match started {
                    Ok(Ok((exchange, limits))) => {
                        let (sender, events) = mpsc::channel();
                        let activity_queued = Arc::new(AtomicBool::new(false));
                        let id = supervisor.start(exchange, limits, Duration::from_millis(250));
                        outputs.insert(
                            id,
                            Output {
                                events: sender,
                                activity_queued: Arc::clone(&activity_queued),
                            },
                        );
                        let _ = reply.send(Ok(Started {
                            id,
                            events,
                            activity_queued,
                        }));
                    }
                    Ok(Err(error)) => {
                        let _ = reply.send(Err(error));
                    }
                    Err(_) => {
                        let _ = reply.send(Err("provider factory panicked".to_owned()));
                    }
                }
            }
            Some(Command::Cancel(id)) => {
                let _ = supervisor.cancel(id);
            }
            Some(Command::Stop) => {
                stop_service(&mut supervisor, &mut outputs);
                return;
            }
            None => {}
        }
        dispatch(&mut supervisor, &mut outputs);
    }
}

fn stop_service(supervisor: &mut Supervisor, outputs: &mut HashMap<TurnId, Output>) {
    supervisor.shutdown(Duration::from_millis(250));
    while !supervisor.is_empty() {
        dispatch(supervisor, outputs);
        thread::sleep(Duration::from_millis(1));
    }
}

fn dispatch(supervisor: &mut Supervisor, outputs: &mut HashMap<TurnId, Output>) {
    for event in supervisor.poll(Duration::from_millis(5)) {
        let id = match &event {
            Event::Update { turn_id, .. } | Event::Ended { turn_id, .. } => *turn_id,
        };
        let ended = matches!(event, Event::Ended { .. });
        let delivered = outputs.get(&id).is_none_or(|output| {
            let progress = matches!(
                event,
                Event::Update {
                    update: Update::Activity,
                    ..
                }
            );
            // One unread progress update says all there is to say.
            if progress && output.activity_queued.swap(true, Ordering::SeqCst) {
                return true;
            }
            output.events.send(event).is_ok()
        });
        if !delivered && !ended {
            let _ = supervisor.cancel(id);
            outputs.remove(&id);
        } else if ended {
            outputs.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Instant;

    use seatline_core::exchange::Update;

    use super::*;

    struct Factory;

    struct One(VecDeque<Update>);

    impl Exchange for One {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            self.0.pop_front()
        }

        fn cancel(&mut self, _grace: Duration) {
            self.0 = VecDeque::from([Update::Stopped]);
        }
    }

    impl TurnFactory for Factory {
        fn start(
            &mut self,
            _request: TurnRequest,
        ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String> {
            Ok((
                Box::new(One(VecDeque::from([Update::Started, Update::Completed]))),
                Some(Timeouts {
                    start: Duration::from_secs(1),
                    idle: Duration::from_secs(1),
                    max_turn: Duration::from_secs(2),
                    stop_grace: Duration::ZERO,
                }),
            ))
        }
    }

    #[test]
    fn service_turn_ends_once() {
        let runtime = Runtime::start(Namespace::fixed("test").unwrap(), || Box::new(Factory));
        let mut turn = runtime
            .start_turn(TurnRequest {
                system: None,
                messages: vec![seatline_core::turn::Message {
                    role: seatline_core::turn::Role::User,
                    text: "hello".to_owned(),
                }],
                model: None,
                tools: seatline_core::turn::ToolPolicy::None,
                session: seatline_core::turn::SessionPolicy::Ephemeral,
                continuation: None,
                cleanup_group: None,
                check_sign_in: false,
            })
            .unwrap();
        let mut ended = 0;
        while let Some(event) = turn.recv() {
            if matches!(event, Event::Ended { .. }) {
                ended += 1;
            }
        }
        assert_eq!(ended, 1);
    }
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct DropFactory {
        cancelled: Arc<AtomicBool>,
    }

    struct Hangs {
        cancelled: Arc<AtomicBool>,
    }

    impl Exchange for Hangs {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            None
        }

        fn cancel(&mut self, _grace: Duration) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    impl TurnFactory for DropFactory {
        fn start(
            &mut self,
            _request: TurnRequest,
        ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String> {
            Ok((
                Box::new(Hangs {
                    cancelled: Arc::clone(&self.cancelled),
                }),
                None,
            ))
        }
    }

    #[test]
    fn dropping_a_turn_cancels_its_exchange() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&cancelled);
        let runtime = Runtime::start(Namespace::fixed("test").unwrap(), move || {
            Box::new(DropFactory { cancelled: probe })
        });
        let turn = runtime
            .start_turn(TurnRequest {
                system: None,
                messages: vec![seatline_core::turn::Message {
                    role: seatline_core::turn::Role::User,
                    text: "hello".to_owned(),
                }],
                model: None,
                tools: seatline_core::turn::ToolPolicy::None,
                session: seatline_core::turn::SessionPolicy::Ephemeral,
                continuation: None,
                cleanup_group: None,
                check_sign_in: false,
            })
            .unwrap();
        drop(turn);

        let deadline = Instant::now() + Duration::from_secs(1);
        while !cancelled.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(cancelled.load(Ordering::SeqCst));
    }

    /// Reports progress as fast as it is asked, until it is cancelled.
    struct FloodsProgress {
        started: bool,
        cancelled: bool,
    }

    impl Exchange for FloodsProgress {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            if self.cancelled {
                return Some(Update::Stopped);
            }
            if !std::mem::replace(&mut self.started, true) {
                return Some(Update::Started);
            }
            Some(Update::Activity)
        }

        fn cancel(&mut self, _grace: Duration) {
            self.cancelled = true;
        }
    }

    struct FloodFactory;

    impl TurnFactory for FloodFactory {
        fn start(
            &mut self,
            _request: TurnRequest,
        ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String> {
            Ok((
                Box::new(FloodsProgress {
                    started: false,
                    cancelled: false,
                }),
                None,
            ))
        }
    }

    fn hello() -> TurnRequest {
        TurnRequest {
            system: None,
            messages: vec![seatline_core::turn::Message {
                role: seatline_core::turn::Role::User,
                text: "hello".to_owned(),
            }],
            model: None,
            tools: seatline_core::turn::ToolPolicy::None,
            session: seatline_core::turn::SessionPolicy::Ephemeral,
            continuation: None,
            cleanup_group: None,
            check_sign_in: false,
        }
    }

    #[test]
    fn a_slow_consumer_holds_at_most_one_unread_progress_update() {
        let runtime = Runtime::start(Namespace::fixed("test").unwrap(), || Box::new(FloodFactory));
        let mut turn = runtime.start_turn(hello()).unwrap();

        // Nothing is read while the provider floods progress, and then the
        // turn is cancelled and given time to end.
        thread::sleep(Duration::from_millis(300));
        turn.cancel();
        thread::sleep(Duration::from_millis(500));

        let mut progress = 0;
        let mut ended = 0;
        while let Some(event) = turn.recv() {
            match event {
                Event::Update {
                    update: Update::Activity,
                    ..
                } => progress += 1,
                Event::Ended { .. } => ended += 1,
                Event::Update { .. } => {}
            }
        }
        assert_eq!(progress, 1, "unread progress piled up");
        assert_eq!(ended, 1);
    }

    #[test]
    fn progress_that_was_read_is_reported_again() {
        let runtime = Runtime::start(Namespace::fixed("test").unwrap(), || Box::new(FloodFactory));
        let mut turn = runtime.start_turn(hello()).unwrap();
        let mut progress = 0;
        while progress < 3 {
            match turn.recv().expect("the turn is still running") {
                Event::Update {
                    update: Update::Activity,
                    ..
                } => progress += 1,
                Event::Ended { reason, .. } => panic!("ended early: {reason:?}"),
                Event::Update { .. } => {}
            }
        }
    }
}
