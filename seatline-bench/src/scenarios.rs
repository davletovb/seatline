//! What the harness measures. Each scenario is one state of the system, named
//! for what is warm and what is fresh in it, so that no number is quoted for a
//! state it was not measured in: a second request on a warm broker is not a
//! faster first request, and the report never lets one stand in for the other.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::lab::{Lab, read_telemetry, wait_until_stopped};
use crate::report::{AppResult, Counts, Scenario};
use crate::stats::{Summary, summarize};
use crate::workload::{Broker, Method, Req, Sample, Spec, Via};

/// How the harness pauses between a paced application's requests, in
/// milliseconds, and how many requests it measures.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub samples: usize,
    pub warmup: usize,
    pub gap_ms: u64,
}

/// Every scenario, in the order they are run.
pub const ALL: [&str; 10] = [
    "cold-broker",
    "warm-send",
    "warm-send-adapter",
    "warm-send-probe",
    "warm-status",
    "resumed-context",
    "reused-process",
    "short-isolated-paced",
    "three-app-short",
    "short-contended",
];

/// The scenarios a live provider can be asked for. The competing-load ones
/// need requests of a known length, which only the fake provider has.
pub const LIVE: [&str; 7] = [
    "cold-broker",
    "warm-send",
    "warm-send-adapter",
    "warm-send-probe",
    "warm-status",
    "resumed-context",
    "reused-process",
];

/// How long the fake provider's `slow` behavior takes: two messages 300 ms
/// apart. It is the "long request" of the contended scenario.
const LONG_MS: u64 = 300;

struct Description {
    state: &'static str,
    text: &'static str,
}

fn describe(name: &str) -> Option<Description> {
    Some(match name {
        "cold-broker" => Description {
            state: "cold_broker_fresh_provider",
            text: "No broker is running. The application's client starts one, connects, and sends one request; the provider is a fresh process. Each sample uses a new broker and data directory. Preparation includes starting the broker.",
        },
        "warm-send" => Description {
            state: "warm_broker_fresh_provider",
            text: "A broker is already running. Each request is made on a new connection, as the shipped client does, and runs a fresh provider process. No sign-in probe.",
        },
        "warm-send-adapter" => Description {
            state: "warm_broker_fresh_provider",
            text: "As `warm-send`, but through the shipped `RemoteProvider` the way an application's adapter calls it, which starts a thread, a runtime and a connection for every exchange. Only what shows from outside is timed, from the call that starts the exchange, so connecting and the handshake are not reported apart.",
        },
        "warm-send-probe" => Description {
            state: "warm_broker_fresh_provider",
            text: "As `warm-send`, but each request asks the adapter to check the sign-in first, which runs the provider's own status command before the turn.",
        },
        "warm-status" => Description {
            state: "warm_broker_fresh_provider",
            text: "A running broker answering `status` requests: the readiness check on its own.",
        },
        "resumed-context" => Description {
            state: "resumed_context",
            text: "A running broker. Each request continues the provider-side conversation the previous one started, in a fresh provider process. Resuming a conversation is not reusing a process.",
        },
        "reused-process" => Description {
            state: "reused_process",
            text: "One provider process serving several requests.",
        },
        "short-isolated-paced" => Description {
            state: "warm_broker_fresh_provider",
            text: "One application makes short requests, paced, with the broker to itself. The reference for `short-contended`.",
        },
        "three-app-short" => Description {
            state: "warm_broker_fresh_provider",
            text: "Three applications, as three processes, start together and each makes short requests back to back.",
        },
        "short-contended" => Description {
            state: "warm_broker_fresh_provider",
            text: "As `short-isolated-paced`, while two other applications keep the provider's slots busy with long requests (about 300 ms each) for as long as the short one is measured.",
        },
        _ => return None,
    })
}

fn request(id: String, prompt: &str) -> Req {
    Req {
        id,
        method: Method::Send,
        prompt: prompt.to_owned(),
        persistent: false,
        resume: false,
        check_sign_in: false,
        gap_ms: 0,
        measured: true,
        via: Via::Wire,
    }
}

