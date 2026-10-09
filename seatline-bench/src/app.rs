//! One simulated application: a process that runs its requests against the
//! broker the way an app's client does, one authenticated connection per
//! request, and times each from its own side.
//!
//! It is a process of its own so that a cold start goes through the shipped
//! client's `connect`, which starts the broker when none is running, and so
//! that several applications compete the way separate programs do.

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use seatline_companion::client::RemoteProvider;
use seatline_companion::config::{self, Grant};
use seatline_companion::remote::RemoteClient;
use seatline_companion::{PROTOCOL_VERSION, client, wire};
use seatline_core::exchange::{Exchange, Scripted, Timeouts, Update};
use seatline_core::protocol::{Capabilities, Capability};
use seatline_core::readiness::Freshness;
use seatline_core::turn::{Message, Role, SessionPolicy, ToolPolicy, Turn};
use seatline_providers::Provider;
use serde_json::{Value, json};

use crate::workload::{CACHED_MAX_AGE_MS, Method, Req, Sample, Spec, Via};

/// How long one request may take, from connecting to its end.
const REQUEST_LIMIT: Duration = Duration::from_secs(300);

/// Runs `spec`: says it is ready, waits to be told to start, then makes every
/// request in turn and prints a [`Sample`] for each.
pub fn run(spec: Spec) -> io::Result<()> {
    let grant = config::load_grant(&spec.root, &spec.app)?;
    let mut out = io::stdout().lock();
    writeln!(out, "{}", json!({"ready": true}))?;
    out.flush()?;
    let mut go = String::new();
    io::stdin().lock().read_line(&mut go)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut handle: Option<String> = None;
    // The application's one client, for the requests that share it: it
    // connects on the first of them, which is a warm-up in a warm scenario.
    let shared = RemoteClient::with_root(&spec.app, spec.root.clone());
    for req in &spec.requests {
        if spec.stop_file.as_ref().is_some_and(|stop| stop.exists()) {
            break;
        }
        if req.gap_ms > 0 {
            std::thread::sleep(Duration::from_millis(req.gap_ms));
        }
        let sample = match req.via {
            Via::Wire => runtime.block_on(request(&spec, &grant, req, &mut handle)),
            Via::Adapter => through_adapter(&spec, req, &mut handle, None),
            Via::Shared => through_adapter(&spec, req, &mut handle, Some(&shared)),
        };
        writeln!(out, "{}", serde_json::to_string(&sample)?)?;
        out.flush()?;
    }
    writeln!(out, "{}", json!({"done": true}))?;
    out.flush()
}

/// Readiness evidence up to the most a request may accept.
fn cached() -> Freshness {
    Freshness::Cached {
        max_age_ms: CACHED_MAX_AGE_MS,
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn turn(req: &Req, handle: &Option<String>) -> Turn {
    Turn {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: req.prompt.clone(),
        }],
        model: None,
        reasoning_effort: None,
        service_tier: None,
        tools: ToolPolicy::None,
        session: if req.persistent {
            SessionPolicy::Persistent
        } else {
            SessionPolicy::Ephemeral
        },
        continuation: if req.resume { handle.clone() } else { None },
        cleanup_group: None,
        check_sign_in: req.check_sign_in,
    }
}

fn blank(spec: &Spec, req: &Req) -> Sample {
    Sample {
        id: req.id.clone(),
        app: spec.app.clone(),
        method: req.method,
        measured: req.measured,
        via: req.via,
        outcome: "error".to_owned(),
        detail: None,
        connect_us: None,
        handshake_us: None,
        prepare_us: None,
        submit_to_launched_us: None,
        submit_to_started_us: None,
        submit_to_first_text_us: None,
        submit_to_complete_us: None,
        total_us: 0,
        broker: None,
    }
}

/// What the shipped client needs to know about a provider: not how to run it.
struct Metadata(String);

impl Provider for Metadata {
    fn id(&self) -> &str {
        &self.0
    }

    fn timeouts(&self) -> Timeouts {
        Timeouts {
            start: REQUEST_LIMIT,
            idle: REQUEST_LIMIT,
            max_turn: REQUEST_LIMIT,
            stop_grace: Duration::from_secs(2),
        }
    }

    fn capabilities(&self) -> Capabilities {
        let supported = Capability::Supported;
        Capabilities {
            streaming: supported,
            continuation: supported,
            web_search: supported,
            model_selection: supported,
            reasoning_effort: seatline_core::protocol::Capability::Unknown,
            service_tier: seatline_core::protocol::Capability::Unknown,
            cancellation: supported,
            tool_isolation: supported,
        }
    }

    fn supports_persistent_session(&self) -> bool {
        true
    }

    fn status(&self) -> Box<dyn Exchange> {
        Box::new(Scripted::new([]))
    }

