//! Linux hub-only measurements: no sockets, external services or real prompts.
//! The same measurement driver can run against an earlier companion revision.
use crate::{
    lab::{Lab, Settings},
    stats::summarize,
};
use seatline_companion::{config, hub};
use serde_json::{Value, json};
use std::io;
use std::time::{Duration, Instant};

const APPS: [&str; 3] = ["bench-a", "bench-b", "bench-c"];

#[allow(clippy::disallowed_methods)] // Only this benchmark binary and its deterministic fake provider are launched.
pub fn command(args: &[String]) -> io::Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::other(
            "hub CPU/wakeup measurement needs Linux /proc",
        ));
    }
    let (mut idle_ms, mut samples, mut label, mut output) = (2000, 30, "hub-only".to_owned(), None);
    let mut flags = args.iter();
    while let Some(flag) = flags.next() {
        let value = flags
            .next()
            .ok_or_else(|| io::Error::other("hub option needs a value"))?;
        match flag.as_str() {
            "--idle-ms" => idle_ms = crate::number(value, flag)?,
            "--samples" => samples = crate::number(value, flag)?,
            "--label" => label = value.clone(),
            "--output" => output = Some(value.clone()),
            _ => return Err(io::Error::other("unknown hub option")),
        }
    }
    if !(100..=30_000).contains(&idle_ms) || !(1..=1000).contains(&samples) {
        return Err(io::Error::other("hub measurement outside bounds"));
    }
    let settings = Settings {
        companion: crate::sibling("seatline-companion")?,
        fake_provider: crate::sibling("seatline-bench-fake-provider")?,
        harness: std::env::current_exe()?,
        live: None,
        scratch: crate::lab::default_scratch(),
        keep: false,
    };
    crate::require(&settings.fake_provider, "fake provider")?;
    crate::require(&settings.companion, "companion")?;
    let mut lab = Lab::new(&settings)?;
    let instance = lab.instance(&APPS)?;
    let result = std::process::Command::new(&settings.harness)
        .args(["hub-child", &idle_ms.to_string(), &samples.to_string()])
        .envs(lab.environment(&instance, &APPS, 120)?)
        .env_remove("CODEX_HOME")
        .env_remove("CODEX_SQLITE_HOME")
        .env_remove("CODEX_CA_CERTIFICATE")
        .output()?;
    if !result.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&result.stderr).into_owned(),
        ));
    }
    let mut report: Value = serde_json::from_slice(&result.stdout)?;
    report["label"] = json!(label);
    let (probes, generations) = lab.fake_invocations();
    report["fake_counts"] = json!({"probes":probes,"generations":generations});
    let report = serde_json::to_string_pretty(&report)?;
    if let Some(output) = output {
        std::fs::write(output, &report)?;
    }
    println!("{report}");
    Ok(())
}

fn counters() -> io::Result<(u64, u64)> {
    let (mut cpu_ns, mut switches) = (0, 0);
    for task in std::fs::read_dir("/proc/self/task")? {
        let task = task?.path();
        let stat = std::fs::read_to_string(task.join("schedstat"))?;
        cpu_ns += stat
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(io::Error::other)?;
        for line in std::fs::read_to_string(task.join("status"))?.lines() {
            if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
                switches += value.trim().parse::<u64>().map_err(io::Error::other)?;
            }
        }
    }
    Ok((cpu_ns, switches))
}

fn idle(ms: u64) -> io::Result<Value> {
    let before = counters()?;
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(ms));
    let elapsed_us = micros(start.elapsed());
    let after = counters()?;
    Ok(
        json!({"elapsed_us":elapsed_us,"cpu_ns":after.0.saturating_sub(before.0),"voluntary_context_switches":after.1.saturating_sub(before.1)}),
    )
}

