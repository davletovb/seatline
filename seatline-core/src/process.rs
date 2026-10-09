//! Provider process manager (NAT-04).
//!
//! [`Process`] starts a provider executable from an absolute path and an
//! argument array, with no shell and no `PATH` search, and supervises it until
//! it has exited and been reaped:
//!
//! - the process gets only the environment variables its [`ProcessSpec`]
//!   lists, never the host's whole environment (SEC-02);
//! - stdin, stdout, and stderr are three separate pipes, so a provider never
//!   shares the host's own Native Messaging streams;
//! - output arrives in chunks of at most [`MAX_CHUNK_BYTES`], in the order each
//!   stream produced it, with at most [`MAX_QUEUED_CHUNKS`] read ahead of the
//!   caller;
//! - input is written by a helper thread, so a provider that isn't reading
//!   can't block the caller;
//! - [`Process::terminate`] asks the process to stop and kills it when the
//!   grace period runs out, and [`Process::kill`] kills it at once;
//! - dropping a [`Process`] kills and reaps it;
//! - a [`Reaper`] takes over a process whose owner has what it needed from it,
//!   waits for it to exit on its own off the owner's path, and stops it only if
//!   it does not within a grace period.
//!
//! On POSIX the child leads a new process group. Stopping it signals the whole
//! group, so the processes it started stop too, and when it exits on its own
//! the host kills any it left behind. The host signals the group before it
//! reaps the child: until then, the child's process ID, which is also the
//! group's, can't be reused. On Windows only the child itself is stopped so
//! far; stopping its whole tree needs a Job Object (ADR-0001).

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Largest chunk of output read from a provider at once.
pub const MAX_CHUNK_BYTES: usize = 8 * 1024;

/// Output chunks read ahead of the caller, across stdout and stderr. Beyond
/// them, the reader threads wait, and so does a provider that keeps writing:
/// at most 128 KiB of a provider's output waits in the host.
pub const MAX_QUEUED_CHUNKS: usize = 16;

/// How often a waiting call checks whether the process has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long the host keeps reading after the process exited and its process
/// group was stopped. Only a descendant that left the group can hold the
/// output open that long.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// How long cleanup waits for helper threads to finish before leaving them to
/// end on their own.
const JOIN_TIMEOUT: Duration = Duration::from_millis(100);

/// An executable to start, the arguments to pass it, its environment, and
/// where to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    current_dir: Option<PathBuf>,
}

impl ProcessSpec {
    /// Starts `program`, which must be an absolute path. Finding a provider's
    /// executable is the provider registry's job (PRO-02), not the process
    /// manager's.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            current_dir: None,
        }
    }

    /// Appends one argument. It is passed as its own argv element: no shell
    /// parses or splits it.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends each of `args`, as [`ProcessSpec::arg`] does.
    #[must_use]
    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable for the process. Its environment starts
    /// empty: it gets only the variables set here, none of the host's.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sets each of `vars`, as [`ProcessSpec::env`] does.
    #[must_use]
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.extend(
            vars.into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    /// Runs the process in `dir` instead of the host's working directory.
    #[must_use]
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }
}

/// Something a process did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Bytes the process wrote to stdout: at most [`MAX_CHUNK_BYTES`], cut
    /// wherever a read happened to end.
    Stdout(Vec<u8>),
    /// Bytes the process wrote to stderr, chunked the same way.
    Stderr(Vec<u8>),
    /// The process exited and was reaped. Always the last event.
    Exited(Exit),
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The exit status, or `None` if something outside the host reaped the
    /// process first.
    pub status: Option<ExitStatus>,
    pub ending: Ending,
    /// Whether stdout and stderr both reached end of file. `false` means a
    /// descendant outside the process group still held them open when the
    /// host stopped waiting, so output may be missing.
    pub output_closed: bool,
}

/// Who ended a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// It exited on its own.
    Natural,
    /// It exited within the grace period after [`Process::terminate`] asked
    /// it to stop.
    Stopped,
    /// It was killed: by [`Process::kill`], by a drop, or when a grace period
    /// ran out.
    Killed,
}

