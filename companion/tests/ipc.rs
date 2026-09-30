use interprocess::local_socket::tokio::prelude::*;
use seatline_companion::{client, config, wire};
use serde_json::json;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[allow(clippy::disallowed_methods)] // Exercise the installed binary, never provider execution.
fn real_ipc_authentication_singleton_and_revocation() {
    let root =
        std::env::temp_dir().join(format!("seatline-ipc-{}", config::random_token().unwrap()));
    let exe = env!("CARGO_BIN_EXE_seatline-companion");
    let authorized = Command::new(exe)
        .env("SEATLINE_DATA_DIR", &root)
        .args(["authorize", "test_app", "codex"])
        .output()
        .unwrap();
    assert!(authorized.status.success());
    let mut broker = Broker(
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
        let mut connected = None;
        for _ in 0..100 {
            if let Ok(stream) = interprocess::local_socket::tokio::Stream::connect(
                client::socket_name(&root).unwrap(),
            )
            .await
            {
                connected = Some(stream);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut stream = connected.expect("broker listening");
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
        let second = Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
            .arg("serve")
            .output()
            .unwrap();
        assert!(second.status.success());
        assert!(broker.0.try_wait().unwrap().is_none());
        let mut denied =
            interprocess::local_socket::tokio::Stream::connect(client::socket_name(&root).unwrap())
                .await
                .unwrap();
        wire::write_frame(
            &mut denied,
            &json!({"version":1,"app":"test_app","token":"0".repeat(64)}),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), wire::read_frame(&mut denied))
                .await
                .unwrap()
                .is_err()
        );
        std::fs::remove_file(config::app_path(&root, "test_app").unwrap()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), wire::read_frame(&mut stream))
                .await
                .unwrap()
                .is_err()
        );
    });
    drop(broker);
    std::fs::remove_dir_all(root).unwrap();
}
