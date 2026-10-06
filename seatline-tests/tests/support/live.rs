//! What the opt-in live smoke tests share: whether they run at all, and small
//! checks that keep credentials and the prompt out of anywhere they shouldn't be.
//!
//! A live test asks the real provider CLI installed on the machine. It is
//! switched on by an environment variable of its own. Unset (or `0`), as in
//! normal CI runs, the test passes at once. Set to `1`, it runs when the CLI is
//! installed and signed in, and is skipped, passing, when it isn't. Set to
//! `required`, as in a job set up with a sign-in, a missing or signed-out CLI
//! fails it instead.

use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use seatline_core::exchange::{Exchange, Update};
use seatline_core::protocol::Availability;
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::Provider;

/// How long a real model may take to answer.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a status check may take.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Run when the CLI is installed and signed in; skip otherwise.
    IfReady,
    /// Run, and fail when the CLI isn't installed or signed in.
    Required,
}

/// The mode the environment variable `variable` asks for.
pub fn mode(variable: &str) -> Mode {
    match std::env::var(variable).as_deref() {
        Err(_) | Ok("" | "0") => Mode::Off,
        Ok("required") => Mode::Required,
        Ok(_) => Mode::IfReady,
    }
}

/// Why the test can't run here, when the CLI isn't ready: a failure in
/// `required` mode, a note otherwise.
pub fn skip_or_fail(variable: &str, mode: Mode, reason: &str) {
    assert_ne!(mode, Mode::Required, "{variable}=required: {reason}");
    eprintln!("skipped: {reason}");
}

/// Pulls updates until a terminal one, allowing `timeout` for all of them.
pub fn run_within(exchange: &mut dyn Exchange, timeout: Duration) -> Vec<Update> {
    let deadline = Instant::now() + timeout;
    let mut updates = Vec::new();
    loop {
        let update = exchange
            .next(deadline)
            .unwrap_or_else(|| panic!("the exchange should end within {timeout:?}: {updates:?}"));
        let terminal = update.is_terminal();
        updates.push(update);
        if terminal {
            return updates;
        }
    }
}

/// An ephemeral turn of one question, with the sign-in checked first.
pub fn turn(system: Option<&str>, text: &str, tools: ToolPolicy) -> Turn {
    Turn {
        system: system.map(str::to_owned),
        messages: vec![Message {
            role: Role::User,
            text: text.to_owned(),
        }],
        model: None,
        reasoning_effort: None,
        tools,
        session: SessionPolicy::Ephemeral,
        continuation: None,
        cleanup_group: None,
        check_sign_in: true,
    }
}

/// The provider's status, asked again after a moment when it says the CLI is
/// unavailable: a status probe can need the network (Antigravity's fetches its
/// models), and one blip must not fail a `required` run.
pub fn status_of(provider: &dyn Provider) -> Vec<Update> {
    const ATTEMPTS: u32 = 3;
    let mut updates = Vec::new();
    for attempt in 1..=ATTEMPTS {
        updates = run_within(provider.status().as_mut(), STATUS_TIMEOUT);
        let unavailable = matches!(
            updates.first(),
            Some(Update::Status { status, .. }) if status.availability == Availability::Unavailable
        );
        if !unavailable || attempt == ATTEMPTS {
            break;
        }
        eprintln!("the status probe said unavailable (attempt {attempt}); asking again");
        std::thread::sleep(Duration::from_secs(3));
    }
    updates
}

/// Panics unless the turn ended with `Completed`, and says how it did end: the
/// terminal update and how many updates came before it, not all of them.
pub fn assert_completed(what: &str, updates: &[Update]) {
    match updates.last() {
        Some(Update::Completed) => {}
        Some(other) => panic!(
            "{what} ended with {other:?} after {} updates",
            updates.len()
        ),
        None => panic!("{what} gave no updates"),
    }
}

/// A scratch directory of this test run, under cargo's temporary directory,
/// removed when it goes out of scope, a panic included.
pub struct Scratch(PathBuf);

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn scratch(name: &str) -> Scratch {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("live-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    Scratch(dir)
}

/// The user's home directory, where the provider CLIs keep their own files.
pub fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(unix) { "HOME" } else { "USERPROFILE" }).map(PathBuf::from)
}

/// Where a provider CLI keeps its own files: the directory its environment
/// variable `variable` names when that is set, otherwise `default` under the
/// user's home directory.
pub fn provider_home(variable: &str, default: &str) -> Option<PathBuf> {
    match std::env::var_os(variable) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home().map(|home| home.join(default)),
    }
}

/// How far before a run began a file's modification time may lie and the file
/// still count as written during the run: file systems keep times coarsely
/// (FAT to two seconds), and clocks are read at different moments.
const CLOCK_SLACK: Duration = Duration::from_secs(5);

/// A string no earlier run could have written, to look for afterwards, and
/// when the run that made it began.
#[derive(Debug, Clone)]
pub struct Marker {
    text: String,
    since: SystemTime,
}

