//! Phase telemetry through the real broker binary (B-01): off unless the
//! environment asks for a file, and then one broker record, one handshake
//! record per connection and one record per request, in that order, with
//! nothing private in them.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::{Stream, prelude::*};
use seatline_companion::{client, config, telemetry, wire};
use serde_json::{Value, json};

struct Broker(Child);

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    root: PathBuf,
    broker: Option<Broker>,
}

impl Fixture {
    #[allow(clippy::disallowed_methods)] // Exercise the installed binary, never provider execution.
    fn start(telemetry_file: Option<&Path>) -> Self {
        let root = std::env::temp_dir().join(format!(
            "seatline-telemetry-{}",
            &config::random_token().unwrap()[..12]
        ));
        let exe = env!("CARGO_BIN_EXE_seatline-companion");
        let authorized = Command::new(exe)
            .env("SEATLINE_DATA_DIR", &root)
            .args(["authorize", "test_app", "codex"])
            .output()
            .unwrap();
        assert!(authorized.status.success());
        // Match client startup: choose the endpoint before spawning the broker.
        // On Windows, simultaneous first lookups can create different pipe IDs.
        client::socket_name(&root).unwrap();
        // No provider can be found, so the request does not depend on one
        // being installed on the machine that runs the test.
        let empty = root.join("no-providers");
        std::fs::create_dir_all(&empty).unwrap();
        let mut command = Command::new(exe);
        command
            .env("SEATLINE_DATA_DIR", &root)
            .env("TEST_APP_PROVIDER_PATH", &empty)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if let Some(path) = telemetry_file {
            command.env(telemetry::FILE_VARIABLE, path);
        }
        Self {
            root,
            broker: Some(Broker(command.spawn().unwrap())),
        }
    }

    /// One connection that asks for a status and reads it to its end.
    async fn status(&self, id: &str) -> Vec<Value> {
        self.call(id, "status", Value::Null).await
    }

    async fn call(&self, id: &str, method: &str, params: Value) -> Vec<Value> {
        let give_up = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match Stream::connect(client::socket_name(&self.root).unwrap()).await {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < give_up => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("the broker never listened: {error}"),
            }
        };
        let grant = config::load_grant(&self.root, "test_app").unwrap();
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
        wire::write_frame(
            &mut stream,
            &json!({"id":id,"provider":"codex","method":method,"params":params}),
        )
        .await
        .unwrap();
        let mut events = Vec::new();
        loop {
            let event =
                tokio::time::timeout(Duration::from_secs(10), wire::read_frame(&mut stream))
                    .await
                    .expect("the status never finished")
                    .unwrap();
            let done = event["event"]["type"] == "completed" || event["event"]["type"] == "failed";
            events.push(event);
            if done {
                return events;
            }
        }
    }
}

#[test]
fn preparation_and_readiness_use_the_real_wire_and_preserve_missing_provider_status() {
    let fixture = Fixture::start(None);
    runtime().block_on(async {
        for method in ["prepare", "readiness"] {
            let events = fixture
                .call(method, method, json!({"mode":"cached","max_age_ms":30000}))
                .await;
            assert_eq!(events[0]["event"]["status"]["availability"], "not_found");
            assert_eq!(events[0]["event"]["status"]["authentication"], "unknown");
            assert_eq!(events[0]["event"]["status"]["readiness"]["source"], "fresh");
            assert_eq!(events.last().unwrap()["event"]["type"], "completed");
        }
        let malformed = fixture
            .call(
                "bad-freshness",
                "prepare",
                json!({"mode":"cached","max_age_ms":-1}),
            )
            .await;
        assert_eq!(malformed[0]["event"]["reason"], "INVALID_REQUEST");
    });
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.broker.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn lines(path: &Path, at_least: usize) -> Vec<Value> {
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is JSON"))
            .collect();
        if lines.len() >= at_least || Instant::now() >= give_up {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_broker_writes_its_configuration_each_handshake_and_each_request() {
    let file = std::env::temp_dir().join(format!(
        "seatline-telemetry-{}.jsonl",
        &config::random_token().unwrap()[..12]
    ));
    let fixture = Fixture::start(Some(&file));
    let events = runtime().block_on(fixture.status("probe-1"));
    assert_eq!(events.last().unwrap()["event"]["type"], "completed");

    let lines = lines(&file, 3);
    let _ = std::fs::remove_file(&file);
    let kinds: Vec<&str> = lines
        .iter()
        .map(|line| line["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["broker", "connection", "request"], "{lines:?}");

    let broker = &lines[0];
    assert_eq!(broker["protocol"], 1);
    assert_eq!(broker["limits"]["max_provider_running"], 2);
    assert_eq!(broker["limits"]["max_running"], 8);

    let handshake = &lines[1];
    assert_eq!(handshake["app"], "test_app");
    assert!(handshake["handshake_us"].is_u64());

    let request = &lines[2];
    assert_eq!(request["request"], "probe-1");
    assert_eq!(request["connection"], handshake["connection"]);
    assert_eq!(request["method"], "status");
    assert_eq!(request["provider"], "codex");
    assert_eq!(request["outcome"], "completed");
    let total = request["total_us"].as_u64().unwrap();
    let phases: u64 = request["phases_us"]
        .as_object()
        .unwrap()
        .values()
        .map(|value| value.as_u64().unwrap())
        .sum();
    assert_eq!(phases, total, "the phases tile the request: {request}");

    // Neither the credential nor anything the provider said is in the file.
    let grant = config::load_grant(&fixture.root, "test_app").unwrap();
    let text = serde_json::to_string(&lines).unwrap();
    assert!(!text.contains(&grant.token));
}

#[test]
fn nothing_is_written_unless_the_environment_asks() {
    let fixture = Fixture::start(None);
    let events = runtime().block_on(fixture.status("probe-1"));
    assert_eq!(events.last().unwrap()["event"]["type"], "completed");
    std::thread::sleep(Duration::from_millis(200));
    let stray: Vec<_> = walk(&fixture.root)
        .into_iter()
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                || path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("telemetry"))
        })
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        }
        found.push(path);
    }
    found
}