/// A running provider process. See the module documentation.
pub struct Process {
    child: Child,
    /// Queues input for the stdin writer thread; `None` once stdin is closed.
    stdin: Option<Sender<Vec<u8>>>,
    output: Receiver<Output>,
    /// Output streams that haven't reached end of file.
    open_streams: usize,
    threads: Vec<JoinHandle<()>>,
    ending: Ending,
    state: State,
    /// Sees the child's exit before it is reaped.
    exit_watch: tree::ExitWatch,
}

#[derive(Clone, Copy)]
enum State {
    Running,
    /// Reaped; the rest of its output is read until `drain_until`.
    Reaped {
        status: Option<ExitStatus>,
        drain_until: Instant,
    },
    Finished(Exit),
}

/// What the reader threads send.
enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// One stream reached end of file, or failed.
    Closed,
}

impl Process {
    /// Starts `spec` with piped stdin, stdout, and stderr.
    ///
    /// Fails with [`io::ErrorKind::InvalidInput`] if the program path isn't
    /// absolute, and with the operating system's error if the process can't
    /// start.
    pub fn spawn(spec: &ProcessSpec) -> io::Result<Self> {
        if !spec.program.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a provider executable path must be absolute",
            ));
        }

        // The one place the host starts a process (SEC-01).
        #[allow(clippy::disallowed_methods)]
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &spec.current_dir {
            command.current_dir(dir);
        }
        tree::configure(&mut command);
        let mut child = command.spawn()?;
        let exit_watch = tree::ExitWatch::new(&child);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (sender, output) = mpsc::sync_channel(MAX_QUEUED_CHUNKS);
        let mut process = Self {
            child,
            stdin: None,
            output,
            open_streams: 0,
            threads: Vec::new(),
            ending: Ending::Natural,
            state: State::Running,
            exit_watch,
        };

        // If a helper thread can't start, dropping `process` kills the child.
        if let Some(stdout) = stdout {
            process.start_reader("provider-stdout", stdout, Output::Stdout, sender.clone())?;
        }
        if let Some(stderr) = stderr {
            process.start_reader("provider-stderr", stderr, Output::Stderr, sender)?;
        }
        if let Some(stdin) = stdin {
            process.start_writer(stdin)?;
        }
        Ok(process)
    }

    /// The operating system's process ID. On POSIX it is also the ID of the
    /// process group the process leads.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Queues `bytes` for the process's stdin and returns without waiting for
    /// the process to read them.
    ///
    /// Fails with [`io::ErrorKind::BrokenPipe`] once stdin is closed: after
    /// [`Process::close_stdin`], after a stop, or once the process has stopped
    /// reading.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match &self.stdin {
            Some(stdin) if stdin.send(bytes.to_vec()).is_ok() => Ok(()),
            _ => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// Closes stdin once the queued input is written, so the process reads end
    /// of file.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Returns the process's next event, waiting until `deadline` at most.
    ///
    /// All output comes before [`Event::Exited`], which is the last event:
    /// once it is returned, every later call returns it again. Returns `None`
    /// if the deadline passes first; that is how a caller times a process out
    /// before stopping it. A process whose output nobody pulls eventually
    /// blocks on its own writes.
    pub fn next_event(&mut self, deadline: Instant) -> Option<Event> {
        loop {
            if let State::Finished(exit) = self.state {
                return Some(Event::Exited(exit));
            }

            let now = Instant::now();
            if self.open_streams > 0 {
                let until = match self.state {
                    State::Reaped { drain_until, .. } => drain_until,
                    _ => now + POLL_INTERVAL,
                };
                let timeout = until.min(deadline).saturating_duration_since(now);
                match self.output.recv_timeout(timeout) {
                    Ok(Output::Stdout(bytes)) => return Some(Event::Stdout(bytes)),
                    Ok(Output::Stderr(bytes)) => return Some(Event::Stderr(bytes)),
                    Ok(Output::Closed) => self.open_streams = self.open_streams.saturating_sub(1),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => self.open_streams = 0,
                }
            } else if matches!(self.state, State::Running) {
                thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
            }

            self.poll_exit();
            if let State::Reaped {
                status,
                drain_until,
            } = self.state
            {
                if self.open_streams == 0 || Instant::now() >= drain_until {
                    self.finish(status);
                    continue;
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
        }
    }

    /// Asks the process to stop without waiting: closes stdin and, on POSIX,
    /// sends SIGTERM to the process group. Keep pulling events to see it exit,
    /// and call [`Process::kill`] if it outlives your grace period. A process
    /// that exits after this is reported as [`Ending::Stopped`].
    pub fn request_stop(&mut self) {
        self.stdin = None;
        self.poll_exit();
        if matches!(self.state, State::Running) {
            self.ending = Ending::Stopped;
            tree::request_stop(&self.child);
        }
    }

    /// Asks the process to stop, waits up to `grace` for it to exit, and then
    /// kills it. Output it writes after the request is discarded.
    ///
    /// The request closes stdin and, on POSIX, sends SIGTERM to the process
    /// group. On Windows, closing stdin is the only request.
    pub fn terminate(&mut self, grace: Duration) -> Exit {
        self.stop(Some(grace))
    }

    /// Kills the process, and on POSIX its process group, without asking
    /// first. Output it hasn't been pulled yet is discarded.
    pub fn kill(&mut self) -> Exit {
        self.stop(None)
    }

    fn stop(&mut self, grace: Option<Duration>) -> Exit {
        if let State::Finished(exit) = self.state {
            return exit;
        }
        self.stdin = None;
        self.poll_exit();

        if let (State::Running, Some(grace)) = (self.state, grace) {
            self.ending = Ending::Stopped;
            tree::request_stop(&self.child);
            let give_up = Instant::now().checked_add(grace);
            while matches!(self.state, State::Running) {
                let now = Instant::now();
                let remaining = match give_up {
                    Some(give_up) if now >= give_up => break,
                    Some(give_up) => give_up - now,
                    None => POLL_INTERVAL,
                };
                self.discard_output(remaining.min(POLL_INTERVAL));
                self.poll_exit();
            }
        }

        let (status, drain_until) = match self.state {
            State::Finished(exit) => return exit,
            State::Reaped {
                status,
                drain_until,
            } => (status, drain_until),
            State::Running => {
                self.ending = Ending::Killed;
                tree::force_stop(&mut self.child);
                self.reap(true);
                match self.state {
                    State::Reaped {
                        status,
                        drain_until,
                    } => (status, drain_until),
                    State::Finished(exit) => return exit,
                    State::Running => unreachable!("a blocking reap always reaps"),
                }
            }
        };
        while self.open_streams > 0 && Instant::now() < drain_until {
            self.discard_output(drain_until.saturating_duration_since(Instant::now()));
        }
        self.finish(status)
    }

    fn start_reader<R: Read + Send + 'static>(
        &mut self,
        name: &str,
        stream: R,
        wrap: fn(Vec<u8>) -> Output,
        sender: SyncSender<Output>,
    ) -> io::Result<()> {
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || read_output(stream, wrap, &sender))?;
        self.threads.push(thread);
        self.open_streams += 1;
        Ok(())
    }

    fn start_writer(&mut self, mut stdin: ChildStdin) -> io::Result<()> {
        let (sender, inputs) = mpsc::channel::<Vec<u8>>();
        let thread = thread::Builder::new()
            .name("provider-stdin".to_owned())
            .spawn(move || {
                // Runs until stdin is closed or the process stops reading;
                // dropping `stdin` then closes the pipe.
                for bytes in inputs {
                    if stdin.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })?;
        self.threads.push(thread);
        self.stdin = Some(sender);
        Ok(())
    }

    /// Reaps the process if it has exited.
    fn poll_exit(&mut self) {
        self.reap(false);
    }

    /// Reaps the process once it has exited, waiting for that if `block` is
    /// set, and stops whatever it left behind: descendants left in its group
    /// would outlive it as orphans, and could hold its output open.
    fn reap(&mut self, block: bool) {
        if !matches!(self.state, State::Running) {
            return;
        }
        let seen = if block {
            self.exit_watch.wait(&self.child)
        } else {
            self.exit_watch.exited(&self.child)
        };
        let stopped_first = match seen {
            Some(false) if !block => return,
            // Exited but not reaped, so its process ID, and with it the
            // group's, is still taken: the signal can reach only the group.
            Some(true) => {
                tree::stop_leftovers(&self.child);
                true
            }
            // No way to tell without reaping: signal after it, see
            // `tree::stop_leftovers`.
            _ => false,
        };
        let status = if block {
            self.child.wait().ok()
        } else {
            match self.child.try_wait() {
                Ok(None) => return,
                Ok(Some(status)) => Some(status),
                // Only something outside the host reaping the child fails
                // this.
                Err(_) => None,
            }
        };
        if !stopped_first {
            tree::stop_leftovers(&self.child);
        }
        self.state = State::Reaped {
            status,
            drain_until: Instant::now() + DRAIN_TIMEOUT,
        };
    }

    /// Waits up to `timeout` for output and throws it away.
    fn discard_output(&mut self, timeout: Duration) {
        if self.open_streams == 0 {
            thread::sleep(timeout);
            return;
        }
        match self.output.recv_timeout(timeout) {
            Ok(Output::Closed) => self.open_streams = self.open_streams.saturating_sub(1),
            Ok(Output::Stdout(_) | Output::Stderr(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => self.open_streams = 0,
        }
    }

    fn finish(&mut self, status: Option<ExitStatus>) -> Exit {
        let exit = Exit {
            status,
            ending: self.ending,
            output_closed: self.open_streams == 0,
        };
        self.stdin = None;
        join_threads(&mut self.threads);
        self.state = State::Finished(exit);
        exit
    }
}

impl Drop for Process {
    /// Kills the process if it is still running and reaps it, so it never
    /// outlives its owner, even as a zombie.
    fn drop(&mut self) {
        self.kill();
    }
}

/// Waits for finished processes to leave, so that their owners need not.
///
/// A provider's CLI can go on working after it has said all it has to say: it
/// uploads its own usage analytics, removes its own bookkeeping files, and only then exits.
/// An owner that needs nothing more from the process, because it has the answer
/// and there is no session left to save, hands it to a reaper and goes on. The
/// process is not stopped to make that quicker: a signal is answered by the
/// same wrap-up, and a kill would skip it and leave the CLI's own files behind.
/// A helper thread waits for it instead, up to a grace period. One that has not
/// left by then is asked to stop and, if that is not enough, killed, as an
/// owner that waited would have done.
///
/// A reaper waits for at most as many processes as its capacity, so that a
/// burst of turns cannot pile up helper threads or processes: past that,
/// [`Reaper::release`] gives the process back and its owner waits for it as it
/// did before. Clones share the count.
///
/// Adapters share one, [`Reaper::shared`], so that the bound is on the whole
/// process (every application a broker serves) and not on each adapter.
#[derive(Debug, Clone)]
pub struct Reaper {
    waiting: Arc<AtomicUsize>,
    capacity: usize,
}

/// One place at a reaper, given back when the helper thread is done, however
/// it ends.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How many finished processes the shared reaper waits for at once.
pub const SHARED_REAPER_CAPACITY: usize = 8;

impl Reaper {
    /// The reaper the whole process shares, which waits for at most
    /// [`SHARED_REAPER_CAPACITY`] processes at once whichever adapter handed
    /// them over.
    pub fn shared() -> Self {
        static SHARED: OnceLock<Reaper> = OnceLock::new();
        SHARED
            .get_or_init(|| Reaper::new(SHARED_REAPER_CAPACITY))
            .clone()
    }

    /// A reaper that waits for at most `capacity` processes at once. With
    /// none, every process is given back.
    pub fn new(capacity: usize) -> Self {
        Self {
            waiting: Arc::new(AtomicUsize::new(0)),
            capacity,
        }
    }

    /// How many processes the helper threads are waiting for now.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Acquire)
    }

    /// Takes over `process`, which must not be needed any more: whatever it
    /// still writes is discarded. A helper thread waits up to `exit_grace` for
    /// it to exit and reaps it; if it has not, the thread asks it to stop,
    /// gives it `stop_grace`, and kills it.
    ///
    /// Gives the process back, still running, when the reaper is already
    /// waiting for as many as its capacity or the helper thread cannot start.
    /// (Boxed, as the way back is the rare one and a process is large.)
    pub fn release(
        &self,
        process: Process,
        exit_grace: Duration,
        stop_grace: Duration,
    ) -> Result<(), Box<Process>> {
        if !self.take_place() {
            return Err(Box::new(process));
        }
        // The thread starts first and the process follows, so that a thread
        // that cannot start leaves it with the caller. If it fails, the place
        // goes back with the closure it was moved into.
        let place = Place(Arc::clone(&self.waiting));
        let (hand_over, receive) = mpsc::sync_channel::<Process>(1);
        let spawned = thread::Builder::new()
            .name("provider-reaper".to_owned())
            .spawn(move || {
                let _place = place;
                if let Ok(process) = receive.recv() {
                    wait_out(process, exit_grace, stop_grace);
                }
            });
        if spawned.is_err() {
            return Err(Box::new(process));
        }
        hand_over.send(process).map_err(|error| Box::new(error.0))
    }

    /// Takes one of the places, if one is free. (A compare-and-swap loop of its
    /// own: the library's `fetch_update` is being renamed, and this builds the
    /// same on the oldest Rust the workspace supports and on the newest.)
    fn take_place(&self) -> bool {
        let mut waiting = self.waiting.load(Ordering::Acquire);
        loop {
            if waiting >= self.capacity {
                return false;
            }
            match self.waiting.compare_exchange_weak(
                waiting,
                waiting + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => waiting = now,
            }
        }
    }
}

/// What a reaper's helper thread does with one process.
fn wait_out(mut process: Process, exit_grace: Duration, stop_grace: Duration) {
    process.close_stdin();
    let now = Instant::now();
    let give_up = now.checked_add(exit_grace).unwrap_or(now);
    loop {
        match process.next_event(give_up) {
            Some(Event::Exited(_)) => return,
            // Output nobody wants any more is read and dropped, so that a
            // process that keeps writing is not held up by its own pipe.
            Some(_) => {}
            None => break,
        }
        if Instant::now() >= give_up {
            break;
        }
    }
    // It has not left: ask it to, and make it.
    process.terminate(stop_grace);
}

/// Forwards `stream` in chunks until end of file, then reports it closed.
/// Stops early once the [`Process`] is gone.
fn read_output(mut stream: impl Read, wrap: fn(Vec<u8>) -> Output, sender: &SyncSender<Output>) {
    let mut buffer = vec![0; MAX_CHUNK_BYTES];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if sender.send(wrap(buffer[..count].to_vec())).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = sender.send(Output::Closed);
}

/// Joins the helper threads. A reader still blocked on output that an escaped
/// descendant holds open is left to finish when that output closes.
fn join_threads(threads: &mut Vec<JoinHandle<()>>) {
    let give_up = Instant::now() + JOIN_TIMEOUT;
    for handle in threads.drain(..) {
        while !handle.is_finished() && Instant::now() < give_up {
            thread::sleep(Duration::from_millis(1));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

/// Process-tree control through process groups (ADR-0001).
#[cfg(unix)]
mod tree {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    /// Makes the child lead a new process group, whose ID is its process ID.
    pub fn configure(command: &mut Command) {
        command.process_group(0);
    }

    /// Asks the whole group to stop.
    pub fn request_stop(child: &Child) {
        signal_group(child, Signal::SIGTERM);
    }

    /// Kills the whole group, the child included.
    pub fn force_stop(child: &mut Child) {
        signal_group(child, Signal::SIGKILL);
        let _ = child.kill();
    }

    /// Kills what is left of the child's group once the child has exited.
    /// Called before the child is reaped, while its process ID, and so the
    /// group's ID, can't be reused, it reaches only the child's own
    /// descendants. Where [`ExitWatch`] can't see the exit first, it is
    /// called just after reaping: a group ID stays taken while any member is
    /// alive, so descendants are still all it can reach, but if none are left,
    /// a group that took the ID in the microseconds since would be hit.
    pub fn stop_leftovers(child: &Child) {
        signal_group(child, Signal::SIGKILL);
    }

    fn signal_group(child: &Child, signal: Signal) {
        // Group 0 would be the host's own group, and 1 is init's.
        if let Ok(group) = i32::try_from(child.id()) {
            if group > 1 {
                let _ = killpg(Pid::from_raw(group), signal);
            }
        }
    }

    pub use exit_watch::ExitWatch;

    /// Sees the exit with `waitid` and `WNOWAIT`, which leaves the child to
    /// be reaped.
    #[cfg(any(
        target_os = "android",
        target_os = "freebsd",
        all(target_os = "linux", not(target_env = "uclibc"))
    ))]
    mod exit_watch {
        use std::process::Child;

        use nix::errno::Errno;
        use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
        use nix::unistd::Pid;

        pub struct ExitWatch;

        impl ExitWatch {
            pub fn new(_child: &Child) -> Self {
                Self
            }

            /// Whether the child has exited, still unreaped; `None` if that
            /// can't be told.
            pub fn exited(&mut self, child: &Child) -> Option<bool> {
                let pid = Pid::from_raw(i32::try_from(child.id()).ok()?);
                let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
                match waitid(Id::Pid(pid), flags) {
                    Ok(WaitStatus::StillAlive) => Some(false),
                    Ok(_) => Some(true),
                    Err(_) => None,
                }
            }

            /// Waits until the child has exited, leaving it unreaped:
            /// `Some(true)`, or `None` if that can't be told.
            pub fn wait(&mut self, child: &Child) -> Option<bool> {
                let pid = Pid::from_raw(i32::try_from(child.id()).ok()?);
                let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT;
                loop {
                    match waitid(Id::Pid(pid), flags) {
                        Ok(_) => return Some(true),
                        Err(Errno::EINTR) => {}
                        Err(_) => return None,
                    }
                }
            }
        }
    }

    /// Sees the exit with kqueue: the kernel posts `NOTE_EXIT` before the
    /// child can be reaped. nix offers no `waitid` here.
    #[cfg(target_vendor = "apple")]
    mod exit_watch {
        use std::process::Child;

        use nix::errno::Errno;
        use nix::libc::timespec;
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

        /// A timeout that only polls.
        const NOW: timespec = timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        pub struct ExitWatch {
            /// `None` if the child couldn't be watched.
            queue: Option<Kqueue>,
            exited: bool,
        }

        impl ExitWatch {
            /// Watches `child` from now on. A child already exiting can't be
            /// watched (`ESRCH`), and counts as exited.
            pub fn new(child: &Child) -> Self {
                let unwatched = |exited| Self {
                    queue: None,
                    exited,
                };
                let (Ok(queue), Ok(pid)) = (Kqueue::new(), usize::try_from(child.id())) else {
                    return unwatched(false);
                };
                let exit = KEvent::new(
                    pid,
                    EventFilter::EVFILT_PROC,
                    EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
                    FilterFlag::NOTE_EXIT,
                    0,
                    0,
                );
                match queue.kevent(&[exit], &mut [], Some(NOW)) {
                    Ok(_) => Self {
                        queue: Some(queue),
                        exited: false,
                    },
                    Err(Errno::ESRCH) => unwatched(true),
                    Err(_) => unwatched(false),
                }
            }

            /// Whether the child has exited, still unreaped; `None` if that
            /// can't be told.
            pub fn exited(&mut self, _child: &Child) -> Option<bool> {
                self.check(Some(NOW))
            }

            /// Waits until the child has exited, leaving it unreaped:
            /// `Some(true)`, or `None` if that can't be told.
            pub fn wait(&mut self, _child: &Child) -> Option<bool> {
                self.check(None).filter(|&exited| exited)
            }

            /// Collects `NOTE_EXIT`, waiting up to `timeout`, or for as long
            /// as it takes if there is none.
            fn check(&mut self, timeout: Option<timespec>) -> Option<bool> {
                if self.exited {
                    return Some(true);
                }
                let queue = self.queue.as_ref()?;
                let mut events = [KEvent::new(
                    0,
                    EventFilter::EVFILT_PROC,
                    EvFlags::empty(),
                    FilterFlag::empty(),
                    0,
                    0,
                )];
                loop {
                    match queue.kevent(&[], &mut events, timeout) {
                        Ok(0) => return Some(false),
                        Ok(_) if !events[0].flags().contains(EvFlags::EV_ERROR) => {
                            self.exited = true;
                            return Some(true);
                        }
                        Err(Errno::EINTR) => {}
                        _ => return None,
                    }
                }
            }
        }
    }

    /// Elsewhere the exit shows only when the child is reaped.
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "android",
        target_os = "freebsd",
        all(target_os = "linux", not(target_env = "uclibc"))
    )))]
    mod exit_watch {
        use std::process::Child;

        pub struct ExitWatch;

        impl ExitWatch {
            pub fn new(_child: &Child) -> Self {
                Self
            }

            pub fn exited(&mut self, _child: &Child) -> Option<bool> {
                None
            }

            pub fn wait(&mut self, _child: &Child) -> Option<bool> {
                None
            }
        }
    }
}

