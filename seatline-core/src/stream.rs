//! Native stream manager (NAT-05).
//!
//! [`LineStream`] turns a provider process's raw output into complete stdout
//! lines under a bounded memory policy:
//!
//! - output arrives in chunks cut wherever a read ended, even inside a
//!   multi-byte UTF-8 character; lines are reassembled across any number of
//!   chunks, and each is checked to be UTF-8 only once it is complete;
//! - a line longer than the stream's limit, not counting its line ending,
//!   ends the stream with an error instead of growing the host's memory;
//! - stderr is counted and discarded: it is meant for people, and it can hold
//!   secrets such as masked keys. An adapter may opt in to keeping a small,
//!   bounded tail of it ([`LineStream::keeping_stderr_tail`]) to classify how
//!   the process failed; that tail is never logged or forwarded;
//! - the stream ends in exactly one terminal state: [`Output::Final`] after a
//!   clean end, [`Output::Error`] when the output broke the policy, or
//!   [`Output::Stopped`] after [`LineStream::cancel`];
//! - cancelling discards everything not yet delivered at once, asks the
//!   process to stop, and kills it once the grace period runs out;
//! - output that keeps coming without completing a line, such as a stderr
//!   flood, holds a caller at most [`BUSY_LIMIT`] past its deadline.
//!
//! [`split_text`] cuts outgoing text into bounded pieces without splitting a
//! character.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::process::{Event, Exit, Process, Reaper};

/// How long past its deadline a call keeps consuming output that arrives
/// without giving it anything to return. A provider that never stops writing,
/// to stderr or in lines nobody acts on, can't hold its caller longer than
/// this, so a loop serving several streams gets back to the others.
pub const BUSY_LIMIT: Duration = Duration::from_millis(5);

/// Why a stream failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    /// A line grew past the stream's limit.
    LineTooLong,
    /// A complete line was not valid UTF-8.
    InvalidUtf8,
}

/// Something a [`LineStream`] delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// One complete stdout line, without its line ending.
    Line(String),
    /// Terminal: stdout ended and the process exited.
    Final(Exit),
    /// Terminal: the output broke the stream's policy, and the process was
    /// killed.
    Error(StreamError),
    /// Terminal: the stream was cancelled, and the process has exited.
    Stopped(Exit),
}

/// Splits a byte stream into lines of at most `max_line_bytes`, not counting
/// their line endings. It holds at most that much of an unfinished line, plus
/// a `\r` that a `\n` may still turn into its line ending.
#[derive(Debug)]
pub struct LineSplitter {
    pending: Vec<u8>,
    max_line_bytes: usize,
}

impl LineSplitter {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_line_bytes,
        }
    }

    /// Adds `bytes` and appends each line they complete to `lines`. A line ends
    /// at `\n`; a `\r` just before it is dropped too.
    pub fn push(&mut self, bytes: &[u8], lines: &mut VecDeque<String>) -> Result<(), StreamError> {
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&byte| byte == b'\n') {
            let content = &rest[..end];
            // The `\r` before this `\n` may have come in an earlier chunk.
            let carriage_return = content.last().or(self.pending.last()) == Some(&b'\r');
            let length = self.pending.len() + content.len() - usize::from(carriage_return);
            if length > self.max_line_bytes {
                return Err(StreamError::LineTooLong);
            }
            self.pending.extend_from_slice(content);
            if carriage_return {
                self.pending.pop();
            }
            rest = &rest[end + 1..];
            let line = String::from_utf8(std::mem::take(&mut self.pending))
                .map_err(|_| StreamError::InvalidUtf8)?;
            lines.push_back(line);
        }
        // A `\r` at the end may yet be dropped, so it may pass the limit.
        let carriage_return = rest.last().or(self.pending.last()) == Some(&b'\r');
        let limit = self
            .max_line_bytes
            .saturating_add(usize::from(carriage_return));
        if self.pending.len() + rest.len() > limit {
            return Err(StreamError::LineTooLong);
        }
        self.pending.extend_from_slice(rest);
        Ok(())
    }

    /// Ends the input and returns its last line if that had no line ending.
    /// Without a `\n`, a `\r` at its end is part of the line.
    pub fn finish(&mut self) -> Result<Option<String>, StreamError> {
        let line = std::mem::take(&mut self.pending);
        if line.is_empty() {
            return Ok(None);
        }
        if line.len() > self.max_line_bytes {
            return Err(StreamError::LineTooLong);
        }
        String::from_utf8(line)
            .map(Some)
            .map_err(|_| StreamError::InvalidUtf8)
    }

    /// Bytes of the unfinished line held so far.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Splits `text` into pieces of at most `max_bytes` bytes, never inside a