    fn send(&self, _turn: Turn) -> Box<dyn Exchange> {
        Box::new(Scripted::new([]))
    }
}

/// What the wire request for `req` carries, as the broker reads it: a turn for
/// a send, readiness to accept for a prepare, both for a send that checks first.
fn wire_params(req: &Req, handle: &Option<String>) -> Value {
    match req.method {
        Method::Send => json!(turn(req, handle)),
        Method::Status => Value::Null,
        Method::Prepare => json!(cached()),
        Method::SendReady => json!({"turn": turn(req, handle), "freshness": cached()}),
    }
}

/// The call on `provider` that `req` is: what an application's adapter makes of
/// the same request.
fn begin(provider: &dyn Provider, req: &Req, handle: &Option<String>) -> Box<dyn Exchange> {
    match req.method {
        Method::Send => provider.send(turn(req, handle)),
        Method::Status => provider.status(),
        Method::Prepare => provider.prepare(cached()),
        Method::SendReady => provider.send_with_readiness(turn(req, handle), cached()),
    }
}

/// The request as an application's adapter makes it, through `RemoteProvider`.
/// Without a `client` the exchange it returns starts a thread, a runtime and a
/// connection of its own; with one it is a request on the application's shared
/// connection. Either way none of that can be timed apart: only what shows
/// from outside.
fn through_adapter(
    spec: &Spec,
    req: &Req,
    handle: &mut Option<String>,
    client: Option<&RemoteClient>,
) -> Sample {
    let mut sample = blank(spec, req);
    let metadata = Metadata(spec.provider.clone());
    let provider = match client {
        Some(client) => RemoteProvider::with_client(&spec.app, client.clone(), &metadata),
        None => RemoteProvider::new(&spec.app, &metadata),
    };
    let begun = Instant::now();
    let mut exchange = begin(&provider, req, handle);
    let deadline = begun + REQUEST_LIMIT;
    loop {
        let Some(update) = exchange.next(deadline) else {
            sample.detail = Some("timeout".to_owned());
            break;
        };
        let since = micros(begun.elapsed());
        match update {
            Update::Launched => {
                sample.submit_to_launched_us.get_or_insert(since);
            }
            Update::Started => {
                sample.submit_to_started_us.get_or_insert(since);
            }
            Update::Delta(text) if !text.is_empty() => {
                sample.submit_to_first_text_us.get_or_insert(since);
            }
            Update::Session(session) => *handle = Some(session),
            Update::Completed => {
                sample.submit_to_complete_us = Some(since);
                sample.outcome = "completed".to_owned();
                break;
            }
            Update::Stopped => {
                sample.submit_to_complete_us = Some(since);
                sample.outcome = "stopped".to_owned();
                break;
            }
            Update::Failed(failure) => {
                sample.submit_to_complete_us = Some(since);
                sample.outcome = "failed".to_owned();
                sample.detail = Some(failure.reason.to_owned());
                break;
            }
            _ => {}
        }
    }
    sample.total_us = micros(begun.elapsed());
    sample
}

async fn request(spec: &Spec, grant: &Grant, req: &Req, handle: &mut Option<String>) -> Sample {
    let mut sample = blank(spec, req);
    let begun = Instant::now();
    let result = tokio::time::timeout(
        REQUEST_LIMIT,
        exchange(spec, grant, req, handle, &mut sample),
    )
    .await;
    sample.total_us = micros(begun.elapsed());
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => sample.detail = Some(format!("io:{:?}", error.kind())),
        Err(_) => sample.detail = Some("timeout".to_owned()),
    }
    sample
}