/// On Windows only the child itself is controlled so far: stopping its whole
/// tree needs a Job Object (ADR-0001).
#[cfg(not(unix))]
mod tree {
    use std::process::{Child, Command};

    pub fn configure(_command: &mut Command) {}

    /// Closing stdin, which the caller does first, is the only request.
    pub fn request_stop(_child: &Child) {}

    pub fn force_stop(child: &mut Child) {
        let _ = child.kill();
    }

    pub fn stop_leftovers(_child: &Child) {}

    /// Only reaping shows the exit here, and there is no group to signal.
    pub struct ExitWatch;

    impl ExitWatch {
        pub fn new(_child: &Child) -> Self {
            Self
        }

        pub fn exited(&mut self, _child: &Child) -> Option<bool> {
            None
        }

        pub fn wait(&mut self, _child: &Child) -> Option<bool> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_program_path_is_refused_before_anything_starts() {
        for program in ["fake-provider", "./fake-provider", "bin/sh"] {
            let error = Process::spawn(&ProcessSpec::new(program))
                .err()
                .expect("a relative path must be refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{program}");
        }
    }

    #[test]
    fn arguments_are_kept_as_separate_elements() {
        let spec =
            ProcessSpec::new("/opt/provider")
                .arg("--mode")
                .args(["a b", "$(id)", "; rm -rf /"]);
        assert_eq!(
            spec.args,
            ["--mode", "a b", "$(id)", "; rm -rf /"].map(OsString::from)
        );
    }