pub fn child(args: &[String]) -> io::Result<()> {
    if args.len() != 2 {
        return Err(io::Error::other("hub-child needs idle-ms and samples"));
    }
    let idle_ms: u64 = crate::number(&args[0], "idle-ms")?;
    let samples: usize = crate::number(&args[1], "samples")?;
    let root = config::data_dir()?;
    let input = hub::start(root.clone())?;
    std::thread::sleep(Duration::from_millis(100)); // exclude initialization
    let disconnected = idle(idle_ms)?;
    let mut outputs = Vec::new();
    for (i, app) in APPS.iter().enumerate() {
        let (output, mut receive) = tokio::sync::mpsc::channel(64);
        input
            .send(hub::Command::Open {
                connection: i as u64,
                grant: Box::new(config::load_grant(&root, app)?),
                output,
            })
            .map_err(io::Error::other)?;
        if receive.blocking_recv().is_none() {
            return Err(io::Error::other("hub closed during open"));
        }
        outputs.push(receive);
    }
    std::thread::sleep(Duration::from_millis(100));
    let connected = idle(idle_ms)?;
    let mut text = Vec::new();
    let mut done = Vec::new();
    for i in 0..samples + 3 {
        let app = i % APPS.len();
        let begin = Instant::now();
        input.send(hub::Command::Request { connection:app as u64, value:json!({"id":format!("r{i}"),"provider":"codex","method":"send","params":{"system":null,"messages":[{"role":"user","text":"short"}],"model":null,"tools":"none","session":"ephemeral","continuation":null,"cleanup_group":null,"check_sign_in":false}}) }).map_err(io::Error::other)?;
        let mut first = None;
        while let Some(value) = outputs[app].blocking_recv() {
            match value["event"]["type"].as_str() {
                Some("delta") if first.is_none() => {
                    first = Some(micros(begin.elapsed()));
                }
                Some("completed") => break,
                Some("failed" | "stopped") => {
                    return Err(io::Error::other(format!(
                        "fake hub turn failed: {}",
                        value["event"]["reason"]
                    )));
                }
                _ => {}
            }
        }
        if i >= 3 {
            text.push(first.ok_or_else(|| io::Error::other("fake hub turn had no text"))?);
            done.push(micros(begin.elapsed()));
        }
    }
    // A fresh readiness check while both provider generation slots are busy.
    send(&input, 1, "ready-long-b", "slow", false)?;
    until_started(&mut outputs[1])?;
    send(&input, 2, "ready-long-c", "slow", false)?;
    until_started(&mut outputs[2])?;
    let begin = Instant::now();
    input.send(hub::Command::Request { connection:0, value:json!({"id":"ready-contended","provider":"codex","method":"status","params":null}) }).map_err(io::Error::other)?;
    finish(&mut outputs[0], begin)?;
    let readiness_contended_us = micros(begin.elapsed());
    finish(&mut outputs[1], begin)?;
    finish(&mut outputs[2], begin)?;

    // A bounded backlog, with interactive work arriving after six queued long
    // requests. All eight long turns must still complete after the short one.
    send(&input, 1, "burst-b-0", "slow", false)?;
    until_started(&mut outputs[1])?;
    send(&input, 2, "burst-c-0", "slow", false)?;
    until_started(&mut outputs[2])?;
    for app in [1, 2] {
        for n in 1..4 {
            send(&input, app, &format!("burst-{app}-{n}"), "slow", false)?;
        }
    }
    let begin = Instant::now();
    send(&input, 0, "burst-short", "short", true)?;
    let (burst_text, burst_done) = finish(&mut outputs[0], begin)?;
    for output in &mut outputs[1..] {
        for _ in 0..4 {
            finish(output, begin)?;
        }
    }
    let burst_all_done = micros(begin.elapsed());
    drop(outputs);
    drop(input);
    std::thread::sleep(Duration::from_millis(30));
    println!(
        "{}",
        serde_json::to_string(&json!({
            "schema":1,"measurement":"hub only; no IPC; deterministic fake provider; telemetry off",
            "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"idle_requested_ms":idle_ms,
            "idle_disconnected":disconnected,"idle_three_connections":connected,
            "warmup":3,"submit_to_text_us":summarize(&text),"submit_to_done_us":summarize(&done),
            "samples_us":{"text":text,"done":done},
            "readiness_contended_us":readiness_contended_us,
            "interactive_burst":{"short_text_us":burst_text,"short_done_us":burst_done,"all_eight_long_done_us":burst_all_done}
        }))?
    );
    Ok(())
}

fn micros(took: Duration) -> u64 {
    u64::try_from(took.as_micros()).unwrap_or(u64::MAX)
}

fn send(
    input: &std::sync::mpsc::SyncSender<hub::Command>,
    connection: u64,
    id: &str,
    prompt: &str,
    interactive: bool,
) -> io::Result<()> {
    input.send(hub::Command::Request { connection, value:json!({"id":id,"provider":"codex","method":"send","scheduling":{"interactive":interactive},"params":{"system":null,"messages":[{"role":"user","text":prompt}],"model":null,"tools":"none","session":"ephemeral","continuation":null,"cleanup_group":null,"check_sign_in":false}}) }).map_err(io::Error::other)
}

fn until_started(output: &mut tokio::sync::mpsc::Receiver<Value>) -> io::Result<()> {
    while let Some(value) = output.blocking_recv() {
        match value["event"]["type"].as_str() {
            Some("started") => return Ok(()),
            Some("failed" | "stopped" | "completed") => break,
            _ => {}
        }
    }
    Err(io::Error::other("long fake turn did not start"))
}

fn finish(
    output: &mut tokio::sync::mpsc::Receiver<Value>,
    begin: Instant,
) -> io::Result<(Option<u64>, u64)> {
    let mut first = None;
    while let Some(value) = output.blocking_recv() {
        match value["event"]["type"].as_str() {
            Some("delta") if first.is_none() => {
                first = Some(micros(begin.elapsed()));
            }
            Some("completed") => return Ok((first, micros(begin.elapsed()))),
            Some("failed" | "stopped") => {
                return Err(io::Error::other(format!(
                    "fake hub turn failed: {}",
                    value["event"]["reason"]
                )));
            }
            _ => {}
        }
    }
    Err(io::Error::other("fake turn failed or connection closed"))
}