/// character. `max_bytes` below 4, the longest UTF-8 character, is raised to 4.
pub fn split_text(text: &str, max_bytes: usize) -> impl Iterator<Item = &str> {
    let max_bytes = max_bytes.max(4);
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(max_bytes);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, tail) = rest.split_at(end);
        rest = tail;
        Some(piece)
    })
}

/// A provider process's stdout, delivered line by line. See the module
/// documentation.
pub struct LineStream {
    process: Process,
    lines: LineSplitter,
    ready: VecDeque<String>,
    stderr_bytes: u64,
    /// The last bytes of stderr, when an adapter asked to keep them.
    stderr_tail: Vec<u8>,
    stderr_tail_limit: usize,
    state: State,
}

enum State {
    Reading,
    /// The process exited; the lines still queued come before `Final`.
    Draining(Exit),
    /// Cancelled; the process is killed at `kill_at` if it is still running.
    Stopping {
        kill_at: Instant,
    },
    Done(Output),
}

impl LineStream {
    /// Reads `process`, allowing lines of up to `max_line_bytes`. Write the
    /// process's input before handing it over.
    pub fn new(process: Process, max_line_bytes: usize) -> Self {
        Self {
            process,
            lines: LineSplitter::new(max_line_bytes),
            ready: VecDeque::new(),
            stderr_bytes: 0,
            stderr_tail: Vec::new(),
            stderr_tail_limit: 0,
            state: State::Reading,
        }
    }

    /// Keeps the last `limit` bytes of stderr, for [`LineStream::stderr_tail`].
    #[must_use]
    pub fn keeping_stderr_tail(mut self, limit: usize) -> Self {
        self.stderr_tail_limit = limit;
        self
    }

    /// Returns the next line or terminal state, waiting until `deadline` at
    /// most. Returns `None` if the deadline passes first, or once output has
    /// kept arriving for [`BUSY_LIMIT`] past it without completing a line.
    /// Once a terminal state is returned, every later call returns it again.
    pub fn next(&mut self, deadline: Instant) -> Option<Output> {
        let busy_until = deadline.max(after(BUSY_LIMIT));
        loop {
            match &self.state {
                State::Done(output) => return Some(output.clone()),
                State::Draining(exit) => {
                    let exit = *exit;
                    return Some(match self.ready.pop_front() {
                        Some(line) => Output::Line(line),
                        None => self.end(Output::Final(exit)),
                    });
                }
                State::Stopping { kill_at } => {
                    let kill_at = *kill_at;
                    // Checked first, so output that keeps coming can't put
                    // the kill off.
                    if Instant::now() >= kill_at {
                        let exit = self.process.kill();
                        return Some(self.end(Output::Stopped(exit)));
                    }
                    match self.process.next_event(deadline.min(kill_at)) {
                        Some(Event::Exited(exit)) => return Some(self.end(Output::Stopped(exit))),
                        // Output after a cancel is dropped.
                        Some(_) => {}
                        None if Instant::now() >= kill_at => continue,
                        None => return None,
                    }
                }
                State::Reading => {
                    if let Some(line) = self.ready.pop_front() {
                        return Some(Output::Line(line));
                    }
                    match self.process.next_event(deadline)? {
                        Event::Stdout(bytes) => {
                            if let Err(error) = self.lines.push(&bytes, &mut self.ready) {
                                return Some(self.fail(error));
                            }
                        }
                        Event::Stderr(bytes) => {
                            self.stderr_bytes = self
                                .stderr_bytes
                                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                            self.keep_stderr(&bytes);
                        }
                        Event::Exited(exit) => match self.lines.finish() {
                            Ok(last) => {
                                self.ready.extend(last);
                                self.state = State::Draining(exit);
                            }
                            Err(error) => return Some(self.fail(error)),
                        },
                    }
                }
            }
            if self.ready.is_empty() && Instant::now() >= busy_until {
                return None;
            }
        }
    }