/// What a short question is: the fake provider answers any first word it
/// does not know; a live one is asked something it can answer in one word.
fn short_prompt(live: bool) -> &'static str {
    if live {
        "Reply with the single word: ok"
    } else {
        "answers hello"
    }
}

fn ids(scenario: &str, app: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("{scenario}-{app}-{index}"))
        .collect()
}

/// `warmup` requests that are not measured, then `samples` that are.
fn sequence(
    scenario: &str,
    app: &str,
    live: bool,
    params: Params,
    shape: impl Fn(&mut Req),
) -> Vec<Req> {
    ids(scenario, app, params.warmup + params.samples)
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            let mut req = request(id, short_prompt(live));
            req.measured = index >= params.warmup;
            shape(&mut req);
            req
        })
        .collect()
}

struct Plan {
    app: &'static str,
    role: &'static str,
    requests: Vec<Req>,
}

impl Plan {
    /// A background load runs until the measured applications are done.
    fn is_background(&self) -> bool {
        self.role == "long"
    }
}

pub fn run(lab: &mut Lab, name: &str, params: Params) -> io::Result<Scenario> {
    let described =
        describe(name).ok_or_else(|| io::Error::other(format!("no scenario `{name}`")))?;
    let live = lab.settings.live.is_some();
    let mut scenario = Scenario {
        name: name.to_owned(),
        state: described.state.to_owned(),
        status: "measured".to_owned(),
        description: described.text.to_owned(),
        reason: None,
        apps: Vec::new(),
        counts: Counts::default(),
    };
    let plans = match name {
        "cold-broker" => return cold(lab, scenario, params),
        "reused-process" => {
            scenario.status = "unsupported".to_owned();
            scenario.reason = Some("No provider process is reused across requests: every `send` starts a fresh one, and a persistent-provider adapter does not exist yet (tracker item E-02). There is nothing to measure, so nothing is reported; resumed context (`resumed-context`) is a different state.".to_owned());
            return Ok(scenario);
        }
        "warm-send" => vec![Plan {
            app: "bench-a",
            role: "single",
            requests: sequence(name, "bench-a", live, params, |_| {}),
        }],
        "warm-send-adapter" => vec![Plan {
            app: "bench-a",
            role: "single",
            requests: sequence(name, "bench-a", live, params, |req| req.via = Via::Adapter),
        }],
        "warm-send-probe" => vec![Plan {
            app: "bench-a",
            role: "single",
            requests: sequence(name, "bench-a", live, params, |req| {
                req.check_sign_in = true
            }),
        }],
        "warm-status" => vec![Plan {
            app: "bench-a",
            role: "single",
            requests: sequence(name, "bench-a", live, params, |req| {
                req.method = Method::Status
            }),
        }],
        "resumed-context" => {
            let mut requests = sequence(name, "bench-a", live, params, |req| {
                req.persistent = true;
                req.resume = true;
            });
            // The first request starts the conversation the rest continue.
            let mut prime = request(format!("{name}-bench-a-prime"), short_prompt(live));
            prime.persistent = true;
            prime.measured = false;
            requests.insert(0, prime);
            vec![Plan {
                app: "bench-a",
                role: "single",
                requests,
            }]
        }
        "short-isolated-paced" => vec![Plan {
            app: "bench-a",
            role: "short",
            requests: sequence(name, "bench-a", live, params, |req| {
                req.gap_ms = params.gap_ms
            }),
        }],
        "three-app-short" => ["bench-a", "bench-b", "bench-c"]
            .into_iter()
            .map(|app| Plan {
                app,
                role: "short",
                requests: sequence(name, app, live, params, |_| {}),
            })
            .collect(),
        "short-contended" => {
            // Enough long requests to outlast the short application even if
            // each of its requests waits out a whole one; the load is stopped
            // when the short application is done, not when it runs out.
            let window_ms =
                (params.warmup + params.samples) as u64 * (params.gap_ms + LONG_MS + 15);
            let longs = (window_ms / LONG_MS + 3) as usize;
            // Two applications that began together would finish together and
            // leave the short request the same wait every time: the second
            // starts half a request later, as independent programs drift.
            let long = |app: &'static str, offset_ms: u64| Plan {
                app,
                role: "long",
                requests: ids(name, app, longs)
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| {
                        let mut req = request(id, "slow hello");
                        if index == 0 {
                            req.gap_ms = offset_ms;
                        }
                        req
                    })
                    .collect(),
            };
            vec![
                Plan {
                    app: "bench-a",
                    role: "short",
                    requests: sequence(name, "bench-a", live, params, |req| {
                        req.gap_ms = params.gap_ms
                    }),
                },
                long("bench-b", 0),
                long("bench-c", LONG_MS / 2),
            ]
        }
        other => return Err(io::Error::other(format!("no scenario `{other}`"))),
    };

    let apps: Vec<&str> = plans.iter().map(|plan| plan.app).collect();
    let instance = lab.instance(&apps)?;
    let broker = lab.start_broker(&instance, &apps)?;
    let stop_file = lab.scratch.join("stop-background");
    let mut children = Vec::new();
    for plan in &plans {
        let spec = Spec {
            root: instance.root.clone(),
            app: plan.app.to_owned(),
            provider: lab.settings.provider().to_owned(),
            requests: plan.requests.clone(),
            stop_file: plan.is_background().then(|| stop_file.clone()),
        };
        children.push(lab.spawn_app(&instance, &apps, crate::lab::WARM_IDLE_SECS, &spec)?);
    }
    // Every application is ready: start them together.
    for child in &mut children {
        child.go()?;
    }
    // The applications being measured finish first; then the background load,
    // which has been keeping them company all along, is told to stop.
    let mut results: Vec<Option<Vec<Sample>>> = plans.iter().map(|_| None).collect();
    let mut children: Vec<Option<_>> = children.into_iter().map(Some).collect();
    for background in [false, true] {
        if background {
            std::fs::write(&stop_file, b"")?;
        }
        for (index, plan) in plans.iter().enumerate() {
            if plan.is_background() == background {
                let child = children[index].take().expect("each child finishes once");
                results[index] = Some(child.finish()?);
            }
        }
    }
    let results: Vec<Vec<Sample>> = results.into_iter().flatten().collect();
    let expected: usize = results.iter().map(Vec::len).sum();
    let records = wait_for_records(&instance.telemetry, expected);
    drop(broker);
    lab.remember_broker(&records);

    let mut requests_total = 0;
    for (plan, mut samples) in plans.iter().zip(results) {
        join_for(&mut samples, &records);
        requests_total += samples.len() as u64;
        let result = app_result(plan.app, plan.role, &samples);
        ensure_some_completed(name, &result)?;
        scenario.apps.push(result);
    }
    scenario.counts = counts(lab, &scenario.apps, requests_total);
    Ok(scenario)
}

