//! The slow-consumer policy: what a queue of updates may hold for a consumer
//! that reads at its own pace, when the producer must never wait for it.
//!
//! The producer here is a thread that serves other work too (the service's
//! polling thread, the remote client's IPC task), so it cannot block on one
//! consumer, and the answer text must not be dropped to make room. The policy
//! that satisfies both:
//!
//! - an update is queued while less than the bound is unread, so a single
//!   large message that crosses the bound is delivered whole;
//! - an update that arrives with the bound already passed is **not** queued:
//!   the turn must be stopped, what is queued is still delivered whole and in
//!   order, and the turn ends once with [`CONSUMER_TOO_SLOW`], which tells the
//!   consumer its answer is incomplete. Output produced after that decision is
//!   discarded, which is why the ending is a failure and never a completion;
//! - `Update::Activity` says only that the provider is working, so at most one
//!   is queued until it is read, and it takes no room;
//! - a terminal update is never counted or refused.
//!
//! The memory a queue holds is therefore at most the bound, plus the one
//! update that crossed it, plus a progress update and the ending.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::exchange::Update;
use crate::protocol::{ErrorCode, Failure};

/// How much output a turn may have queued for a consumer that has not read it:
/// far more than any answer, so only a consumer that has stopped reading, or a
/// provider that floods, reaches it.
pub const MAX_UNREAD_BYTES: usize = 4 * 1024 * 1024;

/// The `reason` a turn ends with when its consumer fell more than the bound
/// behind. The text it was given is a complete prefix of the answer; the rest
/// was not kept.
pub const CONSUMER_TOO_SLOW: &str = "CONSUMER_TOO_SLOW";

/// Counted against every queued update on top of its text, so a provider that
/// sends millions of tiny updates is bounded too.
const EVENT_OVERHEAD: usize = 64;

/// The failure a turn ends with when its consumer fell too far behind.
pub fn too_slow() -> Failure {
    Failure {
        code: ErrorCode::InternalError,
        reason: CONSUMER_TOO_SLOW,
        retryable: false,
    }
}

/// What an update costs to keep queued: its text and fields, and a fixed
/// amount for the update itself.
pub fn weight(update: &Update) -> usize {
    EVENT_OVERHEAD
        + match update {
            Update::Delta(text) | Update::Session(text) => text.len(),
            Update::Source(source) => {
                source.id.len()
                    + source.backend_id.len()
                    + source.title.len()
                    + source.url.len()
                    + source.snippet.len()
                    + source.source_name.as_ref().map_or(0, String::len)
                    + source.age.as_ref().map_or(0, String::len)
            }
            Update::Status { provider_id, .. } => provider_id.len(),
            _ => 0,
        }
}

/// What the producer does with one update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Queue it.
    Queue,
    /// It says nothing the queued progress update does not: drop it.
    Skip,
    /// The consumer is too far behind: stop the turn, and queue nothing more.
    Overflow,
}

/// What one queue holds that its consumer has not read, shared by both sides.
/// One producer calls [`Backlog::admit`] before it queues an update; the
/// consumer calls [`Backlog::read`] after it takes one.
#[derive(Debug, Default)]
pub struct Backlog {
    activity_queued: AtomicBool,
    unread: AtomicUsize,
}

impl Backlog {
    /// Decides whether `update` may be queued, and counts it if so. The bound
    /// is at least 1, so the first update of a queue always gets in.
    pub fn admit(&self, update: &Update, max_unread_bytes: usize) -> Admission {
        if update.is_terminal() {
            return Admission::Queue;
        }
        if matches!(update, Update::Activity) {
            return if self.activity_queued.swap(true, Ordering::SeqCst) {
                Admission::Skip
            } else {
                Admission::Queue
            };
        }
        if self.unread.load(Ordering::SeqCst) >= max_unread_bytes.max(1) {
            return Admission::Overflow;
        }
        self.unread.fetch_add(weight(update), Ordering::SeqCst);
        Admission::Queue
    }

    /// Makes room for an update the consumer has taken from the queue.
    pub fn read(&self, update: &Update) {
        if update.is_terminal() {
            return;
        }
        if matches!(update, Update::Activity) {
            self.activity_queued.store(false, Ordering::SeqCst);
        } else {
            self.unread.fetch_sub(weight(update), Ordering::SeqCst);
        }
    }

