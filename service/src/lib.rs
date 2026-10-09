//! Threaded entry point for async/server consumers.
//!
//! The service owns its supervisor on a dedicated thread. A channel closing
//! before a terminal event is converted by the turn handle into one synthetic
//! runtime-loss ending, preserving the exactly-once consumer contract.
//!
//! A turn's events wait in a queue until its handle reads them, and the queue
//! is bounded by the slow-consumer policy of [`seatline_core::backlog`],
//! because a consumer that stops reading must not be able to fill the host's
//! memory:
//!
//! - `Update::Activity` says only that the provider is still working, which the
//!   scheduler has already counted, so a turn queues at most one until the
//!   handle reads it.
//! - Everything else, the answer text above all, waits in the queue until
//!   [`Limits::max_unread_bytes`] ([`MAX_UNREAD_BYTES`] by default) is unread.
//!   Text is never dropped to make room, and the service thread never waits
//!   for a consumer, so the scheduler, cancellation and shutdown stay
//!   responsive. A consumer that is still that far behind when the next event
//!   arrives has the turn **stopped**: what was queued is still delivered, in
//!   order and whole, then the turn ends once with [`CONSUMER_TOO_SLOW`],
//!   which tells it the answer is incomplete. Output the provider produced
//!   after the stop was decided is discarded, which is why that end is a
//!   failure and never a completion.
//!
//! The memory a turn can hold unread is therefore at most `max_unread_bytes`
//! plus the event that crossed it (a provider's single message, which an
//! adapter limits to 8 MiB) plus the output the scheduler had already produced
//! when the stop was decided, which is at most one polling slice, and one
//! unread progress update and the ending. A host that runs many turns at once
//! multiplies that by the number it allows.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use seatline_core::backlog::{Admission, Backlog};
pub use seatline_core::backlog::{CONSUMER_TOO_SLOW, MAX_UNREAD_BYTES};
use seatline_core::exchange::{Exchange, Timeouts};
use seatline_core::process::Reaper;
use seatline_core::turn::{Namespace, Turn as TurnRequest};
use seatline_scheduler::{EndReason, Event, Supervisor, TurnId};

/// How long stopping the service waits for the processes its adapters left to
/// exit on their own, once told to stop: a stop grace of Claude's (2 s) and a
/// moment to kill.
const REAPER_STOP_WAIT: Duration = Duration::from_secs(3);

/// What the service holds back for slow consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How much output one turn may have queued and unread, in bytes of text
    /// and fields plus a fixed cost per event, before the next event stops the
    /// turn. The event that crosses the bound is still queued, so the backlog
    /// can pass it by one event. At least 1.
    pub max_unread_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_unread_bytes: MAX_UNREAD_BYTES,
        }
    }
}

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
    backlog: Arc<Backlog>,
}

/// A turn's queue of events, as the service side sees it.
struct Output {
    events: Sender<Event>,
    backlog: Arc<Backlog>,
    /// The consumer fell behind its bound: the turn is being stopped, and what
    /// it produces from here on is discarded.
    overflowed: bool,
    /// The consumer asked for the turn to stop.
    cancelled: bool,
}

/// What became of one event.
enum Delivery {
    /// It is queued, or it was progress already queued.
    Queued,
    /// It was discarded because the turn is already being stopped for overflow.
    Discarded,
    /// It would pass the bound: the turn must be stopped.
    Overflowed,
    /// Nobody holds the other end.
    Abandoned,
}

impl Output {
    fn new(events: Sender<Event>, backlog: Arc<Backlog>) -> Self {
        Self {
            events,
            backlog,
            overflowed: false,
            cancelled: false,
        }
    }

    /// The consumer asked for the turn to stop. Whichever stopped it first is
    /// why it ended, so this counts only before an overflow.
    fn consumer_cancelled(&mut self) {
        self.cancelled |= !self.overflowed;
    }