/// A new broker for every sample, started by the application's own client.
fn cold(lab: &mut Lab, mut scenario: Scenario, params: Params) -> io::Result<Scenario> {
    let live = lab.settings.live.is_some();
    let apps = ["bench-a"];
    let requests = sequence("cold-broker", "bench-a", live, params, |_| {});
    let mut all = Vec::new();
    for req in &requests {
        let instance = lab.instance(&apps)?;
        let spec = Spec {
            root: instance.root.clone(),
            app: "bench-a".to_owned(),
            provider: lab.settings.provider().to_owned(),
            requests: vec![req.clone()],
            stop_file: None,
        };
        // A broker that starts for this request leaves by itself a second after
        // it is idle, so that the next sample finds none.
        let mut child = lab.spawn_app(&instance, &apps, 1, &spec)?;
        child.go()?;
        let mut samples = child.finish()?;
        let records = wait_for_records(&instance.telemetry, 1);
        lab.remember_broker(&records);
        join_for(&mut samples, &records);
        all.append(&mut samples);
        wait_until_stopped(&instance.root)?;
    }
    let result = app_result("bench-a", "single", &all);
    ensure_some_completed("cold-broker", &result)?;
    scenario.apps.push(result);
    scenario.counts = counts(lab, &scenario.apps, requests.len() as u64);
    Ok(scenario)
}