/// One connection, one request, read to its end. Fills `sample` as it goes, so
/// that what was seen before a break is kept.
async fn exchange(
    spec: &Spec,
    grant: &Grant,
    req: &Req,
    handle: &mut Option<String>,
    sample: &mut Sample,
) -> io::Result<()> {
    let begun = Instant::now();
    let mut stream = client::connect(&spec.root).await?;
    let connected = Instant::now();
    sample.connect_us = Some(micros(connected - begun));
    wire::write_frame(
        &mut stream,
        &json!({"version": PROTOCOL_VERSION, "app": grant.app, "token": grant.token}),
    )
    .await?;
    let hello = wire::read_frame(&mut stream).await?;
    let ready = Instant::now();
    sample.handshake_us = Some(micros(ready - connected));
    sample.prepare_us = Some(micros(ready - begun));
    if hello["type"] == "busy" {
        sample.outcome = "failed".to_owned();
        sample.detail = Some("QUEUE_FULL".to_owned());
        return Ok(());
    }
    if hello["type"] != "ready" {
        sample.detail = Some("authorization_refused".to_owned());
        return Ok(());
    }

    let params = wire_params(req, handle);
    let submitted = Instant::now();
    wire::write_frame(
        &mut stream,
        &json!({"id": req.id, "provider": spec.provider, "method": req.method.name(), "params": params}),
    )
    .await?;
    loop {
        let frame = wire::read_frame(&mut stream).await?;
        if frame["id"] != req.id.as_str() {
            continue;
        }
        let since = micros(submitted.elapsed());
        let event = &frame["event"];
        match event["type"].as_str().unwrap_or_default() {
            "launched" => {
                sample.submit_to_launched_us.get_or_insert(since);
            }
            "started" => {
                sample.submit_to_started_us.get_or_insert(since);
            }
            "delta" if event["text"].as_str().is_some_and(|text| !text.is_empty()) => {
                sample.submit_to_first_text_us.get_or_insert(since);
            }
            "session" => *handle = event["handle"].as_str().map(str::to_owned),
            "completed" => {
                sample.submit_to_complete_us = Some(since);
                sample.outcome = "completed".to_owned();
                return Ok(());
            }
            "stopped" => {
                sample.submit_to_complete_us = Some(since);
                sample.outcome = "stopped".to_owned();
                return Ok(());
            }
            "failed" => {
                sample.submit_to_complete_us = Some(since);
                sample.outcome = "failed".to_owned();
                sample.detail = event["reason"].as_str().map(str::to_owned);
                return Ok(());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use seatline_core::readiness::MAX_AGE;

    use super::*;

    /// A provider that only records which call a request became.
    struct Recorder(RefCell<Vec<String>>);

    impl Recorder {
        fn log(&self, call: String) -> Box<dyn Exchange> {
            self.0.borrow_mut().push(call);
            Box::new(Scripted::new([]))
        }
    }

    impl Provider for Recorder {
        fn id(&self) -> &str {
            "recorder"
        }

        fn timeouts(&self) -> Timeouts {
            Metadata(String::new()).timeouts()
        }

        fn capabilities(&self) -> Capabilities {
            Metadata(String::new()).capabilities()
        }

        fn status(&self) -> Box<dyn Exchange> {
            self.log("status".to_owned())
        }

        fn prepare(&self, freshness: Freshness) -> Box<dyn Exchange> {
            self.log(format!("prepare {freshness:?}"))
        }

        fn send_with_readiness(&self, turn: Turn, freshness: Freshness) -> Box<dyn Exchange> {
            self.log(format!(
                "send_ready {freshness:?} check_sign_in={}",
                turn.check_sign_in
            ))
        }

        fn send(&self, turn: Turn) -> Box<dyn Exchange> {
            self.log(format!("send check_sign_in={}", turn.check_sign_in))
        }
    }

    fn req(method: Method) -> Req {
        Req {
            id: "r".to_owned(),
            method,
            prompt: "p".to_owned(),
            persistent: false,
            resume: false,
            check_sign_in: false,
            gap_ms: 0,
            measured: true,
            via: Via::Adapter,
        }
    }

    #[test]
    fn a_request_becomes_the_call_an_adapter_makes_with_the_most_cached_readiness_it_may_ask_for() {
        let recorder = Recorder(RefCell::new(Vec::new()));
        for method in [
            Method::Send,
            Method::Status,
            Method::Prepare,
            Method::SendReady,
        ] {
            begin(&recorder, &req(method), &None);
        }
        let freshness = "Cached { max_age_ms: 30000 }";
        assert_eq!(
            *recorder.0.borrow(),
            [
                "send check_sign_in=false".to_owned(),
                "status".to_owned(),
                format!("prepare {freshness}"),
                format!("send_ready {freshness} check_sign_in=false"),
            ]
        );
        // The documented "most a request may ask for" is the broker's own cap.
        assert_eq!(u128::from(CACHED_MAX_AGE_MS), MAX_AGE.as_millis());
        assert_eq!(cached().max_age(), MAX_AGE);
    }

    #[test]
    fn a_wire_request_carries_what_the_broker_reads_from_it() {
        let read = |params: &Value| serde_json::from_value::<Freshness>(params.clone());
        assert_eq!(
            read(&wire_params(&req(Method::Prepare), &None)).unwrap(),
            cached()
        );
        let ready = wire_params(&req(Method::SendReady), &None);
        assert_eq!(read(&ready["freshness"]).unwrap(), cached());
        let sent = serde_json::from_value::<Turn>(ready["turn"].clone()).unwrap();
        assert_eq!(sent.messages[0].text, "p");
        assert!(serde_json::from_value::<Turn>(wire_params(&req(Method::Send), &None)).is_ok());
        assert_eq!(wire_params(&req(Method::Status), &None), Value::Null);
    }
}
