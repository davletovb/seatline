//! Fair, bounded scheduling for provider exchanges.
//!
//! The scheduler owns provider exchanges and their lifecycle limits. It has no
//! browser or Native Messaging concepts. The supervisor is the panic boundary
//! shared by synchronous and service-thread entry points.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use seatline_core::exchange::{Exchange, Timeouts, Update};
use seatline_core::protocol::Failure;
use seatline_core::telemetry::Timeline;

const STOP_SLACK: Duration = Duration::from_secs(1);

/// Timelines of ended turns kept for the host to take. A host that starts timed
/// turns takes each one when its turn ends; one that never does loses the
/// oldest past this many instead of growing without bound.
const MAX_FINISHED: usize = 4096;

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
    /// Phase marks, kept only when the host asked for them.
    timeline: Option<Timeline>,
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

    /// Stamps what `update` shows. A stopped turn's updates are suppressed, so
    /// only its terminal one counts.
    fn observe(&mut self, update: &Update) {
        let stopped = self.stop.is_some();
        if let Some(timeline) = self.timeline.as_mut() {
            // The clock is read only for an update that sets a mark.
            if (!stopped || update.is_terminal()) && timeline.wants(update) {
                timeline.observe(update, Instant::now());
            }
        }
    }

    /// Closes the timeline of a turn that is being removed: its terminal mark
    /// if it never saw one, the probe its exchange measured, and the time its
    /// exchange took to drop, which is when its process is killed and reaped.
    fn finish_timeline(&mut self) -> Option<Timeline> {
        let mut timeline = self.timeline.take()?;
        timeline.terminal(Instant::now());
        // Like every other call into an adapter, behind the panic boundary: this
        // runs after the turn has left `running`, so a panic escaping here
        // would leave nothing to report the turn's end. Telemetry is an extra,
        // so a probe report that panics is simply dropped.
        let probe = self.exchange.as_ref().and_then(|exchange| {
            catch_unwind(AssertUnwindSafe(|| exchange.probe_span()))
                .ok()
                .flatten()
        });
        if let Some(span) = probe {
            timeline.set_probe(span);
        }
        self.drop_exchange();
        timeline.released(Instant::now());
        Some(timeline)
    }

    fn begin_stop(&mut self, reason: StopReason, grace: Duration, now: Instant) -> bool {
        if self.stop.is_some() {
            return false;
        }
        if let Some(timeline) = self.timeline.as_mut() {
            timeline.stop_requested(now);
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
    /// Timelines of turns that ended, until the host takes them (at most
    /// [`MAX_FINISHED`]).
    finished: BTreeMap<TurnId, Timeline>,
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
            finished: BTreeMap::new(),
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
        self.start_with(exchange, timeouts, status_stop_grace, None)
    }

    /// [`Scheduler::start`] that also stamps `timeline` with what the turn
    /// does. Take the finished timeline with [`Scheduler::take_timeline`] once
    /// the turn has ended.
    pub fn start_timed(
        &mut self,
        exchange: Box<dyn Exchange>,
        timeouts: Option<Timeouts>,
        status_stop_grace: Duration,
        timeline: Timeline,
    ) -> TurnId {
        self.start_with(exchange, timeouts, status_stop_grace, Some(timeline))
    }

    fn start_with(
        &mut self,
        exchange: Box<dyn Exchange>,
        timeouts: Option<Timeouts>,
        status_stop_grace: Duration,
        timeline: Option<Timeline>,
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
            timeline,
        });
        id
    }

    /// The timeline of a timed turn that has ended, once. A turn that was not
    /// started with a timeline, or whose timeline was taken, has none.
    pub fn take_timeline(&mut self, id: TurnId) -> Option<Timeline> {
        self.finished.remove(&id)
    }

    /// Keeps an ended turn's timeline for its host, dropping the oldest one a
    /// host has left untaken once there are too many.
    fn keep(&mut self, id: TurnId, timeline: Timeline) {
        self.finished.insert(id, timeline);
        while self.finished.len() > MAX_FINISHED {
            self.finished.pop_first();
        }
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

                self.running[index].observe(&update);
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
                if let Some(timeline) = running.finish_timeline() {
                    self.keep(id, timeline);
                }
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
        let mut running = std::mem::take(&mut self.running);
        running
            .drain(..)
            .map(|mut running| {
                let id = running.id;
                if let Some(timeline) = running.finish_timeline() {
                    self.finished.insert(id, timeline);
                }
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

    /// [`Supervisor::start`] that also stamps `timeline`; see
    /// [`Scheduler::start_timed`].
    pub fn start_timed(
        &mut self,
        exchange: Box<dyn Exchange>,
        timeouts: Option<Timeouts>,
        status_stop_grace: Duration,
        timeline: Timeline,
    ) -> TurnId {
        self.scheduler
            .start_timed(exchange, timeouts, status_stop_grace, timeline)
    }

    /// The timeline of a timed turn that has ended, once.
    pub fn take_timeline(&mut self, id: TurnId) -> Option<Timeline> {
        self.scheduler.take_timeline(id)
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
                // Timelines the host has not taken yet outlive the scheduler.
                let finished = std::mem::take(&mut self.scheduler.finished);
                self.scheduler = Scheduler::with_next_id(next_id);
                self.scheduler.finished = finished;
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

    use seatline_core::telemetry::{Kind, Span};

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

    fn timeline() -> Timeline {
        let mut timeline = Timeline::new(Kind::Send, Instant::now());
        timeline.admitted(Instant::now());
        timeline
    }

    /// The marks of a timeline in the order they must have been taken.
    fn in_order(timeline: &Timeline) -> Vec<(&'static str, u64)> {
        let marks = timeline.marks();
        [
            ("admitted", marks.admitted),
            ("launched", marks.launched),
            ("started", marks.started),
            ("first_text", marks.first_text),
            ("terminal", marks.terminal),
            ("released", marks.released),
        ]
        .into_iter()
        .filter_map(|(name, at)| Some((name, at?)))
        .collect()
    }

    fn assert_ordered(timeline: &Timeline) {
        let marks = in_order(timeline);
        assert!(
            marks.windows(2).all(|pair| pair[0].1 <= pair[1].1),
            "marks out of order: {marks:?}"
        );
    }

    #[test]
    fn a_timed_turn_reports_its_boundaries_in_order() {
        let mut scheduler = Scheduler::new();
        let id = scheduler.start_timed(
            Box::new(Scripted::new([
                Update::Launched,
                Update::Started,
                Update::Activity,
                Update::Delta("answer".to_owned()),
                Update::Completed,
            ])),
            Some(limits()),
            Duration::ZERO,
            timeline(),
        );
        let events = scheduler.poll(Duration::from_millis(1));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Ended { turn_id, reason: EndReason::Completed } if *turn_id == id
        )));
        let timeline = scheduler.take_timeline(id).expect("a timeline");
        let names: Vec<_> = in_order(&timeline)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "admitted",
                "launched",
                "started",
                "first_text",
                "terminal",
                "released"
            ]
        );
        assert_ordered(&timeline);
        assert_eq!(timeline.phases().sum(), timeline.total_us());
        assert!(
            scheduler.take_timeline(id).is_none(),
            "a timeline is taken once"
        );
    }

    #[test]
    fn timelines_a_host_never_takes_are_bounded() {
        let mut scheduler = Scheduler::new();
        let mut first = None;
        for _ in 0..MAX_FINISHED + 50 {
            let id = scheduler.start_timed(
                Box::new(Scripted::new([Update::Completed])),
                Some(limits()),
                Duration::ZERO,
                timeline(),
            );
            first.get_or_insert(id);
            let _ = scheduler.poll(Duration::ZERO);
        }
        assert_eq!(scheduler.finished.len(), MAX_FINISHED);
        assert!(
            scheduler.take_timeline(first.unwrap()).is_none(),
            "the oldest was the one dropped"
        );
    }

    #[test]
    fn a_turn_started_without_a_timeline_keeps_nothing() {
        let mut scheduler = Scheduler::new();
        let id = scheduler.start(
            Box::new(Scripted::new([Update::Started, Update::Completed])),
            Some(limits()),
            Duration::ZERO,
        );
        let _ = scheduler.poll(Duration::from_millis(1));
        assert!(scheduler.take_timeline(id).is_none());
        assert!(scheduler.finished.is_empty());
    }

    #[test]
    fn a_cancel_is_marked_and_late_text_is_not_a_boundary() {
        let mut scheduler = Scheduler::new();
        let id = scheduler.start_timed(
            Box::new(TalksAfterCancel {
                started: false,
                cancelled: false,
                talked: false,
            }),
            Some(limits()),
            Duration::from_secs(1),
            timeline(),
        );
        let _ = scheduler.poll(Duration::ZERO);
        assert!(scheduler.cancel(id));
        // The exchange never ends by itself, so the stop limit ends it.
        let give_up = Instant::now() + Duration::from_secs(10);
        let mut ended = false;
        while !ended && Instant::now() < give_up {
            ended = scheduler
                .poll(Duration::from_millis(5))
                .iter()
                .any(|event| {
                    matches!(
                        event,
                        Event::Ended {
                            reason: EndReason::Cancelled,
                            ..
                        }
                    )
                });
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ended, "the cancelled turn never ended");
        let timeline = scheduler.take_timeline(id).expect("a timeline");
        let marks = timeline.marks();
        assert!(marks.started.is_some());
        assert!(marks.stop_requested.is_some());
        assert_eq!(marks.first_text, None, "the late delta was suppressed");
        assert!(marks.terminal >= marks.stop_requested);
        assert_ordered(&timeline);
    }

    #[test]
    fn a_timeout_is_marked_as_a_stop() {
        let mut scheduler = Scheduler::new();
        let id = scheduler.start_timed(
            Box::new(Scripted::new([Update::Started])),
            Some(Timeouts {
                start: Duration::from_secs(1),
                idle: Duration::ZERO,
                max_turn: Duration::from_secs(2),
                stop_grace: Duration::ZERO,
            }),
            Duration::ZERO,
            timeline(),
        );
        let mut reason = None;
        while reason.is_none() {
            for event in scheduler.poll(Duration::from_millis(1)) {
                if let Event::Ended { reason: ended, .. } = event {
                    reason = Some(ended);
                }
            }
        }
        assert_eq!(reason, Some(EndReason::Timeout(TimeoutKind::Idle)));
        let timeline = scheduler.take_timeline(id).expect("a timeline");
        assert!(timeline.marks().stop_requested.is_some());
        assert!(timeline.has_terminal());
        assert_ordered(&timeline);
    }

    /// Reports the probe it says it ran.
    struct Probed(VecDeque<Update>, Span);

    impl Exchange for Probed {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            self.0.pop_front()
        }

        fn cancel(&mut self, _grace: Duration) {}

        fn probe_span(&self) -> Option<Span> {
            Some(self.1)
        }
    }

    #[test]
    fn the_probe_an_exchange_measured_reaches_the_timeline() {
        let mut scheduler = Scheduler::new();
        let mut probe = Span::begin(Instant::now());
        probe.finish(Instant::now());
        let id = scheduler.start_timed(
            Box::new(Probed(
                VecDeque::from([Update::Launched, Update::Started, Update::Completed]),
                probe,
            )),
            Some(limits()),
            Duration::ZERO,
            timeline(),
        );
        let _ = scheduler.poll(Duration::from_millis(1));
        let timeline = scheduler.take_timeline(id).expect("a timeline");
        let marks = timeline.marks();
        assert!(marks.probe_started.is_some() && marks.probe_ended.is_some());
        assert_eq!(timeline.probes(), 1);
        assert_eq!(timeline.phases().sum(), timeline.total_us());
    }

    /// An exchange that finishes normally but panics when asked for its probe.
    struct PanicsReportingItsProbe(VecDeque<Update>);

    impl Exchange for PanicsReportingItsProbe {
        fn next(&mut self, _deadline: Instant) -> Option<Update> {
            self.0.pop_front()
        }

        fn cancel(&mut self, _grace: Duration) {}

        fn probe_span(&self) -> Option<Span> {
            panic!("a broken probe report")
        }
    }

    #[test]
    fn an_adapter_that_panics_reporting_its_probe_still_ends_its_turn_normally() {
        let mut supervisor = Supervisor::new();
        let id = supervisor.start_timed(
            Box::new(PanicsReportingItsProbe(VecDeque::from([
                Update::Started,
                Update::Completed,
            ]))),
            Some(limits()),
            Duration::ZERO,
            timeline(),
        );
        let events = supervisor.poll(Duration::from_millis(1));
        // Telemetry is an extra: the turn ends the way it would have without it,
        // and is not mistaken for a scheduler panic that strands its slot.
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Ended { turn_id, reason: EndReason::Completed } if *turn_id == id
            )),
            "{events:?}"
        );
        assert!(!supervisor.contains(id));
        let timeline = supervisor.take_timeline(id).expect("a timeline");
        assert_eq!(timeline.probes(), 0, "the broken report is dropped");
        assert!(timeline.marks().released.is_some());
    }

    #[test]
    fn an_adapter_panic_still_leaves_a_closed_timeline() {
        let mut scheduler = Scheduler::new();
        let id = scheduler.start_timed(
            Box::new(Scripted::panics()),
            Some(limits()),
            Duration::ZERO,
            timeline(),
        );
        let _ = scheduler.poll(Duration::from_millis(1));
        let timeline = scheduler.take_timeline(id).expect("a timeline");
        let marks = timeline.marks();
        assert!(marks.terminal.is_some() && marks.released.is_some());
        assert_ordered(&timeline);
    }

    #[test]
    fn a_timeline_survives_a_scheduler_panic() {
        let mut supervisor = Supervisor::new();
        let running = supervisor.start_timed(
            Box::new(Scripted::new([Update::Started])),
            Some(limits()),
            Duration::ZERO,
            timeline(),
        );
        supervisor.inject_scheduler_panic_for_test();
        let events = supervisor.poll(Duration::ZERO);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Ended { turn_id, reason: EndReason::SchedulerPanicked { .. } }
                if *turn_id == running
        )));
        let timeline = supervisor.take_timeline(running).expect("a timeline");
        assert!(timeline.marks().released.is_some());
        // The next generation can still time a turn.
        let next = supervisor.start_timed(
            Box::new(Scripted::new([Update::Completed])),
            Some(limits()),
            Duration::ZERO,
            self::timeline(),
        );
        let _ = supervisor.poll(Duration::ZERO);
        assert!(supervisor.take_timeline(next).is_some());
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
