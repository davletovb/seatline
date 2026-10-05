//! Stopping the running broker: `seatline-companion stop`, and what `install` does after it has
//! registered a new copy.
//!
//! The broker is one process shared by every app, and it stays up while anything is connected, so an
//! installed upgrade did not take effect until the old broker had been idle for ten minutes. Apps that
//! keep a connection (or reconnect often) kept it alive indefinitely, and the only remedy was to find
//! and kill the process by hand. Now:
//!
//! - A broker publishes a fresh random **control token** in `broker-control` (readable by the current
//!   user only) while it is listening, and withdraws it when it leaves. The token is not an app grant:
//!   an app's token cannot stop the broker, and this one is no use for any app request.
//! - `stop` reads the token, connects to the broker without starting one, and sends
//!   `{"version":1,"control":"stop","token":…}` in place of the authentication frame. The broker
//!   answers `{"type":"stopping"}`, closes its listener, and leaves as soon as the hub has nothing
//!   queued or running, or after [`grace`] at the latest, whichever comes first. What is still running
//!   then is ended the way a closing broker always ends it: providers are stopped and reaped, and the
//!   apps' connections close, so each app sees a lost connection and reconnects by itself.
//! - The next use of Seatline starts the installed copy, as it does after an idle exit.
//!
//! A broker that was started by a Seatline without `stop` does not understand the frame and drops the
//! connection: [`Outcome::Unanswered`] says so, and that one broker has to be ended by hand, once.
use std::io;
use std::path::Path;
use std::time::Duration;

use interprocess::local_socket::tokio::{Stream, prelude::*};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use crate::{PROTOCOL_VERSION, client, config, wire};

/// Where the running broker publishes its control token, in the data directory.
pub const TOKEN_FILE: &str = "broker-control";
/// How long a stopping broker waits for running work, in milliseconds. A test sets it.
pub const GRACE_VARIABLE: &str = "SEATLINE_STOP_GRACE_MS";
const DEFAULT_GRACE: Duration = Duration::from_secs(10);
/// How long `stop` waits, beyond the grace period, for the broker to finish leaving.
const LEAVING: Duration = Duration::from_secs(5);
/// How long `stop` gives a broker that holds the lock to publish its token and start listening.
const STARTING: Duration = Duration::from_secs(3);

/// The longest a stopping broker waits for work that is still running before it ends it.
pub fn grace() -> Duration {
    std::env::var(GRACE_VARIABLE)
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(DEFAULT_GRACE, Duration::from_millis)
}

/// Creates the token this broker accepts a stop request with, and publishes it.
pub fn publish(root: &Path) -> io::Result<String> {
    let token = config::random_token()?;
    config::write_private(&root.join(TOKEN_FILE), token.as_bytes())?;
    Ok(token)
}

/// Removes the published token. A token left behind by a broker that was killed is harmless: the next
/// broker replaces it, and until then nothing answers to it.
pub fn withdraw(root: &Path) {
    let _ = std::fs::remove_file(root.join(TOKEN_FILE));
}

/// Whether a broker is running: it holds `broker.lock` for as long as it lives.
pub fn running(root: &Path) -> io::Result<bool> {
    match config::lock(root, "broker.lock") {
        Ok(_free) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

/// Whether `frame` is a request to stop the broker holding `token`.
pub fn is_stop(frame: &Value, token: &str) -> bool {
    frame["version"] == PROTOCOL_VERSION
        && frame["control"] == "stop"
        && config::same_token(token, frame["token"].as_str().unwrap_or(""))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No broker was running.
    NotRunning,
    /// The broker left.
    Stopped,
    /// A broker is running that did not understand the request: it was started by a Seatline without
    /// `stop` and has to be ended by hand once.
    Unanswered,
    /// The broker accepted the request but had not left when the waiting ended.
    Lingering,
}

/// Asks the running broker to stop and waits for it to leave.
pub async fn stop(root: &Path) -> io::Result<Outcome> {
    if !running(root)? {
        return Ok(Outcome::NotRunning);
    }
    // A broker that supports `stop` publishes its token and starts listening a little after it takes the
    // lock, and keeps the token until it leaves. One that has only just started gets time to do both
    // before its silence is taken for a Seatline without `stop`.
    let settled = Instant::now() + STARTING;
    let mut published = false;
    let asked = loop {
        if let Ok(token) = std::fs::read_to_string(root.join(TOKEN_FILE)) {
            published = true;
            match ask(root, token.trim()).await {
                Asked::NotListening => {}
                answered => break answered,
            }
        }
        if !running(root)? {
            // It left on its own while we waited.
            return Ok(Outcome::Stopped);
        }
        if Instant::now() >= settled {
            if !published {
                return Ok(Outcome::Unanswered);
            }
            break Asked::NotListening;
        }
        sleep(Duration::from_millis(25)).await;
    };
    if asked == Asked::Misunderstood {
        // It may have left on its own while we asked.
        return Ok(if running(root)? {
            Outcome::Unanswered
        } else {
            Outcome::Stopped
        });
    }
    // Stopping, or no longer listening because it is already leaving: wait for it to leave.
    let until = Instant::now() + grace() + LEAVING;
    while running(root)? {
        if Instant::now() >= until {
            return Ok(Outcome::Lingering);
        }
        sleep(Duration::from_millis(25)).await;
    }
    Ok(Outcome::Stopped)
}

#[derive(Debug, PartialEq, Eq)]
enum Asked {
    /// The broker said it is stopping.
    Stopping,
    /// Nothing accepted the connection (a broker that is already leaving has closed its listener) or
    /// the broker stayed busy.
    NotListening,
    /// Something accepted the connection and did not answer the request: a broker without `stop`.
    Misunderstood,
}

/// Sends the request. A broker at its connection limit says it is busy: ask again.
async fn ask(root: &Path, token: &str) -> Asked {
    for _ in 0..10 {
        let Ok(name) = client::socket_name(root) else {
            return Asked::NotListening;
        };
        let Ok(Ok(mut stream)) = timeout(Duration::from_secs(1), Stream::connect(name)).await
        else {
            return Asked::NotListening;
        };
        let frame = json!({"version":PROTOCOL_VERSION,"control":"stop","token":token});
        if wire::write_frame(&mut stream, &frame).await.is_err() {
            return Asked::Misunderstood;
        }
        match timeout(Duration::from_secs(2), wire::read_frame(&mut stream)).await {
            Ok(Ok(reply)) if reply["type"] == "stopping" => return Asked::Stopping,
            Ok(Ok(reply)) if reply["type"] == "busy" => sleep(Duration::from_millis(200)).await,
            _ => return Asked::Misunderstood,
        }
    }
    Asked::NotListening
}

/// What to tell someone whose running broker cannot be asked to stop.
pub const UNANSWERED: &str = "A Seatline is running that cannot be asked to stop (it was started by a version without `stop`). \
End it once yourself: on macOS or Linux run `pkill -f \"seatline-companion serve\"`; on Windows end seatline-companion.exe in Task Manager. \
Later versions stop with `seatline-companion stop`.";
