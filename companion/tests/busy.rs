use interprocess::local_socket::tokio::{Stream, prelude::*};
use seatline_companion::{client, config, wire};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// At the connection limit a new client is told so, instead of being left to
/// guess why the connection closed.
#[test]
#[allow(clippy::disallowed_methods)] // Exercise the installed binary, never provider execution.
fn a_client_over_the_connection_limit_is_told_the_broker_is_busy() {
    let root = std::env::temp_dir().join(format!(
        "seatline-busy-{}",
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
    let broker = Broker(
        Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
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
        let name = || client::socket_name(&root).unwrap();
        let mut first = None;
        for _ in 0..100 {
            if let Ok(stream) = Stream::connect(name()).await {
                first = Some(stream);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Unauthenticated connections hold their slot until the 5 s deadline.
        let mut held = vec![first.expect("broker listening")];
        while held.len() < 32 {
            held.push(Stream::connect(name()).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut refused = Stream::connect(name()).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(2), wire::read_frame(&mut refused))
            .await
            .expect("the broker must answer")
            .unwrap();
        assert_eq!(frame["type"], "busy");
        drop(held);
    });
    // Windows will not delete files a running broker still holds open.
    drop(broker);
    let _ = std::fs::remove_dir_all(root);
}
