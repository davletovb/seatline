//! Reproducible measurements of what the runtime adds to a request, in states
//! that are named, so that two results can be compared without guessing what
//! was warm. See `docs/performance-measurement.md` for the method.
//!
//! ```text
//! seatline-bench run [--scenario NAME]... [--samples N] [--warmup N] [--gap-ms N]
//!                    [--label TEXT] [--output FILE.json] [--markdown FILE.md]
//!                    [--companion PATH] [--fake-provider PATH]
//!                    [--live PROVIDER] [--policy FILE] [--scratch DIR] [--keep]
//! seatline-bench report FILE.json
//! seatline-bench compare BEFORE.json AFTER.json
//! seatline-bench overhead [--turns N] [--updates N] [--rounds N]
//! seatline-bench hub [--idle-ms N] [--samples N] [--label TEXT] [--output FILE.json]
//! ```
//!
//! `app` is internal: the harness runs itself as each simulated application.

mod app;
mod environment;
mod hub_overhead;
mod lab;
mod overhead;
mod report;
mod scenarios;
mod stats;
mod workload;

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use lab::{Lab, Settings};
use report::{Parameters, Report};
use scenarios::Params;
use seatline_companion::scheduling;

const USAGE: &str = "usage:
  seatline-bench run [--scenario NAME]... [--samples N] [--warmup N] [--gap-ms N]
                     [--label TEXT] [--output FILE.json] [--markdown FILE.md]
                     [--companion PATH] [--fake-provider PATH] [--live PROVIDER]
                     [--policy FILE] [--scratch DIR] [--keep]
  seatline-bench report FILE.json
  seatline-bench compare BEFORE.json AFTER.json
  seatline-bench overhead [--turns N] [--updates N] [--rounds N]
  seatline-bench hub [--idle-ms N] [--samples N] [--label TEXT] [--output FILE.json]

scenarios: cold-broker cold-three-app warm-send warm-send-adapter warm-send-shared
           warm-send-probe prepared-send warm-status resumed-context reused-process
           short-isolated-paced three-app-short short-contended

--live runs the real provider CLI installed on this machine and sends it real
prompts. It needs SEATLINE_BENCH_LIVE=1 as well, and uses a little of the
account's quota. It uses your own HOME and provider configuration, so that you
are signed in.

--policy FILE writes FILE as the owner's scheduling.json into every scratch
broker's data directory, so that one run can be made with a setting on and the
next with it off.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("seatline-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run_scenarios(&args[1..]),
        Some("app") if args.len() == 2 => {
            let spec =
                serde_json::from_slice(&std::fs::read(&args[1])?).map_err(io::Error::other)?;
            app::run(spec)
        }
        // How a client starts the companion: see `Lab::count_companion_starts`.
        Some("serve") if args.len() == 1 => lab::shim_serve(),
        Some("overhead") => overhead::command(&args[1..]),
        Some("hub") => hub_overhead::command(&args[1..]),
        Some("hub-child") => hub_overhead::child(&args[1..]),
        Some("report") if args.len() == 2 => {
            print!("{}", report::markdown(&read_report(&args[1])?));
            Ok(())
        }
        Some("compare") if args.len() == 3 => {
            print!(
                "{}",
                report::compare(&read_report(&args[1])?, &read_report(&args[2])?)
            );
            Ok(())
        }
        _ => Err(io::Error::other(USAGE)),
    }
}

fn read_report(path: &str) -> io::Result<Report> {
    serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)
}

/// The option's value, or an error naming the option.
fn value<'a>(args: &mut impl Iterator<Item = &'a String>, flag: &str) -> io::Result<&'a String> {
    args.next()
        .ok_or_else(|| io::Error::other(format!("{flag} needs a value")))
}

fn number<T: std::str::FromStr>(text: &str, flag: &str) -> io::Result<T> {
    text.parse()
        .map_err(|_| io::Error::other(format!("{flag} needs a number, not `{text}`")))
}

/// An executable that was built next to this one, as `cargo build` puts them.
fn sibling(name: &str) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }))
}

fn require(path: &Path, what: &str) -> io::Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{what} not found at {}; build it with `cargo build --release -p seatline-companion -p seatline-bench` or pass its path",
            path.display()
        )))
    }
}