impl fmt::Display for Marker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

pub fn marker() -> Marker {
    let now = SystemTime::now();
    let nanos = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    Marker {
        text: format!("marker-{}-{nanos}", std::process::id()),
        since: now.checked_sub(CLOCK_SLACK).unwrap_or(UNIX_EPOCH),
    }
}

/// What a search for a marker under a directory found.
#[derive(Debug, PartialEq, Eq)]
pub struct Scan {
    /// The first file that holds it.
    pub found: Option<PathBuf>,
    /// Whether the search left files unread, because there were too many or
    /// they were too large: not finding it then proves less.
    pub incomplete: bool,
}

/// Looks for `marker` in the files under `dir`, without following links. It
/// looks everywhere, not only where a provider is known to keep its files,
/// because what it is for is finding where a provider keeps them that nobody
/// knew. A file that holds the marker was written after the marker was made,
/// so it only reads files modified since then, and a provider's home that
/// holds years of files (a hundred thousand of them, say) is searched as fully
/// as an empty one: at most `MAX_FILES` files of at most `MAX_FILE_BYTES` each
/// are read, and it says so when it left any unread.
pub fn find_marker(dir: &Path, marker: &Marker) -> Scan {
    const MAX_FILES: usize = 20_000;
    const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

    let mut scan = Scan {
        found: None,
        incomplete: false,
    };
    let mut pending = vec![dir.to_path_buf()];
    let mut read = 0;
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                // A file whose time can't be read is looked at, not assumed old.
                let metadata = entry.metadata().ok();
                if metadata
                    .as_ref()
                    .and_then(|meta| meta.modified().ok())
                    .is_some_and(|modified| modified < marker.since)
                {
                    continue;
                }
                read += 1;
                if read > MAX_FILES {
                    scan.incomplete = true;
                    return scan;
                }
                if metadata.is_some_and(|meta| meta.len() > MAX_FILE_BYTES) {
                    scan.incomplete = true;
                    continue;
                }
                if std::fs::read(&path).is_ok_and(|bytes| contains(&bytes, marker.text.as_bytes()))
                {
                    scan.found = Some(path);
                    return scan;
                }
            }
        }
    }
    scan
}

/// Panics if `marker` is anywhere under `dir`, and says so when it could not
/// look everywhere.
pub fn assert_no_marker(what: &str, dir: &Path, marker: &Marker) {
    let scan = find_marker(dir, marker);
    assert_eq!(scan.found, None, "{what}: a file holds the prompt");
    if scan.incomplete {
        eprintln!(
            "warning: {what}: files under {} that changed during the run were too many or too large to be read in full, so the prompt may be in one that was not read",
            dir.display()
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Panics, without printing it, if `text` holds a credential: the value of one
/// of `variables` in this environment, or anything shaped like an API key.
pub fn assert_no_credentials(what: &str, text: &str, variables: &[&str]) {
    for name in variables {
        if let Ok(value) = std::env::var(name) {
            assert!(
                value.len() < 8 || !text.contains(&value),
                "{what}: the value of {name} appears"
            );
        }
    }
    // A run of key characters after a prefix that keys of the providers use.
    for prefix in ["sk-", "xai-", "AIza"] {
        let shaped_like_a_key = text.match_indices(prefix).any(|(start, _)| {
            text[start + prefix.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .count()
                >= 16
        });
        assert!(
            !shaped_like_a_key,
            "{what}: something shaped like an API key appears"
        );
    }
}

/// The text of the answer in `updates`, checked for credentials before anyone
/// prints it: a model's words are the one thing a test prints that no test
/// wrote.
pub fn answer_of(what: &str, updates: &[Update], variables: &[&str]) -> String {
    assert_no_credentials(what, &format!("{updates:?}"), variables);
    updates
        .iter()
        .filter_map(|update| match update {
            Update::Delta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Asks `provider` a question under a system prompt that says to answer every
/// question with one word, and panics unless the answer has it. A model's
/// compliance is not certain, so it gets a second try before the way the
/// adapter delivers the prompt is called broken. `tools` are the plain turn's.
pub fn assert_follows_system_prompt(
    name: &str,
    provider: &dyn Provider,
    tools: ToolPolicy,
    credentials: &[&str],
) {
    let mut followed = String::new();
    for attempt in 1..=2 {
        let updates = run_within(
            provider
                .send(turn(
                    Some("Whatever you are asked, reply with the single word: marmalade"),
                    "What is two plus two?",
                    tools,
                ))
                .as_mut(),
            ANSWER_TIMEOUT,
        );
        assert_completed("the instructed turn", &updates);
        followed = answer_of("the instructed answer", &updates, credentials);
        eprintln!("with a system prompt (attempt {attempt}), {name} answered: {followed}");
        if followed.to_lowercase().contains("marmalade") {
            return;
        }
    }
    panic!("the system prompt was not followed: {followed}");
}