    /// The size of what is queued and unread, not counting progress.
    pub fn unread(&self) -> usize {
        self.unread.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: usize) -> Update {
        Update::Delta("a".repeat(bytes))
    }

    #[test]
    fn an_update_is_queued_while_the_bound_is_not_passed_and_the_one_that_crosses_it_is_too() {
        let backlog = Backlog::default();
        // Each costs its text and a fixed amount: two leave 128 under 1,000.
        for _ in 0..3 {
            assert_eq!(backlog.admit(&text(400), 1_000), Admission::Queue);
        }
        assert_eq!(backlog.unread(), 3 * (400 + EVENT_OVERHEAD));
        // The bound is passed, so nothing more gets in, however small.
        assert_eq!(backlog.admit(&text(400), 1_000), Admission::Overflow);
        assert_eq!(backlog.admit(&text(0), 1_000), Admission::Overflow);
        assert_eq!(backlog.unread(), 3 * (400 + EVENT_OVERHEAD));
    }

    #[test]
    fn one_update_bigger_than_the_bound_is_queued_whole_and_reading_makes_room() {
        let backlog = Backlog::default();
        let big = text(2_000);
        assert_eq!(backlog.admit(&big, 500), Admission::Queue);
        assert_eq!(backlog.admit(&text(1), 500), Admission::Overflow);
        backlog.read(&big);
        assert_eq!(backlog.unread(), 0);
        assert_eq!(backlog.admit(&text(1), 500), Admission::Queue);
    }

    #[test]
    fn a_bound_of_nothing_still_lets_the_first_update_in() {
        let backlog = Backlog::default();
        assert_eq!(backlog.admit(&text(1), 0), Admission::Queue);
        assert_eq!(backlog.admit(&text(1), 0), Admission::Overflow);
    }

    #[test]
    fn many_tiny_updates_are_bounded_too() {
        let backlog = Backlog::default();
        for _ in 0..10 {
            assert_eq!(
                backlog.admit(&text(0), 10 * EVENT_OVERHEAD),
                Admission::Queue
            );
        }
        assert_eq!(
            backlog.admit(&text(0), 10 * EVENT_OVERHEAD),
            Admission::Overflow
        );
    }

    #[test]
    fn progress_takes_no_room_and_at_most_one_is_queued_until_it_is_read() {
        let backlog = Backlog::default();
        assert_eq!(backlog.admit(&Update::Activity, 1), Admission::Queue);
        assert_eq!(backlog.admit(&Update::Activity, 1), Admission::Skip);
        assert_eq!(backlog.unread(), 0);
        backlog.read(&Update::Activity);
        assert_eq!(backlog.admit(&Update::Activity, 1), Admission::Queue);
        // Progress is still said while the text is over its bound.
        let full = Backlog::default();
        full.admit(&text(10), 1);
        assert_eq!(full.admit(&text(1), 1), Admission::Overflow);
        assert_eq!(full.admit(&Update::Activity, 1), Admission::Queue);
    }

    #[test]
    fn a_terminal_update_is_never_counted_or_refused() {
        let backlog = Backlog::default();
        backlog.admit(&text(10), 1);
        assert_eq!(backlog.admit(&Update::Completed, 1), Admission::Queue);
        assert_eq!(backlog.admit(&Update::Stopped, 1), Admission::Queue);
        let before = backlog.unread();
        backlog.read(&Update::Completed);
        assert_eq!(backlog.unread(), before);
    }

    #[test]
    fn what_is_queued_is_counted_by_its_text_and_fields() {
        assert_eq!(weight(&Update::Started), EVENT_OVERHEAD);
        assert_eq!(weight(&text(10)), EVENT_OVERHEAD + 10);
        assert_eq!(
            weight(&Update::Session("handle".to_owned())),
            EVENT_OVERHEAD + 6
        );
    }

    #[test]
    fn the_failure_is_not_retryable_and_names_its_reason() {
        let failure = too_slow();
        assert_eq!(failure.reason, CONSUMER_TOO_SLOW);
        assert!(!failure.retryable);
    }
}
