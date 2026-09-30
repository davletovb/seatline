use interprocess::local_socket::tokio::prelude::*;
use seatline_companion::{client, config, wire};
use serde_json::json;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A broker with nothing connected exits, so the next start runs the installed
/// binary; one with a connection stays up however long that lasts.
#[test]
#[allow(clippy::disallowed_methods)] // Exercise the installed binary, never provider execution.
fn an_idle_broker_exits_and_a_connected_one_stays() {
    let root = std::env::temp_dir().join(format!(
        "seatline-lifecycle-{}",
        &config::random_token().unwrap()[..12]
    ));
    let exe = env!("CARGO_BIN_EXE_seatline-companion");
    assert!(
        Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
            .args(["authorize", "test_app", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut broker = Broker(
        Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
            .env("SEATLINE_BROKER_IDLE_SECS", "1")
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(connected) = interprocess::local_socket::tokio::Stream::connect(
                client::socket_name(&root).unwrap(),
            )
            .await
            {
                stream = Some(connected);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut stream = stream.expect("broker listening");
        let grant = config::load_grant(&root, "test_app").unwrap();
        wire::write_frame(
            &mut stream,
            &json!({"version":1,"app":"test_app","token":grant.token}),
        )
        .await
        .unwrap();
        assert_eq!(
            wire::read_frame(&mut stream).await.unwrap()["type"],
            "ready"
        );

        // Well past the idle limit, but still connected.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            broker.0.try_wait().unwrap().is_none(),
            "the broker left while a connection was open"
        );

        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = broker.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the idle broker never exited");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(status.success());
        #[cfg(unix)]
        assert!(!root.join("broker.sock").exists());
    });
    drop(broker);
    std::fs::remove_dir_all(root).unwrap();
}