fn counts(lab: &Lab, apps: &[AppResult], requests_total: u64) -> Counts {
    let measured = apps.iter().flat_map(|app| app.samples.iter());
    let (mut probes, mut launches) = (0, 0);
    for sample in measured {
        if let Some(broker) = &sample.broker {
            probes += broker.probes;
            launches += broker.launches;
        }
    }
    let (fake_probes, fake_turns) = if lab.settings.live.is_none() {
        let (probes, turns) = lab.fake_invocations();
        (Some(probes), Some(turns))
    } else {
        (None, None)
    };
    Counts {
        requests_total,
        fake_probes,
        fake_turns,
        broker_probes: probes,
        broker_launches: launches,
    }
}

/// Waits for the broker's telemetry to hold `expected` request records, which
/// its writer thread may still be catching up on, and returns what it has when
/// they are all there or a few seconds have passed.
fn wait_for_records(path: &std::path::Path, expected: usize) -> Vec<Value> {
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        let records = read_telemetry(path);
        let have = records
            .iter()
            .filter(|record| record["kind"] == "request")
            .count();
        if have >= expected || Instant::now() >= give_up {
            return records;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What the broker said about a connection's handshake, by connection number.
fn handshakes(records: &[Value]) -> HashMap<u64, u64> {
    records
        .iter()
        .filter(|record| record["kind"] == "connection")
        .filter_map(|record| {
            Some((
                record["connection"].as_u64()?,
                record["handshake_us"].as_u64()?,
            ))
        })
        .collect()
}

fn broker_of(record: &Value, handshakes: &HashMap<u64, u64>) -> Broker {
    let phases: BTreeMap<String, u64> = record["phases_us"]
        .as_object()
        .map(|phases| {
            phases
                .iter()
                .filter_map(|(name, us)| Some((name.clone(), us.as_u64()?)))
                .collect()
        })
        .unwrap_or_default();
    Broker {
        handshake_us: record["connection"]
            .as_u64()
            .and_then(|c| handshakes.get(&c).copied()),
        outcome: record["outcome"].as_str().unwrap_or_default().to_owned(),
        probes: record["probes"].as_u64().unwrap_or(0),
        launches: record["launches"].as_u64().unwrap_or(0),
        total_us: record["total_us"].as_u64().unwrap_or(0),
        phases_us: phases,
    }
}

/// Attaches the broker's account of each request to the application's, by
/// request ID.
pub fn join(samples: &mut [Sample], records: &[Value]) {
    let handshakes = handshakes(records);
    let requests: HashMap<&str, &Value> = records
        .iter()
        .filter(|record| record["kind"] == "request")
        .filter_map(|record| Some((record["request"].as_str()?, record)))
        .collect();
    for sample in samples {
        if let Some(record) = requests.get(sample.id.as_str()) {
            sample.broker = Some(broker_of(record, &handshakes));
        }
    }
}

/// [`join`] for requests made through `RemoteProvider`, which names every
/// request `request`. One application making one request at a time leaves the
/// broker's records in the order the requests were made, so they are matched
/// by position; if the counts differ the order cannot be trusted and nothing is
/// matched.
pub fn join_in_order(samples: &mut [Sample], records: &[Value]) {
    let handshakes = handshakes(records);
    let theirs: Vec<&Value> = records
        .iter()
        .filter(|record| record["kind"] == "request" && record["request"] == "request")
        .collect();
    if theirs.len() != samples.len() {
        return;
    }
    for (sample, record) in samples.iter_mut().zip(theirs) {
        sample.broker = Some(broker_of(record, &handshakes));
    }
}

/// Joins by whichever means the samples' client allows.
fn join_for(samples: &mut [Sample], records: &[Value]) {
    if !samples.is_empty() && samples.iter().all(|sample| sample.via == Via::Adapter) {
        join_in_order(samples, records);
    } else {
        join(samples, records);
    }
}

/// A scenario in which nothing completed measured nothing: say why, instead of
/// reporting an empty table.
fn ensure_some_completed(scenario: &str, result: &AppResult) -> io::Result<()> {
    if result.completed > 0 || result.samples.is_empty() {
        return Ok(());
    }
    let why = result
        .samples
        .iter()
        .find_map(|sample| sample.detail.clone())
        .unwrap_or_else(|| "no reason was reported".to_owned());
    Err(io::Error::other(format!(
        "{scenario}: none of {}'s {} measured requests completed (first failure: {why})",
        result.app,
        result.samples.len()
    )))
}

fn app_result(app: &str, role: &str, samples: &[Sample]) -> AppResult {
    let measured: Vec<Sample> = samples.iter().filter(|s| s.measured).cloned().collect();
    let completed = measured.iter().filter(|s| s.outcome == "completed").count();
    AppResult {
        app: app.to_owned(),
        role: role.to_owned(),
        completed,
        failed: measured.len() - completed,
        metrics_us: metrics(&measured),
        samples: measured,
    }
}

/// The summaries of what the measured, completed requests showed. A metric a
/// request did not have, such as time to text for a status check, simply has
/// fewer samples.
pub fn metrics(measured: &[Sample]) -> BTreeMap<String, Summary> {
    let done: Vec<&Sample> = measured
        .iter()
        .filter(|s| s.outcome == "completed")
        .collect();
    let mut out = BTreeMap::new();
    let mut add = |name: String, values: Vec<u64>| {
        if let Some(summary) = summarize(&values) {
            out.insert(name, summary);
        }
    };
    add(
        "client_connect_us".into(),
        done.iter().filter_map(|s| s.connect_us).collect(),
    );
    add(
        "client_handshake_us".into(),
        done.iter().filter_map(|s| s.handshake_us).collect(),
    );
    add(
        "client_prepare_us".into(),
        done.iter().filter_map(|s| s.prepare_us).collect(),
    );
    add(
        "client_total_us".into(),
        done.iter().map(|s| s.total_us).collect(),
    );
    // From the application's own start of the request, preparation included:
    // the one figure that is the same thing over the wire and through an
    // adapter, which cannot time its connect apart.
    add(
        "client_start_to_first_text_us".into(),
        done.iter()
            .filter_map(|s| Some(s.prepare_us.unwrap_or(0) + s.submit_to_first_text_us?))
            .collect(),
    );
    add(
        "client_submit_to_launched_us".into(),
        done.iter()
            .filter_map(|s| s.submit_to_launched_us)
            .collect(),
    );
    add(
        "client_submit_to_started_us".into(),
        done.iter().filter_map(|s| s.submit_to_started_us).collect(),
    );
    add(
        "client_submit_to_first_text_us".into(),
        done.iter()
            .filter_map(|s| s.submit_to_first_text_us)
            .collect(),
    );
    add(
        "client_submit_to_complete_us".into(),
        done.iter()
            .filter_map(|s| s.submit_to_complete_us)
            .collect(),
    );
    add(
        "broker_handshake_us".into(),
        done.iter()
            .filter_map(|s| s.broker.as_ref()?.handshake_us)
            .collect(),
    );
    add(
        "broker_total_us".into(),
        done.iter()
            .filter_map(|s| Some(s.broker.as_ref()?.total_us))
            .collect(),
    );
    for phase in [
        "queue_wait",
        "sign_in_probe",
        "provider_init",
        "first_text",
        "completion",
        "cleanup",
    ] {
        add(
            format!("broker_{phase}_us"),
            done.iter()
                .filter_map(|s| s.broker.as_ref()?.phases_us.get(phase).copied())
                .collect(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn sample(id: &str, prepare: u64, text: Option<u64>) -> Sample {
        Sample {
            id: id.into(),
            app: "bench-a".into(),
            method: Method::Send,
            measured: true,
            outcome: "completed".into(),
            detail: None,
            via: Via::Wire,
            connect_us: Some(1),
            handshake_us: Some(2),
            prepare_us: Some(prepare),
            submit_to_launched_us: None,
            submit_to_started_us: None,
            submit_to_first_text_us: text,
            submit_to_complete_us: Some(9),
            total_us: prepare + 9,
            broker: None,
        }
    }

    #[test]
    fn every_scenario_is_described() {
        for name in ALL {
            assert!(describe(name).is_some(), "{name}");
        }
        assert!(LIVE.iter().all(|name| ALL.contains(name)));
    }

    #[test]
    fn warm_up_requests_are_not_measured_and_every_id_is_unique() {
        let params = Params {
            samples: 3,
            warmup: 2,
            gap_ms: 0,
        };
        let requests = sequence("s", "bench-a", false, params, |_| {});
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests.iter().map(|r| r.measured).collect::<Vec<_>>(),
            [false, false, true, true, true]
        );
        let unique: HashSet<&str> = requests.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn the_broker_s_account_is_matched_to_the_request_by_id_and_connection() {
        let records: Vec<Value> = [
            r#"{"kind":"connection","connection":3,"handshake_us":120}"#,
            r#"{"kind":"request","connection":3,"request":"r-1","outcome":"completed","probes":1,"launches":1,"total_us":900,"phases_us":{"queue_wait":10,"provider_init":300}}"#,
            r#"{"kind":"request","connection":9,"request":"someone-else","outcome":"completed","probes":0,"launches":0,"total_us":1,"phases_us":{}}"#,
        ]
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
        let mut samples = vec![sample("r-1", 5, Some(7)), sample("r-2", 5, None)];
        join_for(&mut samples, &records);
        let broker = samples[0].broker.as_ref().unwrap();
        assert_eq!(broker.handshake_us, Some(120));
        assert_eq!(
            (broker.probes, broker.launches, broker.total_us),
            (1, 1, 900)
        );
        assert_eq!(broker.phases_us["queue_wait"], 10);
        assert!(samples[1].broker.is_none(), "no record, nothing invented");
    }

    #[test]
    fn requests_through_the_adapter_are_matched_by_order_or_not_at_all() {
        let records: Vec<Value> = [
            r#"{"kind":"connection","connection":1,"handshake_us":50}"#,
            r#"{"kind":"request","connection":1,"request":"request","outcome":"completed","probes":0,"launches":1,"total_us":100,"phases_us":{"queue_wait":1}}"#,
            r#"{"kind":"connection","connection":2,"handshake_us":60}"#,
            r#"{"kind":"request","connection":2,"request":"request","outcome":"failed","probes":0,"launches":0,"total_us":200,"phases_us":{"queue_wait":2}}"#,
        ]
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
        let mut samples = vec![sample("a", 0, None), sample("b", 0, None)];
        for sample in &mut samples {
            sample.via = Via::Adapter;
        }
        join_for(&mut samples, &records);
        assert_eq!(samples[0].broker.as_ref().unwrap().total_us, 100);
        assert_eq!(samples[1].broker.as_ref().unwrap().handshake_us, Some(60));
        assert_eq!(samples[1].broker.as_ref().unwrap().outcome, "failed");

        // A record is missing: the order cannot be trusted, so nothing is matched.
        let mut samples = vec![
            sample("a", 0, None),
            sample("b", 0, None),
            sample("c", 0, None),
        ];
        for sample in &mut samples {
            sample.via = Via::Adapter;
        }
        join_for(&mut samples, &records);
        assert!(samples.iter().all(|sample| sample.broker.is_none()));
    }

    #[test]
    fn metrics_summarize_completed_measured_requests_only() {
        let mut failed = sample("f", 1_000_000, None);
        failed.outcome = "failed".into();
        let mut warm = sample("w", 1_000_000, None);
        warm.measured = false;
        let samples = [
            sample("a", 10, Some(100)),
            sample("b", 30, Some(300)),
            failed,
            warm,
        ];
        let metrics = metrics(
            &samples
                .iter()
                .filter(|s| s.measured)
                .cloned()
                .collect::<Vec<_>>(),
        );
        let prepare = metrics["client_prepare_us"];
        assert_eq!((prepare.n, prepare.max), (2, 30));
        assert_eq!(metrics["client_submit_to_first_text_us"].p50, 100);
        assert!(!metrics.contains_key("client_submit_to_launched_us"));
    }
}