    /// Stops the stream: output not yet delivered is dropped, no further line
    /// is delivered, and the process is asked to stop and killed if it is still
    /// running after `grace`. Keep calling [`LineStream::next`] until it
    /// returns [`Output::Stopped`].
    pub fn cancel(&mut self, grace: Duration) {
        if matches!(self.state, State::Done(_)) {
            return;
        }
        self.ready.clear();
        self.lines = LineSplitter::new(self.lines.max_line_bytes);
        self.state = match self.state {
            State::Draining(exit) => State::Done(Output::Stopped(exit)),
            _ => {
                self.process.request_stop();
                State::Stopping {
                    kill_at: after(grace),
                }
            }
        };
    }

    /// Ends the stream by leaving its process to `reaper`, which waits up to
    /// `exit_grace` for it to exit on its own and then stops it: for a caller
    /// that has all it needs from the process and would only wait for it to
    /// leave. What the process writes from now on is discarded.
    ///
    /// Gives the stream back, with nothing lost, if the process has already
    /// exited or been told to stop, if lines it wrote are still waiting to be
    /// delivered, or if the reaper cannot take another process. (Boxed, as the
    /// way back is the rare one.)
    pub fn release(
        self,
        reaper: &Reaper,
        exit_grace: Duration,
        stop_grace: Duration,
    ) -> Result<(), Box<Self>> {
        if !matches!(self.state, State::Reading) || !self.ready.is_empty() {
            return Err(Box::new(self));
        }
        let Self {
            process,
            lines,
            ready,
            stderr_bytes,
            stderr_tail,
            stderr_tail_limit,
            state,
        } = self;
        reaper
            .release(process, exit_grace, stop_grace)
            .map_err(|process| {
                Box::new(Self {
                    process: *process,
                    lines,
                    ready,
                    stderr_bytes,
                    stderr_tail,
                    stderr_tail_limit,
                    state,
                })
            })
    }

    /// Bytes the process wrote to stderr, all discarded.
    pub fn stderr_bytes(&self) -> u64 {
        self.stderr_bytes
    }

    /// The last bytes of stderr kept by [`LineStream::keeping_stderr_tail`],
    /// possibly starting inside a character. Empty unless asked for.
    pub fn stderr_tail(&self) -> &[u8] {
        &self.stderr_tail
    }

    fn keep_stderr(&mut self, bytes: &[u8]) {
        if self.stderr_tail_limit == 0 {
            return;
        }
        let bytes = &bytes[bytes.len().saturating_sub(self.stderr_tail_limit)..];
        let excess = (self.stderr_tail.len() + bytes.len()).saturating_sub(self.stderr_tail_limit);
        self.stderr_tail.drain(..excess);
        self.stderr_tail.extend_from_slice(bytes);
    }

    /// Bytes of output held in the host: the unfinished line plus complete
    /// lines not yet delivered.
    pub fn buffered_bytes(&self) -> usize {
        self.lines.pending_len() + self.ready.iter().map(String::len).sum::<usize>()
    }

    /// Kills the process and ends the stream with `error`.
    fn fail(&mut self, error: StreamError) -> Output {
        self.ready.clear();
        self.process.kill();
        self.end(Output::Error(error))
    }

    fn end(&mut self, output: Output) -> Output {
        self.state = State::Done(output.clone());
        output
    }
}

