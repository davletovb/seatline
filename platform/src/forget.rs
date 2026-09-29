//! Removing what a provider keeps for a deleted conversation
//! (`conversation.forget`, v1 §5.4).
//!
//! Only files a provider wrote for the application are removed: a transcript
//! counts as the application's when it names the session being forgotten and
//! records the application's private working directory as the place it ran. Nothing is followed through
//! a symbolic link: a link is removed itself, never what it points to.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use seatline_core::exchange::{Exchange, Scripted, Update};
use seatline_core::protocol::ErrorCode;
use seatline_core::protocol::Failure as ErrorBody;

pub const SESSION_FORGET_FAILED: ErrorBody = ErrorBody {
    code: ErrorCode::InternalError,
    reason: "SESSION_FORGET_FAILED",
    retryable: true,
};

/// Runs `work`, the file-system part of forgetting, on a thread of its own,
/// so a large or slow provider directory never holds up the host loop, and
/// reports how it ended: `Completed`, or `Failed` with
/// [`SESSION_FORGET_FAILED`]. `completed` runs on the caller's thread once
/// `work` succeeded, and never otherwise: what it drops, such as an
/// in-memory mapping, stays for a retry after a failure.
pub fn in_background(
    work: impl FnOnce() -> io::Result<()> + Send + 'static,
    completed: impl FnOnce() + 'static,
) -> Box<dyn Exchange> {
    let (done, outcome) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("provider-forget".to_owned())
        .spawn(move || {
            let _ = done.send(work());
        });
    match spawned {
        Ok(_) => Box::new(Background::Waiting(outcome, Box::new(completed))),
        Err(_) => Box::new(Scripted::failed(SESSION_FORGET_FAILED)),
    }
}

/// Background work in progress, as an exchange.
enum Background {
    Waiting(Receiver<io::Result<()>>, Box<dyn FnOnce()>),
    /// Cancelled: the work may still finish, but its outcome is dropped.
    Stopping,
    Done,
}

impl Exchange for Background {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let update = match self {
            Self::Done => return None,
            Self::Stopping => Update::Stopped,
            Self::Waiting(outcome, _) => {
                match outcome.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Ok(())) => Update::Completed,
                    Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                        Update::Failed(SESSION_FORGET_FAILED)
                    }
                    Err(RecvTimeoutError::Timeout) => return None,
                }
            }
        };
        if let Self::Waiting(_, completed) = std::mem::replace(self, Self::Done) {
            if update == Update::Completed {
                completed();
            }
        }
        Some(update)
    }

    fn cancel(&mut self, _grace: Duration) {
        if matches!(self, Self::Waiting(..)) {
            *self = Self::Stopping;
        }
    }
}

/// Runs best-effort provider cleanup away from the host loop. The work must
/// preserve its durable retry record when it fails.
pub fn work_in_background(work: impl FnOnce() + Send + 'static) {
    let _ = thread::Builder::new()
        .name("provider-cleanup".to_owned())
        .spawn(work);
}

/// Runs best-effort provider cleanup and removes its durable retry marker only
/// after the cleanup succeeds.
pub fn tracked_cleanup(
    marker: Option<PathBuf>,
    work: impl FnOnce() -> io::Result<()> + Send + 'static,
) {
    work_in_background(move || {
        if work().is_ok() {
            if let Some(marker) = marker {
                let _ = remove(&marker);
            }
        }
    });
}

/// How much of a transcript is read to find where it ran.
const HEAD_BYTES: u64 = 1024 * 1024;