fn run_scenarios(args: &[String]) -> io::Result<()> {
    let mut chosen: Vec<String> = Vec::new();
    let (mut samples, mut warmup, mut gap_ms) = (None, None, 40);
    let mut label = String::from("seatline-bench");
    let (mut output, mut markdown): (Option<String>, Option<String>) = (None, None);
    let (mut companion, mut fake_provider, mut live): (
        Option<PathBuf>,
        Option<PathBuf>,
        Option<String>,
    ) = (None, None, None);
    let mut keep = false;
    let mut scratch: Option<PathBuf> = None;
    let mut policy: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--scenario" => chosen.push(value(&mut it, flag)?.clone()),
            "--samples" => samples = Some(number(value(&mut it, flag)?, flag)?),
            "--warmup" => warmup = Some(number(value(&mut it, flag)?, flag)?),
            "--gap-ms" => gap_ms = number(value(&mut it, flag)?, flag)?,
            "--label" => label = value(&mut it, flag)?.clone(),
            "--output" => output = Some(value(&mut it, flag)?.clone()),
            "--markdown" => markdown = Some(value(&mut it, flag)?.clone()),
            "--companion" => companion = Some(PathBuf::from(value(&mut it, flag)?)),
            "--fake-provider" => fake_provider = Some(PathBuf::from(value(&mut it, flag)?)),
            "--live" => live = Some(value(&mut it, flag)?.clone()),
            "--policy" => policy = Some(PathBuf::from(value(&mut it, flag)?)),
            "--keep" => keep = true,
            "--scratch" => scratch = Some(PathBuf::from(value(&mut it, flag)?)),
            other => {
                return Err(io::Error::other(format!(
                    "unknown option `{other}`\n{USAGE}"
                )));
            }
        }
    }
    if let Some(provider) = &live {
        if !matches!(provider.as_str(), "codex" | "claude" | "gemini" | "grok") {
            return Err(io::Error::other(format!("`{provider}` is not a provider")));
        }
        if std::env::var("SEATLINE_BENCH_LIVE").as_deref() != Ok("1") {
            return Err(io::Error::other(
                "--live sends real prompts to the installed provider and uses its quota; set SEATLINE_BENCH_LIVE=1 to confirm",
            ));
        }
    }
    let params = Params {
        // A live provider is slow and metered: fewer, by default.
        samples: samples.unwrap_or(if live.is_some() { 5 } else { 30 }),
        warmup: warmup.unwrap_or(if live.is_some() { 1 } else { 3 }),
        gap_ms,
    };
    if params.samples == 0 {
        return Err(io::Error::other("--samples must be at least 1"));
    }
    let available: &[&str] = if live.is_some() {
        &scenarios::LIVE
    } else {
        &scenarios::ALL
    };
    if chosen.is_empty() {
        chosen = available.iter().map(|name| (*name).to_owned()).collect();
    }
    for name in &chosen {
        if !available.contains(&name.as_str()) {
            return Err(io::Error::other(format!(
                "no scenario `{name}` {}",
                if live.is_some() {
                    "for a live provider"
                } else {
                    "(see --help)"
                }
            )));
        }
    }

    let companion = match companion {
        Some(path) => path,
        None => sibling("seatline-companion")?,
    };
    let fake_provider = match fake_provider {
        Some(path) => path,
        None => sibling("seatline-bench-fake-provider")?,
    };
    require(&companion, "the companion")?;
    if live.is_none() {
        require(&fake_provider, "the fake provider")?;
    }
    // Checked with the broker's own rules now, rather than by a broker that
    // refuses to start after a live run has begun.
    let policy = policy
        .map(|path| {
            let bytes = std::fs::read(&path)?;
            let parsed: scheduling::Policy = serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::other(format!("--policy: {error}")))?;
            parsed
                .validate()
                .map_err(|error| io::Error::other(format!("--policy: {error}")))?;
            Ok::<_, io::Error>(bytes)
        })
        .transpose()?;
    let settings = Settings {
        companion: std::fs::canonicalize(&companion)?,
        fake_provider: std::fs::canonicalize(&fake_provider).unwrap_or(fake_provider),
        harness: std::env::current_exe()?,
        live: live.clone(),
        scratch: scratch.unwrap_or_else(lab::default_scratch),
        keep,
        policy,
    };

    let mut results = Vec::new();
    let mut broker = None;
    let mut failures = 0;
    for name in &chosen {
        eprintln!("running {name} ...");
        // A scenario that cannot run is kept in the report with its reason and
        // the run goes on: what the others measured, on a live provider at the
        // cost of its quota, is not thrown away with it.
        let outcome = Lab::new(&settings).and_then(|mut lab| {
            let scenario = scenarios::run(&mut lab, name, params);
            broker = broker.take().or_else(|| lab.broker_record.clone());
            scenario
        });
        match outcome {
            Ok(scenario) => results.push(scenario),
            Err(error) => {
                eprintln!("{name} failed: {error}");
                failures += 1;
                results.push(scenarios::failed(name, &error));
            }
        }
    }

    // How long a broker idles before exiting is the harness's own setting, and
    // differs by scenario (a cold start lets it leave after a second): it is not
    // part of what the broker ran with.
    if let Some(limits) = broker
        .as_mut()
        .and_then(|record| record.get_mut("limits"))
        .and_then(Value::as_object_mut)
    {
        limits.remove("idle_exit_ms");
    }

    let report = Report {
        schema: report::SCHEMA,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        mode: live
            .as_ref()
            .map_or("fake".to_owned(), |p| format!("live:{p}")),
        label,
        environment: environment::capture(&settings.companion),
        broker,
        providers: providers(&settings),
        parameters: Parameters {
            samples: params.samples,
            warmup: params.warmup,
            gap_ms: params.gap_ms,
            // The file as the owner wrote it: small, and no path or credential.
            policy: settings
                .policy
                .as_deref()
                .and_then(|bytes| serde_json::from_slice(bytes).ok()),
        },
        scenarios: results,
        limitations: limitations(live.is_some(), params),
    };
    if let Some(path) = &output {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&report).map_err(io::Error::other)?,
        )?;
    }
    let text = report::markdown(&report);
    if let Some(path) = &markdown {
        std::fs::write(path, &text)?;
    }
    if output.is_none() && markdown.is_none() {
        print!("{text}");
    }
    if failures > 0 {
        // The report is written either way; the exit status says it is not whole.
        return Err(io::Error::other(format!(
            "{failures} of {} scenarios failed; the report keeps the others and says why",
            chosen.len()
        )));
    }
    Ok(())
}

