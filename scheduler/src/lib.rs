//! Fair, bounded scheduling for provider exchanges.
//!
//! The scheduler owns provider exchanges and their lifecycle limits. It has no
//! browser or Native Messaging concepts. The supervisor is the panic boundary
//! shared by synchronous and service-thread entry points.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use seatline_core::exchange::{Exchange, Timeouts, Update};
use seatline_core::protocol::Failure;

const STOP_SLACK: Duration = Duration::from_secs(1);

pub type TurnId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    Start,
    Idle,
    Absolute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    Failed(Failure),
    Cancelled,
    Timeout(TimeoutKind),
    StoppedUnexpectedly,
    AdapterPanicked { maybe_started: bool },
    SchedulerPanicked { maybe_started: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Update { turn_id: TurnId, update: Update },
    Ended { turn_id: TurnId, reason: EndReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Cancelled,
    Timeout(TimeoutKind),
}

struct Running {
    id: TurnId,
    exchange: Option<Box<dyn Exchange>>,
    timeouts: Option<Timeouts>,
    stop_grace: Duration,
    started_at: Instant,
    last_work: Instant,
    started: bool,
    stop: Option<StopReason>,
    stop_limit: Option<Instant>,
}

impl Running {
    fn timeout(&self, now: Instant) -> Option<TimeoutKind> {
        let limits = self.timeouts?;
        if now.saturating_duration_since(self.started_at) >= limits.max_turn {
            return Some(TimeoutKind::Absolute);
        }
        if self.started {
            (now.saturating_duration_since(self.last_work) >= limits.idle)
                .then_some(TimeoutKind::Idle)
        } else {
            (now.saturating_duration_since(self.started_at) >= limits.start)
                .then_some(TimeoutKind::Start)
        }
    }

    fn recognized_work(update: &Update) -> bool {
        matches!(
            update,
            Update::Started
                | Update::Activity
                | Update::Delta(_)
                | Update::Source(_)
                | Update::Usage(_)
        )
    }

    fn cancel_exchange(&mut self, grace: Duration) -> Result<(), ()> {
        let Some(exchange) = self.exchange.as_mut() else {
            return Ok(());
        };
        catch_unwind(AssertUnwindSafe(|| exchange.cancel(grace))).map_err(|_| ())
    }

    fn next(&mut self, deadline: Instant) -> Result<Option<Update>, ()> {
        let Some(exchange) = self.exchange.as_mut() else {
            return Ok(None);
        };
        catch_unwind(AssertUnwindSafe(|| exchange.next(deadline))).map_err(|_| ())
    }

    fn begin_stop(&mut self, reason: StopReason, grace: Duration, now: Instant) -> bool {
        if self.stop.is_some() {
            return false;
        }
        let cancel_panicked = self.cancel_exchange(grace).is_err();
        self.stop = Some(reason);
        self.stop_limit = Some(if cancel_panicked {
            now
        } else {
            now.checked_add(grace)
                .and_then(|limit| limit.checked_add(STOP_SLACK))
                .unwrap_or(now)
        });
        true
    }

    fn drop_exchange(&mut self) {
        if let Some(exchange) = self.exchange.take() {
            let _ = catch_unwind(AssertUnwindSafe(|| drop(exchange)));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.drop_exchange();
    }
}

pub struct Scheduler {
    next_id: TurnId,
    running: Vec<Running>,
    #[cfg(test)]
    panic_next_poll: bool,
    #[cfg(test)]
    panic_after_events: Option<usize>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub fn new() -> Self {
        Self::with_next_id(1)
    }

    fn with_next_id(next_id: TurnId) -> Self {
        Self {
            next_id,
            running: Vec::new(),
            #[cfg(test)]
            panic_next_poll: false,
            #[cfg(test)]
            panic_after_events: None,
        }
    }

    fn next_id(&self) -> TurnId {
        self.next_id
    }

    pub fn start(
        &mut self,
        exchange: Box<dyn Exchange>,
        timeouts: Option<Timeouts>,
        status_stop_grace: Duration,
    ) -> TurnId {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("turn id space exhausted");
        let now = Instant::now();
        self.running.push(Running {
            id,
            exchange: Some(exchange),
            timeouts,
            stop_grace: timeouts.map_or(status_stop_grace, |limits| limits.stop_grace),
            started_at: now,
            last_work: now,
            started: false,
            stop: None,
            stop_limit: None,
        });
        id
    }

    pub fn contains(&self, id: TurnId) -> bool {
        self.running.iter().any(|running| running.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub fn cancel(&mut self, id: TurnId) -> bool {
        let Some(running) = self.running.iter_mut().find(|running| running.id == id) else {
            return false;
        };
        let grace = running.stop_grace;
        running.begin_stop(StopReason::Cancelled, grace, Instant::now())
    }

    pub fn shutdown(&mut self, grace: Duration) {
        let now = Instant::now();
        for running in &mut self.running {
            let _ = running.begin_stop(StopReason::Cancelled, grace, now);
        }
    }

    pub fn poll(&mut self, slice: Duration) -> Vec<Event> {
        let mut out = Vec::new();
        self.poll_into(slice, &mut out);
        out
    }

    fn poll_into(&mut self, slice: Duration, out: &mut Vec<Event>) {
        #[cfg(test)]
        if std::mem::take(&mut self.panic_next_poll) {
            panic!("injected scheduler panic");
        }
        let mut index = 0;
        while index < self.running.len() {
            let now = Instant::now();
            if let Some(kind) = self.running[index].timeout(now) {
                if self.running[index].stop.is_none() {
                    let grace = self.running[index].stop_grace;
                    let _ = self.running[index].begin_stop(StopReason::Timeout(kind), grace, now);
                }
            }

            let slice_end = now.checked_add(slice).unwrap_or(now);
            let mut finished = None;
            loop {
                let now = Instant::now();
                if self.running[index]
                    .stop_limit
                    .is_some_and(|limit| now >= limit)
                {
                    finished = Some(match self.running[index].stop {
                        Some(StopReason::Timeout(kind)) => EndReason::Timeout(kind),
                        _ => EndReason::Cancelled,
                    });
                    break;
                }

                let update = match self.running[index].next(now) {
                    Ok(Some(update)) => update,
                    Ok(None) => break,
                    Err(()) => {
                        finished = Some(EndReason::AdapterPanicked {
                            maybe_started: true,
                        });
                        break;
                    }
                };

                if self.running[index].stop.is_none() {
                    if Running::recognized_work(&update) {
                        self.running[index].last_work = Instant::now();
                    }
                    if matches!(update, Update::Started) {
                        self.running[index].started = true;
                    }
                }

                if update.is_terminal() {
                    finished = Some(match (self.running[index].stop, update) {
                        (Some(StopReason::Timeout(kind)), _) => EndReason::Timeout(kind),
                        (Some(StopReason::Cancelled), _) => EndReason::Cancelled,
                        (None, Update::Completed) => EndReason::Completed,
                        (None, Update::Failed(error)) => EndReason::Failed(error),
                        (None, Update::Stopped) => EndReason::StoppedUnexpectedly,
                        _ => EndReason::StoppedUnexpectedly,
                    });
                    break;
                }

                if self.running[index].stop.is_none() {
                    out.push(Event::Update {
                        turn_id: self.running[index].id,
                        update,
                    });
                    self.maybe_panic_after_output(out.len());
                }
                if Instant::now() >= slice_end {
                    break;
                }
            }

            if let Some(reason) = finished {
                let mut running = self.running.remove(index);
                let id = running.id;
                running.drop_exchange();
                out.push(Event::Ended {
                    turn_id: id,
                    reason,
                });
                self.maybe_panic_after_output(out.len());
            } else {
                index += 1;
            }
        }
    }

    #[cfg(test)]
    fn maybe_panic_after_output(&mut self, count: usize) {
        if self.panic_after_events == Some(count) {
            self.panic_after_events = None;
            panic!("injected scheduler panic after output");
        }
    }

    #[cfg(not(test))]
    fn maybe_panic_after_output(&mut self, _count: usize) {}

    #[cfg(test)]
    pub fn inject_scheduler_panic_for_test(&mut self) {
        self.panic_next_poll = true;
    }

    #[cfg(test)]
    fn inject_scheduler_panic_after_events_for_test(&mut self, count: usize) {
        self.panic_after_events = Some(count);
    }

    fn take_all_after_panic(&mut self) -> Vec<Event> {
        self.running
            .drain(..)
            .map(|mut running| {
                let id = running.id;
                running.drop_exchange();
                Event::Ended {
                    turn_id: id,
                    reason: EndReason::SchedulerPanicked {
                        maybe_started: true,
                    },
                }
            })
            .collect()
    }
}

pub struct Supervisor {
    scheduler: Scheduler,
    generation: u64,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            scheduler: Scheduler::new(),
            generation: 1,
        }
    }

    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.generation
    }

    pub fn start(
        &mut self,
        exchange: Box<dyn Exchange>,
        timeouts: Option<Timeouts>,
        status_stop_grace: Duration,
    ) -> TurnId {
        self.scheduler.start(exchange, timeouts, status_stop_grace)
    }

    pub fn contains(&self, id: TurnId) -> bool {
        self.scheduler.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.scheduler.is_empty()
    }

    pub fn cancel(&mut self, id: TurnId) -> bool {
        self.scheduler.cancel(id)
    }

    pub fn shutdown(&mut self, grace: Duration) {
        self.scheduler.shutdown(grace);
    }

    pub fn poll(&mut self, slice: Duration) -> Vec<Event> {
        let mut events = Vec::new();
        match catch_unwind(AssertUnwindSafe(|| {
            self.scheduler.poll_into(slice, &mut events);
        })) {
            Ok(()) => events,
            Err(_) => {
                let next_id = self.scheduler.next_id();
                events.extend(self.scheduler.take_all_after_panic());
                self.scheduler = Scheduler::with_next_id(next_id);
                self.generation = self.generation.saturating_add(1);
                events.shrink_to_fit();
                events
            }
        }
    }

    #[cfg(test)]
    pub fn inject_scheduler_panic_for_test(&mut self) {
        self.scheduler.inject_scheduler_panic_for_test();
    }

    #[cfg(test)]
    fn inject_scheduler_panic_after_events_for_test(&mut self, count: usize) {
        self.scheduler
            .inject_scheduler_panic_after_events_for_test(count);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct Scripted {
        updates: VecDeque<Update>,
        panic_next: bool,
    }

    impl Scripted {
        fn new(updates: impl IntoIterator<Item = Update>) -> Self {
            Self {
                updates: updates.into_iter().collect(),
                panic_next: false,
            }
        }

        fn panics() -> Self {
            Self {
                updates: VecDeque::new(),
                panic_next: true,
            }
        }
    }

    impl Exchange for Scripted {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            if std::mem::take(&mut self.panic_next) {
                panic!("adapter panic");
            }
            self.updates.pop_front()
        }

        fn cancel(&mut self, _grace: Duration) {
            self.updates = VecDeque::from([Update::Stopped]);
        }
    }

    struct TalksAfterCancel {
        started: bool,
        cancelled: bool,
        talked: bool,
    }

    impl Exchange for TalksAfterCancel {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            if !self.started {
                self.started = true;
                return Some(Update::Started);
            }
            if self.cancelled && !self.talked {
                self.talked = true;
                return Some(Update::Delta("late".to_owned()));
            }
            None
        }

        fn cancel(&mut self, _grace: Duration) {
            self.cancelled = true;
        }
    }

    fn limits() -> Timeouts {
        Timeouts {
            start: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            max_turn: Duration::from_secs(2),
            stop_grace: Duration::ZERO,
        }
    }

    #[test]
    fn ids_never_repeat_within_a_scheduler_lifetime() {
        let mut scheduler = Scheduler::new();
        let a = scheduler.start(
            Box::new(Scripted::new([Update::Completed])),
            Some(limits()),
            Duration::ZERO,
        );
        let _ = scheduler.poll(Duration::from_millis(1));
        let b = scheduler.start(
            Box::new(Scripted::new([Update::Completed])),
            Some(limits()),
            Duration::ZERO,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn an_adapter_panic_ends_only_its_turn() {
        let mut scheduler = Scheduler::new();
        let bad = scheduler.start(Box::new(Scripted::panics()), Some(limits()), Duration::ZERO);
        let good = scheduler.start(
            Box::new(Scripted::new([Update::Started, Update::Completed])),
            Some(limits()),
            Duration::ZERO,
        );
        let events = scheduler.poll(Duration::from_millis(1));
        assert!(events.iter().any(|event| matches!(event, Event::Ended { turn_id, reason: EndReason::AdapterPanicked { .. } } if *turn_id == bad)));
        assert!(events.iter().any(|event| matches!(event, Event::Ended { turn_id, reason: EndReason::Completed } if *turn_id == good)));
    }

    #[test]
    fn a_scheduler_panic_ends_every_owned_turn_and_recovers() {
        let mut supervisor = Supervisor::new();
        let first = supervisor.start(Box::new(Scripted::new([])), Some(limits()), Duration::ZERO);
        supervisor.inject_scheduler_panic_for_test();
        let events = supervisor.poll(Duration::ZERO);
        assert!(events.iter().any(|event| matches!(event, Event::Ended { turn_id, reason: EndReason::SchedulerPanicked { .. } } if *turn_id == first)));
        assert_eq!(supervisor.generation(), 2);

        let second = supervisor.start(
            Box::new(Scripted::new([Update::Completed])),
            Some(limits()),
            Duration::ZERO,
        );
        assert_ne!(first, second);
        assert!(!supervisor.cancel(first));
        assert!(supervisor.poll(Duration::ZERO).iter().any(|event| matches!(event, Event::Ended { turn_id, reason: EndReason::Completed } if *turn_id == second)));
    }
    #[test]
    fn scheduler_panic_preserves_events_already_produced_in_that_poll() {
        let mut supervisor = Supervisor::new();
        let finished = supervisor.start(
            Box::new(Scripted::new([Update::Completed])),
            Some(limits()),
            Duration::ZERO,
        );
        let still_running =
            supervisor.start(Box::new(Scripted::new([])), Some(limits()), Duration::ZERO);
        supervisor.inject_scheduler_panic_after_events_for_test(1);

        let events = supervisor.poll(Duration::ZERO);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Ended { turn_id, reason: EndReason::Completed } if *turn_id == finished
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Ended { turn_id, reason: EndReason::SchedulerPanicked { .. } }
                if *turn_id == still_running
        )));
    }

    #[test]
    fn timeout_suppresses_late_updates_and_cannot_be_replaced_by_cancel() {
        let mut scheduler = Scheduler::new();
        let id = scheduler.start(
            Box::new(TalksAfterCancel {
                started: false,
                cancelled: false,
                talked: false,
            }),
            Some(Timeouts {
                start: Duration::from_secs(1),
                idle: Duration::ZERO,
                max_turn: Duration::from_secs(2),
                stop_grace: Duration::from_secs(1),
            }),
            Duration::ZERO,
        );
        let first = scheduler.poll(Duration::ZERO);
        assert!(first.iter().any(|event| matches!(
            event,
            Event::Update {
                turn_id,
                update: Update::Started
            } if *turn_id == id
        )));

        let timed_out = scheduler.poll(Duration::from_millis(1));
        assert!(
            !timed_out.iter().any(|event| matches!(
                event,
                Event::Update {
                    update: Update::Delta(_),
                    ..
                }
            )),
            "{timed_out:?}"
        );
        assert!(!scheduler.cancel(id));
    }
}