/// The complete lines within the first megabyte of `path`, or `None` if it
/// can't be read as a regular file.
pub fn head_lines(path: &Path) -> Option<Vec<String>> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut head = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(HEAD_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    let complete = head
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |end| end + 1);
    Some(
        String::from_utf8_lossy(&head[..complete])
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

/// Whether the working directory a provider `recorded` is `workspace`.
pub fn same_directory(recorded: &str, workspace: &Path) -> bool {
    let recorded = Path::new(recorded);
    match (fs::canonicalize(recorded), fs::canonicalize(workspace)) {
        (Ok(recorded), Ok(workspace)) => recorded == workspace,
        _ => recorded == workspace,
    }
}

/// Removes `path`: a file, a symbolic link (not its target), or a directory
/// tree. A path that doesn't exist is already removed.
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

pub fn remove_in_background(path: std::path::PathBuf) {
    let _ = thread::Builder::new()
        .name("provider-cleanup".to_owned())
        .spawn(move || {
            let _ = remove(&path);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forget-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn removes_files_directories_and_missing_paths() {
        let dir = scratch("remove");
        fs::write(dir.join("file"), "x").unwrap();
        fs::create_dir_all(dir.join("tree/inner")).unwrap();
        fs::write(dir.join("tree/inner/file"), "x").unwrap();
        remove(&dir.join("file")).unwrap();
        remove(&dir.join("tree")).unwrap();
        remove(&dir.join("missing")).unwrap();
        assert!(!dir.join("file").exists());
        assert!(!dir.join("tree").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_removed_but_not_followed() {
        let dir = scratch("link");
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("target/keep"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("target"), dir.join("link")).unwrap();
        remove(&dir.join("link")).unwrap();
        assert!(!dir.join("link").exists());
        assert!(dir.join("target/keep").exists());
        assert_eq!(head_lines(&dir.join("link")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    fn run_to_end(exchange: &mut dyn Exchange) -> Vec<Update> {
        let give_up = Instant::now() + Duration::from_secs(10);
        let mut updates = Vec::new();
        while Instant::now() < give_up {
            if let Some(update) = exchange.next(Instant::now() + Duration::from_millis(50)) {
                let terminal = update.is_terminal();
                updates.push(update);
                if terminal {
                    return updates;
                }
            }
        }
        panic!("the background work never ended: {updates:?}");
    }

    #[test]
    fn background_work_reports_how_it_ended_without_blocking() {
        let (release, released) = mpsc::channel::<()>();
        let finished = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = std::rc::Rc::clone(&finished);
        let mut slow = in_background(
            move || {
                let _ = released.recv();
                Ok(())
            },
            move || counted.set(counted.get() + 1),
        );
        // Still working: the caller gets its turn back at its deadline.
        let asked = Instant::now();
        assert_eq!(slow.next(asked + Duration::from_millis(20)), None);
        assert!(asked.elapsed() < Duration::from_secs(2));
        assert_eq!(finished.get(), 0);
        release.send(()).unwrap();
        assert_eq!(run_to_end(slow.as_mut()), [Update::Completed]);

        assert_eq!(finished.get(), 1, "runs once, after the work succeeded");

        let kept = std::rc::Rc::new(std::cell::Cell::new(true));
        let dropped = std::rc::Rc::clone(&kept);
        let mut failing = in_background(
            || Err(io::Error::other("denied")),
            move || dropped.set(false),
        );
        assert_eq!(
            run_to_end(failing.as_mut()),
            [Update::Failed(SESSION_FORGET_FAILED)]
        );
        assert!(kept.get(), "a failure keeps what `completed` would drop");

        let (_keep, never) = mpsc::channel::<()>();
        let mut cancelled = in_background(
            move || {
                let _ = never.recv_timeout(Duration::from_secs(5));
                Ok(())
            },
            || {},
        );
        cancelled.cancel(Duration::ZERO);
        assert_eq!(run_to_end(cancelled.as_mut()), [Update::Stopped]);
    }

    #[test]
    fn head_lines_keeps_only_complete_lines() {
        let dir = scratch("head");
        fs::write(dir.join("t"), "one\ntwo\npartial").unwrap();
        assert_eq!(
            head_lines(&dir.join("t")),
            Some(vec!["one".to_owned(), "two".to_owned()])
        );
        assert_eq!(head_lines(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