/// `duration` from now, or now if that can't be represented.
fn after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(splitter: &mut LineSplitter, chunks: &[&[u8]]) -> Result<Vec<String>, StreamError> {
        let mut lines = VecDeque::new();
        for chunk in chunks {
            splitter.push(chunk, &mut lines)?;
        }
        Ok(lines.into())
    }

    #[test]
    fn lines_are_reassembled_across_chunks() {
        let mut splitter = LineSplitter::new(64);
        let lines = push_all(&mut splitter, &[b"al", b"pha\nbe", b"ta\n\ngam", b"ma"]).unwrap();
        assert_eq!(lines, ["alpha", "beta", ""]);
        assert_eq!(splitter.finish().unwrap().as_deref(), Some("gamma"));
        assert_eq!(splitter.finish().unwrap(), None);
    }

    #[test]
    fn a_character_split_between_chunks_is_reassembled() {
        // "é" is C3 A9, "✓" is E2 9C 93, and "😀" is F0 9F 98 80; every cut
        // below falls inside one of them.
        let text = "é✓😀\n".as_bytes();
        for cut in 1..text.len() {
            let mut splitter = LineSplitter::new(64);
            let lines = push_all(&mut splitter, &[&text[..cut], &text[cut..]]).unwrap();
            assert_eq!(lines, ["é✓😀"], "cut at {cut}");
        }
        let mut splitter = LineSplitter::new(64);
        let bytes: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(push_all(&mut splitter, &bytes).unwrap(), ["é✓😀"]);
    }

    #[test]
    fn a_crlf_line_ending_is_removed_whole() {
        let mut splitter = LineSplitter::new(64);
        let lines = push_all(&mut splitter, &[b"one\r", b"\ntwo\r\n", b"\r\r\n"]).unwrap();
        assert_eq!(lines, ["one", "two", "\r"]);
    }

    #[test]
    fn a_line_may_reach_the_limit_but_not_pass_it() {
        let mut splitter = LineSplitter::new(4);
        assert_eq!(push_all(&mut splitter, &[b"abcd\n"]).unwrap(), ["abcd"]);
        assert_eq!(
            push_all(&mut splitter, &[b"abcde\n"]),
            Err(StreamError::LineTooLong)
        );

        // An unfinished line is held to the same limit.
        let mut splitter = LineSplitter::new(4);
        assert_eq!(
            push_all(&mut splitter, &[b"ab", b"cd"]).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(splitter.pending_len(), 4);
        assert_eq!(
            push_all(&mut splitter, &[b"e"]),
            Err(StreamError::LineTooLong)
        );
    }

    #[test]
    fn a_crlf_line_may_reach_the_limit_too() {
        // The limit counts a line as delivered, without its `\r\n`, wherever a
        // chunk ends: even between the `\r` and the `\n`.
        let text = b"abcd\r\n";
        for cut in 0..=text.len() {
            let mut splitter = LineSplitter::new(4);
            let lines = push_all(&mut splitter, &[&text[..cut], &text[cut..]]).unwrap();
            assert_eq!(lines, ["abcd"], "cut at {cut}");
        }
        let mut splitter = LineSplitter::new(4);
        let bytes: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(push_all(&mut splitter, &bytes).unwrap(), ["abcd"]);

        let mut splitter = LineSplitter::new(4);
        assert_eq!(
            push_all(&mut splitter, &[b"abcde\r\n"]),
            Err(StreamError::LineTooLong)
        );
    }

    #[test]
    fn a_held_carriage_return_counts_unless_a_line_feed_follows() {
        // `abcd\r` may wait for its `\n`, but anything else makes it too long.
        for next in [&b"x"[..], b"\r\n", b"\r"] {
            let mut splitter = LineSplitter::new(4);
            assert_eq!(
                push_all(&mut splitter, &[b"abcd\r"]).unwrap(),
                Vec::<String>::new()
            );
            assert_eq!(
                push_all(&mut splitter, &[next]),
                Err(StreamError::LineTooLong),
                "{next:?}"
            );
        }

        // Without a line ending, the `\r` is part of the last line.
        let mut splitter = LineSplitter::new(4);
        push_all(&mut splitter, &[b"abcd\r"]).unwrap();
        assert_eq!(splitter.finish(), Err(StreamError::LineTooLong));
        let mut splitter = LineSplitter::new(5);
        push_all(&mut splitter, &[b"abcd\r"]).unwrap();
        assert_eq!(splitter.finish().unwrap().as_deref(), Some("abcd\r"));
    }

    #[test]
    fn invalid_utf8_fails_only_once_the_line_is_complete() {
        let mut splitter = LineSplitter::new(64);
        assert!(push_all(&mut splitter, &[b"ok \xC3"]).is_ok());
        assert_eq!(
            push_all(&mut splitter, &[b"\x28\n"]),
            Err(StreamError::InvalidUtf8)
        );

        let mut splitter = LineSplitter::new(64);
        push_all(&mut splitter, &[b"tail \xFF"]).unwrap();
        assert_eq!(splitter.finish(), Err(StreamError::InvalidUtf8));
    }

    #[test]
    fn split_text_never_cuts_a_character() {
        let text = "aé✓😀".repeat(50);
        for max in 1..20 {
            let pieces: Vec<&str> = split_text(&text, max).collect();
            assert_eq!(pieces.concat(), text, "max {max}");
            assert!(
                pieces
                    .iter()
                    .all(|piece| !piece.is_empty() && piece.len() <= max.max(4)),
                "max {max}"
            );
        }
        assert_eq!(split_text("", 8).count(), 0);
        assert_eq!(split_text("abc", 8).collect::<Vec<_>>(), ["abc"]);
        assert_eq!(
            split_text("abcdefgh", 4).collect::<Vec<_>>(),
            ["abcd", "efgh"]
        );
    }

    #[cfg(unix)]
    fn shell_stream(script: &str) -> LineStream {
        let process =
            Process::spawn(&crate::process::ProcessSpec::new("/bin/sh").args(["-c", script]))
                .expect("start a shell");
        LineStream::new(process, 1024)
    }

    #[cfg(unix)]
    fn next_line(stream: &mut LineStream) -> Output {
        stream
            .next(Instant::now() + Duration::from_secs(10))
            .expect("the process wrote or ended in time")
    }

    #[cfg(unix)]
    #[test]
    fn a_stream_whose_process_is_still_running_can_be_left_to_a_reaper() {
        let mut stream = shell_stream("echo answer; sleep 0.3");
        assert_eq!(next_line(&mut stream), Output::Line("answer".to_owned()));
        let reaper = Reaper::new(1);
        let released = Instant::now();
        assert!(
            stream
                .release(&reaper, Duration::from_secs(30), Duration::from_secs(5))
                .is_ok()
        );
        assert!(released.elapsed() < Duration::from_millis(250));
        assert_eq!(reaper.waiting(), 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while reaper.waiting() > 0 {
            assert!(Instant::now() < deadline, "the reaper never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_stream_with_lines_still_to_deliver_is_not_released() {
        // Both lines are one write, so the second is read with the first.
        let mut stream = shell_stream("printf 'first\\nsecond\\n'; sleep 30");
        assert_eq!(next_line(&mut stream), Output::Line("first".to_owned()));
        let mut stream = *stream
            .release(
                &Reaper::new(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .expect_err("a line it wrote was still to be delivered");
        assert_eq!(next_line(&mut stream), Output::Line("second".to_owned()));
        stream.cancel(Duration::from_millis(100));
        assert!(matches!(next_line(&mut stream), Output::Stopped(_)));
    }

    #[cfg(unix)]
    #[test]
    fn a_stream_that_cannot_be_released_is_given_back_whole() {
        // The process has exited and its end has been delivered.
        let mut stream = shell_stream("echo answer");
        assert_eq!(next_line(&mut stream), Output::Line("answer".to_owned()));
        assert!(matches!(next_line(&mut stream), Output::Final(_)));
        let mut stream = *stream
            .release(
                &Reaper::new(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .expect_err("a process that has ended is not released");
        assert!(matches!(next_line(&mut stream), Output::Final(_)));

        // A reaper with no room gives the running process back.
        let mut stream = shell_stream("echo answer; sleep 30");
        assert_eq!(next_line(&mut stream), Output::Line("answer".to_owned()));
        let mut stream = *stream
            .release(
                &Reaper::new(0),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .expect_err("there is no room");
        stream.cancel(Duration::from_millis(100));
        assert!(matches!(next_line(&mut stream), Output::Stopped(_)));
    }
}