fn providers(settings: &Settings) -> Value {
    match &settings.live {
        None => json!({
            "kind": "fake",
            "persona": "codex",
            "behavior": "exec chosen by the first word of the question (`slow` takes about 300 ms, anything else answers at once); sign-in probe succeeds",
            "note": "deterministic: adds no network or model latency",
        }),
        Some(provider) => {
            let executable = match provider.as_str() {
                "gemini" => "agy",
                other => other,
            };
            let version = seatline_core::turn::Namespace::fixed("bench-a")
                .ok()
                .and_then(|namespace| {
                    seatline_platform::discovery::installed(
                        &seatline_platform::layout::Layout::new(namespace),
                    )
                    .find(executable)
                })
                .and_then(|path| environment::output(path, &["--version"]));
            json!({"kind": "live", "provider": provider, "version": version})
        }
    }
}

fn limitations(live: bool, params: Params) -> Vec<String> {
    let mut notes = vec![
        "Broker marks are taken when its hub thread observes an update, so they include the hub's polling interval (5 ms when idle) and cannot resolve differences smaller than that; client timings include the socket and the application's own scheduling.".to_owned(),
        "Only compare results from the same machine, build profile and mode; a shared or virtualized machine adds noise of its own. `compare` warns when these differ.".to_owned(),
        "`prepare`, in the `client_prepare_us` metric, is connecting and the handshake. A provider's own readiness check is measured by `warm-status`, by the sign-in probe in `warm-send-probe`, and by `prepared-send`, which prepares first and so runs no probe in the send.".to_owned(),
        "Resumed context continues a conversation in a fresh provider process; it is not process reuse, which is unsupported until a persistent-provider adapter exists (E-02).".to_owned(),
        "A scenario's first request after warm-up is still the first of its kind in this run's broker; a genuinely cold machine (empty page cache, first start after installation) is not reproduced.".to_owned(),
    ];
    if params.samples < 20 {
        notes.push(format!(
            "With {} measured requests per application the 95th percentile is the maximum; use 20 or more before quoting it.",
            params.samples
        ));
    }
    if live {
        notes.push("A live run includes the provider's start-up, the network and the model, which Seatline cannot see. Only the broker's `queue_wait`, `cleanup` and handshake phases are purely local; compare with the fake-provider run of the same scenario to estimate the rest.".to_owned());
    } else {
        notes.push("These runs use a fake provider: they measure what Seatline adds (broker, scheduling, process start, IPC) and contain no network or model latency, and a real provider's own start-up time is not in them. They say nothing about how soon a real answer begins.".to_owned());
    }
    notes
}