    /// Where the exit watch works, it sees the exit and leaves the child to be
    /// reaped: until then, the child's group can be signalled safely.
    #[cfg(any(
        target_vendor = "apple",
        target_os = "android",
        target_os = "freebsd",
        all(target_os = "linux", not(target_env = "uclibc"))
    ))]
    #[test]
    fn an_exit_is_seen_before_the_process_is_reaped() {
        let mut running = Process::spawn(&ProcessSpec::new("/bin/sh").args(["-c", "sleep 10"]))
            .expect("start a shell");
        assert_eq!(running.exit_watch.exited(&running.child), Some(false));
        assert_eq!(running.kill().ending, Ending::Killed);

        let mut process = Process::spawn(&ProcessSpec::new("/bin/sh").args(["-c", "exit 3"]))
            .expect("start a shell");
        let deadline = Instant::now() + Duration::from_secs(10);
        while process.exit_watch.exited(&process.child) != Some(true) {
            assert!(Instant::now() < deadline, "the exit was never seen");
            thread::sleep(POLL_INTERVAL);
        }
        // Seeing the exit reaped nothing: reaping now still gets the status.
        process.poll_exit();
        assert!(
            matches!(process.state, State::Reaped { status: Some(status), .. } if status.code() == Some(3)),
            "the watch reaped the process"
        );

        // Waiting for the exit, as a kill does, leaves it unreaped too.
        let mut killed = Process::spawn(&ProcessSpec::new("/bin/sh").args(["-c", "sleep 10"]))
            .expect("start a shell");
        tree::force_stop(&mut killed.child);
        assert_eq!(killed.exit_watch.wait(&killed.child), Some(true));
        killed.poll_exit();
        assert!(
            matches!(killed.state, State::Reaped { status: Some(status), .. } if !status.success()),
            "the watch reaped the process"
        );
    }

    /// A process that has exited and been reaped no longer exists; one that has
    /// only exited lingers as a zombie, which `kill` still finds.
    #[cfg(unix)]
    fn is_gone(pid: u32) -> bool {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        kill(Pid::from_raw(i32::try_from(pid).expect("a pid")), None) == Err(Errno::ESRCH)
    }

    /// Waits until `reaper` waits for nothing, which is when its helper threads
    /// are done with every process they were given.
    #[cfg(unix)]
    fn wait_until_idle(reaper: &Reaper) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while reaper.waiting() > 0 {
            assert!(Instant::now() < deadline, "the reaper never finished");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Process {
        Process::spawn(&ProcessSpec::new("/bin/sh").args(["-c", script])).expect("start a shell")
    }

    /// Hands `process` over, which must be accepted.
    #[cfg(unix)]
    fn hand_over(reaper: &Reaper, process: Process, exit_grace: Duration, stop_grace: Duration) {
        assert!(
            reaper.release(process, exit_grace, stop_grace).is_ok(),
            "the reaper had no room"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_shared_reaper_is_one_for_the_whole_process() {
        // Only this test uses it in this test binary, so its count is its own.
        let (one, other) = (Reaper::shared(), Reaper::shared());
        assert_eq!(one.capacity, SHARED_REAPER_CAPACITY);
        hand_over(
            &one,
            shell("sleep 0.3"),
            Duration::from_secs(30),
            Duration::from_secs(5),
        );
        assert_eq!(other.waiting(), 1, "the two do not share a count");
        wait_until_idle(&other);
    }

    #[test]
    fn a_process_can_be_handed_to_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Process>();
    }

    #[cfg(unix)]
    #[test]
    fn a_released_process_leaves_on_its_own_and_is_reaped_off_the_callers_path() {
        let reaper = Reaper::new(2);
        // It exits by itself 300 ms from now, after writing, and is given far
        // longer than that.
        let process = shell("echo done; sleep 0.3");
        let pid = process.id();
        let released = Instant::now();
        hand_over(
            &reaper,
            process,
            Duration::from_secs(30),
            Duration::from_secs(5),
        );
        assert!(
            released.elapsed() < Duration::from_millis(250),
            "release waited for the process"
        );
        assert_eq!(reaper.waiting(), 1);
        wait_until_idle(&reaper);
        assert!(
            released.elapsed() >= Duration::from_millis(250),
            "it was stopped before it left on its own: {:?}",
            released.elapsed()
        );
        assert!(is_gone(pid), "left unreaped");
    }

    #[cfg(unix)]
    #[test]
    fn a_process_that_does_not_leave_is_asked_to_stop_and_then_killed() {
        let reaper = Reaper::new(2);
        // One that stops when asked, and one that ignores the request.
        let obliging = shell("sleep 30");
        let stubborn = shell("trap '' TERM; sleep 30");
        let (obliging_pid, stubborn_pid) = (obliging.id(), stubborn.id());
        let released = Instant::now();
        for process in [obliging, stubborn] {
            hand_over(
                &reaper,
                process,
                Duration::from_millis(100),
                Duration::from_millis(300),
            );
        }
        wait_until_idle(&reaper);
        assert!(released.elapsed() < Duration::from_secs(10));
        assert!(is_gone(obliging_pid) && is_gone(stubborn_pid));
    }

    #[cfg(unix)]
    #[test]
    fn a_process_that_keeps_writing_does_not_hold_a_reaper_up() {
        let reaper = Reaper::new(1);
        let process = shell("while :; do echo flood; echo flood >&2; done");
        let pid = process.id();
        hand_over(
            &reaper,
            process,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        wait_until_idle(&reaper);
        assert!(is_gone(pid));
    }

    #[cfg(unix)]
    #[test]
    fn a_reaper_gives_a_process_back_when_it_is_waiting_for_as_many_as_it_may() {
        // No room at all: the process comes back running.
        let mut returned = Reaper::new(0)
            .release(
                shell("sleep 30"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .expect_err("a reaper with no capacity takes nothing");
        assert!(!is_gone(returned.id()));
        returned.kill();

        let reaper = Reaper::new(1);
        hand_over(
            &reaper,
            shell("sleep 30"),
            Duration::from_millis(600),
            Duration::from_millis(200),
        );
        let mut second = reaper
            .release(
                shell("sleep 30"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .expect_err("it is full");
        let pid = second.id();
        assert!(!is_gone(pid), "the process it gave back was stopped");
        second.kill();
        drop(second);
        assert!(is_gone(pid));

        // A clone shares the count; once the first has left there is room again.
        let clone = reaper.clone();
        assert_eq!(clone.waiting(), 1);
        wait_until_idle(&reaper);
        hand_over(
            &clone,
            shell("exit 0"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        wait_until_idle(&clone);
    }
}