    fn deliver(&mut self, event: Event, limits: Limits) -> Delivery {
        let (turn_id, update) = match event {
            Event::Ended { turn_id, reason } => {
                // A turn cut short for its consumer's sake never ends as a
                // success, or as the cancel the service made: output it
                // discarded is missing. A failure of the provider's own, or a
                // cancel the consumer asked for, is what it says.
                let reason = if self.overflowed
                    && !self.cancelled
                    && matches!(reason, EndReason::Completed | EndReason::Cancelled)
                {
                    EndReason::Failed(seatline_core::backlog::too_slow())
                } else {
                    reason
                };
                return self.send(Event::Ended { turn_id, reason });
            }
            Event::Update { turn_id, update } => (turn_id, update),
        };
        if self.overflowed {
            return Delivery::Discarded;
        }
        match self.backlog.admit(&update, limits.max_unread_bytes) {
            Admission::Queue => self.send(Event::Update { turn_id, update }),
            Admission::Skip => Delivery::Queued,
            Admission::Overflow => {
                self.overflowed = true;
                Delivery::Overflowed
            }
        }
    }

    fn send(&self, event: Event) -> Delivery {
        match self.events.send(event) {
            Ok(()) => Delivery::Queued,
            Err(_) => Delivery::Abandoned,
        }
    }
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
        Self::start_with(namespace, Limits::default(), factory)
    }

    /// Starts the service with `limits` for slow consumers instead of the defaults.
    pub fn start_with<F>(namespace: Namespace, limits: Limits, factory: F) -> Self
    where
        F: FnOnce() -> Box<dyn TurnFactory> + Send + 'static,
    {
        let (commands, receiver) = mpsc::channel();
        let thread = thread::spawn(move || service_loop(namespace, limits, factory(), receiver));
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
            backlog,
        } = answer
            .recv()
            .map_err(|_| "runtime service stopped while starting a turn".to_owned())??;
        Ok(Turn {
            id,
            events,
            backlog,
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
    backlog: Arc<Backlog>,
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
                if let Event::Update { update, .. } = &event {
                    self.backlog.read(update);
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
    limits: Limits,
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
                    stop_service(&mut supervisor, &mut outputs, limits);
                    return;
                }
            }
        } else {
            match commands.recv_timeout(Duration::from_millis(10)) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stop_service(&mut supervisor, &mut outputs, limits);
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
                    Ok(Ok((exchange, timeouts))) => {
                        let (sender, events) = mpsc::channel();
                        let backlog = Arc::new(Backlog::default());
                        let id = supervisor.start(exchange, timeouts, Duration::from_millis(250));
                        outputs.insert(id, Output::new(sender, Arc::clone(&backlog)));
                        let _ = reply.send(Ok(Started {
                            id,
                            events,
                            backlog,
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
                if let Some(output) = outputs.get_mut(&id) {
                    output.consumer_cancelled();
                }
                let _ = supervisor.cancel(id);
            }
            Some(Command::Stop) => {
                stop_service(&mut supervisor, &mut outputs, limits);
                return;
            }
            None => {}
        }
        dispatch(&mut supervisor, &mut outputs, limits);
    }
}

fn stop_service(
    supervisor: &mut Supervisor,
    outputs: &mut HashMap<TurnId, Output>,
    limits: Limits,
) {
    supervisor.shutdown(Duration::from_millis(250));
    while !supervisor.is_empty() {
        dispatch(supervisor, outputs, limits);
        thread::sleep(Duration::from_millis(1));
    }
    // A finished turn's process that its adapter left to exit on its own is
    // stopped too, and reaped: dropping the runtime leaves nothing of its
    // turns running, even for a host that exits right after. (The reaper is
    // the process's, so this also ends the wait for another runtime's.)
    Reaper::shared().stop_all(REAPER_STOP_WAIT);
}

fn dispatch(supervisor: &mut Supervisor, outputs: &mut HashMap<TurnId, Output>, limits: Limits) {
    for event in supervisor.poll(Duration::from_millis(5)) {
        let id = match &event {
            Event::Update { turn_id, .. } | Event::Ended { turn_id, .. } => *turn_id,
        };
        let ended = matches!(event, Event::Ended { .. });
        let Some(output) = outputs.get_mut(&id) else {
            continue;
        };
        match output.deliver(event, limits) {
            Delivery::Queued | Delivery::Discarded => {}
            // The consumer cannot keep up: stop the turn, without waiting for it.
            Delivery::Overflowed => {
                let _ = supervisor.cancel(id);
            }
            Delivery::Abandoned => {
                if !ended {
                    let _ = supervisor.cancel(id);
                }
                outputs.remove(&id);
            }
        }
        if ended {
            outputs.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Instant;

    use seatline_core::exchange::Update;

    use seatline_core::protocol::{ErrorCode, Failure};

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
                reasoning_effort: None,
                service_tier: None,
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
                reasoning_effort: None,
                service_tier: None,
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
            reasoning_effort: None,
            service_tier: None,
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

    /// What `n` of a flood's text is: numbered, so a gap or a repeat shows.
    fn piece(n: u64) -> String {
        format!("{n:08}{}", "x".repeat(1016))
    }

    /// Streams numbered 1 KiB pieces of text as fast as it is asked, without
    /// end, until it is cancelled.
    struct FloodsText {
        next: u64,
        started: bool,
        cancelled: Arc<AtomicBool>,
    }

    impl Exchange for FloodsText {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            if self.cancelled.load(Ordering::SeqCst) {
                return Some(Update::Stopped);
            }
            if !std::mem::replace(&mut self.started, true) {
                return Some(Update::Started);
            }
            self.next += 1;
            Some(Update::Delta(piece(self.next - 1)))
        }

        fn cancel(&mut self, _grace: Duration) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    /// Streams `total` numbered pieces and completes, `burst` at a time with a
    /// pause in between, as a provider that produces at a steady rate does.
    struct Paced {
        total: u64,
        burst: u64,
        sent: u64,
        in_burst: u64,
        started: bool,
    }

    impl Exchange for Paced {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            if !std::mem::replace(&mut self.started, true) {
                return Some(Update::Started);
            }
            if self.sent == self.total {
                return Some(Update::Completed);
            }
            if self.in_burst == self.burst {
                self.in_burst = 0;
                return None;
            }
            self.in_burst += 1;
            self.sent += 1;
            Some(Update::Delta(piece(self.sent - 1)))
        }

        fn cancel(&mut self, _grace: Duration) {
            self.total = self.sent;
        }
    }

    /// Floods a turn that asks for "flood", answers a turn that asks for
    /// "quick" at once, and records whether a flood was cancelled.
    struct Mixed {
        cancelled: Arc<AtomicBool>,
    }

    impl TurnFactory for Mixed {
        fn start(
            &mut self,
            request: TurnRequest,
        ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String> {
            let exchange: Box<dyn Exchange> = if request.messages[0].text == "flood" {
                Box::new(FloodsText {
                    next: 0,
                    started: false,
                    cancelled: Arc::clone(&self.cancelled),
                })
            } else {
                Box::new(One(VecDeque::from([Update::Started, Update::Completed])))
            };
            Ok((exchange, None))
        }
    }

    fn ask(text: &str) -> TurnRequest {
        let mut request = hello();
        request.messages[0].text = text.to_owned();
        request
    }

    fn tight(max_unread_bytes: usize) -> Limits {
        Limits { max_unread_bytes }
    }

    fn flood_service(limits: Limits) -> (Runtime, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&cancelled);
        let runtime = Runtime::start_with(Namespace::fixed("test").unwrap(), limits, move || {
            Box::new(Mixed { cancelled: seen })
        });
        (runtime, cancelled)
    }

    /// Everything a turn has, read to its end: the numbers of the pieces of
    /// text it gave, the bytes of text, and how it ended.
    fn drain(turn: &mut Turn) -> (Vec<u64>, usize, EndReason) {
        let mut numbers = Vec::new();
        let mut bytes = 0;
        let mut ended = Vec::new();
        while let Some(event) = turn.recv() {
            match event {
                Event::Update {
                    update: Update::Delta(text),
                    ..
                } => {
                    bytes += text.len();
                    numbers.push(text[..8].parse().expect("a numbered piece"));
                }
                Event::Update { .. } => {}
                Event::Ended { reason, .. } => ended.push(reason),
            }
        }
        assert_eq!(ended.len(), 1, "a turn ends exactly once: {ended:?}");
        (numbers, bytes, ended.remove(0))
    }

    fn too_slow(reason: &EndReason) -> bool {
        matches!(reason, EndReason::Failed(failure) if failure.reason == CONSUMER_TOO_SLOW)
    }

    #[test]
    fn a_consumer_that_stops_reading_has_its_turn_stopped_with_the_text_it_was_given_intact() {
        let bound = 64 * 1024;
        let (runtime, cancelled) = flood_service(tight(bound));
        let mut turn = runtime.start_turn(ask("flood")).unwrap();

        // Nothing is read while the provider floods.
        thread::sleep(Duration::from_millis(400));
        assert!(
            cancelled.load(Ordering::SeqCst),
            "the flooding exchange was never stopped"
        );

        let (numbers, bytes, reason) = drain(&mut turn);
        // What it was given is the start of the answer: whole, in order, from
        // the first piece, with nothing skipped, and within the bound.
        assert!(!numbers.is_empty());
        assert_eq!(numbers, (0..numbers.len() as u64).collect::<Vec<_>>());
        // The event that crossed the bound is queued too: one piece more.
        assert!(
            bytes <= bound + 1024,
            "{bytes} bytes queued behind a bound of {bound}"
        );
        // It is told, once, that the answer is incomplete.
        assert!(too_slow(&reason), "{reason:?}");
    }

    #[test]
    fn a_consumer_that_keeps_up_gets_everything_however_much_there_is() {
        struct PacedFactory;
        impl TurnFactory for PacedFactory {
            fn start(
                &mut self,
                _request: TurnRequest,
            ) -> Result<(Box<dyn Exchange>, Option<Timeouts>), String> {
                Ok((
                    Box::new(Paced {
                        total: 600,
                        burst: 12,
                        sent: 0,
                        in_burst: 0,
                        started: false,
                    }),
                    None,
                ))
            }
        }
        // 600 KiB of text through a 32 KiB bound: no failure for a reader
        // that is reading.
        let runtime =
            Runtime::start_with(Namespace::fixed("test").unwrap(), tight(32 * 1024), || {
                Box::new(PacedFactory)
            });
        let mut turn = runtime.start_turn(hello()).unwrap();
        let (numbers, bytes, reason) = drain(&mut turn);
        assert_eq!(reason, EndReason::Completed);
        assert_eq!(numbers, (0..600).collect::<Vec<_>>());
        assert_eq!(bytes, 600 * 1024);
    }

    #[test]
    fn a_paused_consumer_holds_up_no_other_turn_and_no_shutdown() {
        let (runtime, cancelled) = flood_service(tight(64 * 1024));
        let mut paused = runtime.start_turn(ask("flood")).unwrap();
        thread::sleep(Duration::from_millis(100));

        // Another turn on the same service is served at once.
        let started = Instant::now();
        let mut quick = runtime.start_turn(ask("quick")).unwrap();
        let (_, _, reason) = drain(&mut quick);
        assert_eq!(reason, EndReason::Completed);
        assert!(started.elapsed() < Duration::from_secs(1));

        // Stopping the service does not wait for the consumer that is not reading.
        let started = Instant::now();
        drop(runtime);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(cancelled.load(Ordering::SeqCst));

        // The turn still ends once, and says why the answer stops where it does.
        let (numbers, _, reason) = drain(&mut paused);
        assert_eq!(numbers, (0..numbers.len() as u64).collect::<Vec<_>>());
        assert!(too_slow(&reason), "{reason:?}");
    }

    #[test]
    fn cancelling_a_turn_that_is_flooding_a_consumer_ends_it_once() {
        let (runtime, _) = flood_service(tight(64 * 1024));
        let mut turn = runtime.start_turn(ask("flood")).unwrap();
        thread::sleep(Duration::from_millis(200));
        // The service stopped it first, so that is why it ended.
        turn.cancel();
        let (numbers, _, reason) = drain(&mut turn);
        assert_eq!(numbers, (0..numbers.len() as u64).collect::<Vec<_>>());
        assert!(too_slow(&reason), "{reason:?}");
    }

    /// An `Output` with nobody reading, for the rules of one turn's queue.
    fn unread_output() -> (Output, Receiver<Event>, Arc<Backlog>) {
        let (sender, events) = mpsc::channel();
        let backlog = Arc::new(Backlog::default());
        (Output::new(sender, Arc::clone(&backlog)), events, backlog)
    }

    fn text(turn_id: TurnId, bytes: usize) -> Event {
        Event::Update {
            turn_id,
            update: Update::Delta("a".repeat(bytes)),
        }
    }

    fn id() -> TurnId {
        Supervisor::new().start(
            Box::new(One(VecDeque::new())),
            None,
            Duration::from_millis(1),
        )
    }

    #[test]
    fn an_event_that_arrives_over_the_bound_stops_the_turn_and_later_output_is_discarded() {
        let limits = tight(1_000);
        let (mut output, events, backlog) = unread_output();
        let turn = id();
        // Each piece costs its text and a fixed amount on top. Two leave the
        // backlog under the bound; the third crosses it and is still queued.
        for _ in 0..3 {
            assert!(matches!(
                output.deliver(text(turn, 400), limits),
                Delivery::Queued
            ));
        }
        assert_eq!(
            backlog.unread(),
            3 * (400 + seatline_core::backlog::weight(&Update::Delta(String::new())))
        );
        // The bound is passed, so the next does not get in, and nothing after
        // it is kept, small or not.
        assert!(matches!(
            output.deliver(text(turn, 400), limits),
            Delivery::Overflowed
        ));
        assert!(matches!(
            output.deliver(text(turn, 1), limits),
            Delivery::Discarded
        ));
        assert_eq!(events.try_iter().count(), 3);
        // Ending: a completion cannot stand for a turn that lost text.
        let ended = Event::Ended {
            turn_id: turn,
            reason: EndReason::Completed,
        };
        assert!(matches!(output.deliver(ended, limits), Delivery::Queued));
        match events.try_recv() {
            Ok(Event::Ended { reason, .. }) => assert!(too_slow(&reason), "{reason:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_cancel_the_consumer_asked_for_first_is_what_the_turn_ends_as() {
        let limits = tight(500);
        let turn = id();
        for (first_cancelled, expected_slow) in [(true, false), (false, true)] {
            let (mut output, events, _) = unread_output();
            // Two pieces fill the bound, the third finds it passed.
            output.deliver(text(turn, 400), limits);
            output.deliver(text(turn, 400), limits);
            if first_cancelled {
                // The service records the consumer's cancel before the overflow.
                output.cancelled = true;
            }
            assert!(matches!(
                output.deliver(text(turn, 400), limits),
                Delivery::Overflowed
            ));
            // A cancel after the overflow changes nothing.
            output.cancelled |= !output.overflowed;
            let ended = Event::Ended {
                turn_id: turn,
                reason: EndReason::Cancelled,
            };
            output.deliver(ended, limits);
            let reason = events
                .try_iter()
                .find_map(|event| match event {
                    Event::Ended { reason, .. } => Some(reason),
                    _ => None,
                })
                .unwrap();
            assert_eq!(too_slow(&reason), expected_slow, "{reason:?}");
            if !expected_slow {
                assert_eq!(reason, EndReason::Cancelled);
            }
        }
    }

    #[test]
    fn a_failure_of_the_providers_own_is_not_hidden_by_the_overflow() {
        let limits = tight(500);
        let (mut output, events, _) = unread_output();
        let turn = id();
        output.deliver(text(turn, 400), limits);
        output.deliver(text(turn, 400), limits);
        assert!(matches!(
            output.deliver(text(turn, 400), limits),
            Delivery::Overflowed
        ));
        let failure = Failure {
            code: ErrorCode::ProviderFailed,
            reason: "PROVIDER_FAILED",
            retryable: false,
        };
        output.deliver(
            Event::Ended {
                turn_id: turn,
                reason: EndReason::Failed(failure),
            },
            limits,
        );
        let reason = events
            .try_iter()
            .find_map(|event| match event {
                Event::Ended { reason, .. } => Some(reason),
                _ => None,
            })
            .unwrap();
        assert_eq!(reason, EndReason::Failed(failure));
    }
}
