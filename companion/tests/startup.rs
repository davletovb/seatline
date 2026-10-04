//! Starting the broker when several clients need one at once (D-02), with the
//! real companion: clients that arrive together with no broker running cause
//! one start, not one each; a claim whose holder never starts a broker, or
//! goes away, is recovered from; and a companion that cannot be started is
//! reported at once.
//!
//! One test runs every case in turn, and the scripts that count starts are
//! written before anything is started: a script that is still open for writing
//! when another thread's process starts cannot be run (`ETXTBSY`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use seatline_companion::startup::{self, Startup};
use seatline_companion::{client, config};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn data_dir(what: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "seatline-start-{what}-{}",
        &config::random_token().unwrap()[..8]
    ))
}

/// A companion that logs each start, then becomes the real one.
#[cfg(unix)]
fn counting_companion(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let (script, log) = (dir.join("counting-companion"), dir.join("starts.log"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ >> '{}'\nexec '{}' serve\n",
            log.display(),
            env!("CARGO_BIN_EXE_seatline-companion"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

/// The processes the counting companion started.
#[cfg(unix)]
fn starts(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Stops the brokers a case started, so that none is left running.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // Stops processes this test started.
fn stop(log: &Path) {
    for pid in starts(log) {
        let _ = std::process::Command::new("kill").arg(&pid).status();
    }
}

fn connect_all(root: &Path, startup: &Startup, clients: usize) -> Vec<Duration> {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(clients));
    let threads: Vec<_> = (0..clients)
        .map(|_| {
            let (root, startup, barrier) = (root.to_owned(), startup.clone(), barrier.clone());
            std::thread::spawn(move || {
                let runtime = runtime();
                barrier.wait();
                let began = Instant::now();
                let stream = runtime.block_on(client::connect_with(&root, &startup));
                let took = began.elapsed();
                // Keep the connection open until every client has one.
                barrier.wait();
                drop(stream.expect("the client found no broker"));
                took
            })
        })
        .collect();
    threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect()
}

#[test]
fn clients_that_need_a_broker_together_start_one_and_every_failure_recovers() {
    let scripts = data_dir("scripts");
    #[cfg(unix)]
    let (counting, log) = counting_companion(&scripts);
    #[cfg(not(unix))]
    let counting = PathBuf::from(env!("CARGO_BIN_EXE_seatline-companion"));
    // The brokers these cases start leave two seconds after they are idle, on
    // every platform, so that none outlives the test.
    let startup = Startup {
        executable: Some(counting.clone()),
        environment: vec![("SEATLINE_BROKER_IDLE_SECS".into(), "2".into())],
        ..Startup::default()
    };

    // Sixteen clients with no broker: one companion is started, one broker
    // serves, and every client connects.
    let root = data_dir("herd");
    let took = connect_all(&root, &startup, 16);
    #[cfg(unix)]
    assert_eq!(starts(&log).len(), 1, "one start for sixteen clients");
    assert!(
        matches!(
            config::lock(&root, "broker.lock"),
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "a broker holds its lock"
    );
    assert!(took.iter().all(|took| *took < Duration::from_secs(5)));
    #[cfg(unix)]
    stop(&log);
    let _ = std::fs::remove_dir_all(&root);
    #[cfg(unix)]
    std::fs::write(&log, "").unwrap();

    // The claim is held by a client that never starts a broker: another takes
    // it over after the delay, and starts one.
    let root = data_dir("held");
    let holder = startup::claim(&root).unwrap().expect("the claim was free");
    let patient = Startup {
        takeover_after: Duration::from_millis(300),
        ..startup.clone()
    };
    let began = Instant::now();
    connect_all(&root, &patient, 1);
    assert!(began.elapsed() >= Duration::from_millis(300));
    #[cfg(unix)]
    assert_eq!(starts(&log).len(), 1);
    drop(holder);
    #[cfg(unix)]
    stop(&log);
    let _ = std::fs::remove_dir_all(&root);
    #[cfg(unix)]
    std::fs::write(&log, "").unwrap();

    // The holder goes away without a broker: the waiting client claims it and
    // starts one at once, long before the takeover delay.
    let root = data_dir("gone");
    let holder = startup::claim(&root).unwrap().expect("the claim was free");
    let releasing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(holder);
    });
    let began = Instant::now();
    connect_all(&root, &startup, 1);
    releasing.join().unwrap();
    assert!(began.elapsed() < startup.takeover_after);
    #[cfg(unix)]
    assert_eq!(starts(&log).len(), 1);
    #[cfg(unix)]
    stop(&log);
    let _ = std::fs::remove_dir_all(&root);

    // A claim file a dead client left behind is nothing: the lock is gone with it.
    let root = data_dir("stale");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(startup::CLAIM_FILE), "left behind").unwrap();
    connect_all(&root, &startup, 1);
    #[cfg(unix)]
    stop(&log);
    let _ = std::fs::remove_dir_all(&root);

    // A companion that cannot be started is reported at once, not after the budget.
    let root = data_dir("missing");
    let broken = Startup {
        executable: Some(root.join("no-such-companion")),
        ..Startup::default()
    };
    let began = Instant::now();
    let error = runtime()
        .block_on(client::connect_with(&root, &broken))
        .map(drop)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
    assert!(began.elapsed() < Duration::from_secs(2));
    // And the claim it held is free for the next client.
    assert!(startup::claim(&root).unwrap().is_some());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&scripts);
}
