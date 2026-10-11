//! Direct Claude CLI experiments (I-05 / I-10), opt-in and independent of a broker.
//! Process supervision and environment policy are shared with Seatline.
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use seatline_core::discovery::SearchPath;
use seatline_core::process::{Event, Process, ProcessSpec};
use seatline_platform::{environment, private_fs};
use serde_json::{Value, json};

use crate::claude_report::{self, Row};

const ARGS: &[&str] = &[
    "-p",
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-mode",
    "default",
    "--strict-mcp-config",
    "--disallowedTools",
    "mcp__*",
    "--no-session-persistence",
];
const VARIANTS: &[&str] = &["base", "dynamic", "short", "padded"];
const SHORT: &str = "Answer the user's message directly.";
const SEARCH: &str = "Search the web before you answer the question below, and base your answer on what you find. Answer directly in two sentences.\n\nWhat is the latest stable version of the Rust programming language?";
const PROOFREADING: &[&str] = &[
    "Proofread this and return only the corrected text: Their going to the libary tomorow to study for there exams, and they dont no weather it will be open.",
    "Proofread this and return only the corrected text: The commitee have decided to postpone the meeting untill further notise, which effect everyones schedule.",
    "Proofread this and return only the corrected text: Me and my freind went too the store, but it was closed, so we goes home and watched a movie.",
    "Proofread this and return only the corrected text: Its important to remember that practise makes perfect, and that mistakes is how we learn.",
];
const EXIT_USAGE: &str = "claude-exit natural|term RUNS [--env NAME=VALUE ...] [--timeout SECONDS]\nCLAUDE_BIN overrides discovery; live runs require SEATLINE_BENCH_LIVE=1.\nterm sends SIGTERM to the process group on Unix; unavailable on Windows.\nTimeout defaults to 180 s, followed by 5 s stop grace. There is no kill measurement mode.";
const PROMPTS_USAGE: &str = "claude-prompts ROUNDS [--prompts FILE] [--short-prompt TEXT] [--variants base,dynamic,short,padded]\n  [--fresh-cwd] [--search] [--answers FILE] [--raw FILE] [--seed N] [--timeout SECONDS]\nclaude-prompts --from-raw FILE [--prompts FILE] [--search]\nDefaults: base,dynamic,short; seed 1; timeout 180 s. CLAUDE_BIN overrides discovery.\nLive runs require SEATLINE_BENCH_LIVE=1; --from-raw needs neither Claude nor opt-in.\nAnswer similarity is not a quality evaluation; compare your application's corpus.";

fn live_executable() -> io::Result<PathBuf> {
    if std::env::var("SEATLINE_BENCH_LIVE").as_deref() != Ok("1") {
        return Err(io::Error::other(
            "this sends real prompts and uses account quota; set SEATLINE_BENCH_LIVE=1 to confirm",
        ));
    }
    let path = std::env::var_os("CLAUDE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            SearchPath::new(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ))
            .find("claude")
        })
        .ok_or_else(|| io::Error::other("no `claude` on PATH; set CLAUDE_BIN"))?;
    std::fs::canonicalize(path)
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> io::Result<Self> {
        let base = crate::lab::default_scratch();
        private_fs::create_private_dir(&base)?;
        let path = private_fs::unique_child(&base, "claude");
        private_fs::create_private_dir(&path)?;
        Ok(Self(std::fs::canonicalize(path)?))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Turn<'a> {
    variant: &'a str,
    prompt: &'a str,
    cwd: &'a Path,
    scratch: &'a Path,
    search: bool,
    short: &'a str,
    timeout: Duration,
    term: bool,
    extra_env: &'a [(OsString, OsString)],
}

fn spec(executable: &Path, turn: &Turn<'_>) -> io::Result<ProcessSpec> {
    let host: Vec<_> = std::env::vars_os().collect();
    let inherited =
        environment::inherit(host.clone(), seatline_providers::claude::CLAUDE_VARIABLES);
    let mut spec = ProcessSpec::new(executable)
        .args(ARGS.iter().copied())
        .envs(inherited)
        .env("DISABLE_AUTOUPDATER", "1")
        .env(
            "PATH",
            environment::search_path_for(executable, environment::lookup(&host, "PATH")),
        )
        .envs(turn.extra_env.iter().cloned())
        .current_dir(turn.cwd);
    spec = if turn.search {
        spec.args(["--tools", "WebSearch", "--allowedTools", "WebSearch"])
    } else {
        spec.args(["--tools", ""])
    };
    match turn.variant {
        "base" => {}
        "dynamic" => spec = spec.arg("--exclude-dynamic-system-prompt-sections"),
        "short" | "padded" => {
            let path = private_fs::unique_child(turn.scratch, "system").with_extension("txt");
            let text = if turn.variant == "padded" {
                let padding = (1..=75).map(|n| format!("Reference note {n}: nothing in this note applies to the task, and it can be ignored.")).collect::<Vec<_>>().join("\n");
                format!("{}\n\n{padding}", turn.short)
            } else {
                turn.short.to_owned()
            };
            private_fs::write_private_file(&path, text.as_bytes())?;
            spec = spec.arg("--system-prompt-file").arg(path);
        }
        _ => return Err(io::Error::other("unknown variant")),
    }
    Ok(spec)
}

fn observe(row: &mut Row, event: &Value, now: f64) -> bool {
    match event["type"].as_str() {
        Some("system") if event["subtype"] == "init" => {
            row.ready_ms.get_or_insert(now);
        }
        Some("stream_event") => {
            let inner = &event["event"];
            if inner["type"] == "message_start" {
                row.answer_begun_ms.get_or_insert(now);
            }
            if inner["type"] == "content_block_delta" && inner["delta"]["type"] == "text_delta" {
                row.first_text_ms.get_or_insert(now);
                row.answer
                    .push_str(inner["delta"]["text"].as_str().unwrap_or(""));
            }
        }
        Some("result") => {
            row.result_ms.get_or_insert(now);
            row.usage = event.get("usage").filter(|u| u.is_object()).cloned();
            if event["is_error"] == true {
                row.error = Some(
                    event["result"]
                        .as_str()
                        .unwrap_or("Claude returned an error")
                        .chars()
                        .take(200)
                        .collect(),
                );
            }
            return true;
        }
        _ => {}
    }
    false
}

/// Drains both pipes while waiting. Deadlines cover stdin, output and exit;
/// dropping Process on any error stops and reaps the child.
fn run_once(executable: &Path, turn: &Turn<'_>) -> io::Result<(Row, Option<i32>, f64)> {
    let spec = spec(executable, turn)?;
    let started = Instant::now();
    let deadline = started + turn.timeout;
    let mut process = Process::spawn(&spec)?;
    let mut message = serde_json::to_vec(&json!({"type":"user", "message":{"role":"user", "content":[{"type":"text", "text":turn.prompt}]}})).map_err(io::Error::other)?;
    message.push(b'\n');
    match process.write(&message) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => return Err(e),
    }
    process.close_stdin();
    let mut row = Row {
        variant: turn.variant.to_owned(),
        ..Row::default()
    };
    let mut buffer = Vec::new();
    let mut stderr = Vec::new();
    let mut signalled = false;
    let exit = loop {
        match process.next_event(deadline) {
            Some(Event::Stdout(bytes)) => {
                let previous = buffer.len();
                buffer.extend(bytes);
                let now = started.elapsed().as_secs_f64() * 1000.0;
                let mut consumed = 0;
                for (index, byte) in buffer.iter().enumerate().skip(previous) {
                    if *byte != b'\n' {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_slice::<Value>(&buffer[consumed..index]) {
                        if observe(&mut row, &event, now) && turn.term && !signalled {
                            process.request_stop();
                            signalled = true;
                        }
                    }
                    consumed = index + 1;
                }
                buffer.drain(..consumed);
                if buffer.len() > 16 * 1024 * 1024 {
                    row.error = Some("Claude output line exceeds 16 MiB".to_owned());
                    break process.terminate(Duration::from_secs(5));
                }
            }
            Some(Event::Stderr(bytes)) => private_fs::keep_tail(&mut stderr, &bytes, 2000),
            Some(Event::Exited(exit)) => {
                // The old scripts dropped a last JSON line without a newline.
                if !buffer.is_empty() {
                    if let Ok(event) = serde_json::from_slice::<Value>(&buffer) {
                        observe(&mut row, &event, started.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                break exit;
            }
            None => {
                row.error = Some(format!("timed out after {} s", turn.timeout.as_secs()));
                break process.terminate(Duration::from_secs(5));
            }
        }
        // next_event may keep returning output after its deadline if a child
        // floods the pipes: enforce the wall-clock limit independently.
        if Instant::now() >= deadline {
            row.error = Some(format!("timed out after {} s", turn.timeout.as_secs()));
            break process.terminate(Duration::from_secs(5));
        }
    };
    let exited_ms = started.elapsed().as_secs_f64() * 1000.0;
    row.stderr = String::from_utf8_lossy(&stderr)
        .chars()
        .rev()
        .take(500)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if row.error.is_none() && row.result_ms.is_none() {
        row.error = Some("the CLI exited without a result: is it signed in?".to_owned());
    }
    if row.error.is_none() && !turn.term && !exit.status.is_some_and(|s| s.success()) {
        row.error = Some(format!("CLI exited unsuccessfully: {:?}", exit.status));
    }
    let code = exit.status.and_then(|status| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .or_else(|| status.signal().map(|signal| -signal))
        }
        #[cfg(not(unix))]
        status.code()
    });
    Ok((row, code, exited_ms))
}

fn timeout(text: &str) -> io::Result<Duration> {
    let seconds: u64 = crate::number(text, "--timeout")?;
    if !(1..=86400).contains(&seconds) {
        return Err(io::Error::other("--timeout must be 1..86400 seconds"));
    }
    Ok(Duration::from_secs(seconds))
}

pub fn exit_command(args: &[String]) -> io::Result<()> {
    if args == ["--help"] {
        println!("{EXIT_USAGE}");
        return Ok(());
    }
    let mut positional = Vec::new();
    let mut extra = Vec::new();
    let mut limit = Duration::from_secs(180);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--env" => {
                let item = crate::value(&mut it, flag)?;
                let (name, value) = item
                    .split_once('=')
                    .filter(|(n, _)| !n.is_empty() && !n.contains('\0'))
                    .ok_or_else(|| io::Error::other("--env needs NAME=VALUE"))?;
                if value.contains('\0') {
                    return Err(io::Error::other("--env value contains NUL"));
                }
                // Match Python's dictionary semantics for repeated names.
                extra.retain(|(n, _): &(OsString, OsString)| n != name);
                extra.push((OsString::from(name), OsString::from(value)));
            }
            "--timeout" => limit = timeout(crate::value(&mut it, flag)?)?,
            s if s.starts_with("--") => return Err(io::Error::other(EXIT_USAGE)),
            _ => positional.push(flag.as_str()),
        }
    }
    if positional.len() != 2 || !["natural", "term"].contains(&positional[0]) {
        return Err(io::Error::other(EXIT_USAGE));
    }
    let runs: usize = crate::number(positional[1], "RUNS")?;
    if runs == 0 {
        return Err(io::Error::other("RUNS must be at least 1"));
    }
    let term = positional[0] == "term";
    if term && !cfg!(unix) {
        return Err(io::Error::other(
            "term requires Unix process-group SIGTERM; use natural on Windows",
        ));
    }
    let executable = live_executable()?;
    let scratch = Scratch::new()?;
    let mut times = Vec::new();
    for index in 0..runs {
        let (row, code, exited) = run_once(
            &executable,
            &Turn {
                variant: "base",
                prompt: "Reply with the single word: ok",
                cwd: &scratch.0,
                scratch: &scratch.0,
                search: false,
                short: SHORT,
                timeout: limit,
                term,
                extra_env: &extra,
            },
        )?;
        if let Some(error) = row.error {
            return Err(io::Error::other(error));
        }
        let ms = exited - row.result_ms.ok_or_else(|| io::Error::other("no result"))?;
        times.push(ms);
        println!(
            "{} run {index}: result -> exit {ms:.0} ms (exit code {})",
            positional[0],
            code.map_or("unknown".to_owned(), |code| code.to_string())
        );
    }
    times.sort_by(f64::total_cmp);
    // The original exit tool used the upper-middle value for an even run count.
    println!(
        "{}: n={} min {:.0} median {:.0} max {:.0} ms",
        positional[0],
        times.len(),
        times[0],
        times[times.len() / 2],
        times[times.len() - 1]
    );
    Ok(())
}

#[derive(Debug)]
struct Options {
    rounds: Option<usize>,
    prompts: Option<String>,
    short: String,
    variants: Vec<String>,
    answers: Option<String>,
    raw: Option<String>,
    from_raw: Option<String>,
    seed: u64,
    timeout: Duration,
    fresh: bool,
    search: bool,
}
impl Options {
    fn parse(args: &[String]) -> io::Result<Self> {
        let mut options = Self {
            rounds: None,
            prompts: None,
            short: SHORT.to_owned(),
            variants: vec!["base".into(), "dynamic".into(), "short".into()],
            answers: None,
            raw: None,
            from_raw: None,
            seed: 1,
            timeout: Duration::from_secs(180),
            fresh: false,
            search: false,
        };
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--prompts" => options.prompts = Some(crate::value(&mut it, flag)?.clone()),
                "--short-prompt" => options.short = crate::value(&mut it, flag)?.clone(),
                "--variants" => {
                    options.variants = crate::value(&mut it, flag)?
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                }
                "--answers" => options.answers = Some(crate::value(&mut it, flag)?.clone()),
                "--raw" => options.raw = Some(crate::value(&mut it, flag)?.clone()),
                "--from-raw" => options.from_raw = Some(crate::value(&mut it, flag)?.clone()),
                "--seed" => options.seed = crate::number(crate::value(&mut it, flag)?, flag)?,
                "--timeout" => options.timeout = timeout(crate::value(&mut it, flag)?)?,
                "--fresh-cwd" => options.fresh = true,
                "--search" => options.search = true,
                s if s.starts_with('-') || options.rounds.is_some() => {
                    return Err(io::Error::other(PROMPTS_USAGE));
                }
                s => options.rounds = Some(crate::number(s, "ROUNDS")?),
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        if options.variants.is_empty()
            || options
                .variants
                .iter()
                .any(|v| !VARIANTS.contains(&v.as_str()) || !seen.insert(v))
        {
            return Err(io::Error::other(
                "--variants needs distinct names from base,dynamic,short,padded",
            ));
        }
        if options.from_raw.is_some() {
            if options.rounds.is_some()
                || options.raw.is_some()
                || options.answers.is_some()
                || options.fresh
            {
                return Err(io::Error::other(PROMPTS_USAGE));
            }
        } else if options.rounds.is_none_or(|n| n == 0 || n == usize::MAX) {
            return Err(io::Error::other(
                "ROUNDS must be at least 1 and less than usize::MAX",
            ));
        }
        if options.raw.is_some() && options.raw == options.answers {
            return Err(io::Error::other(
                "--raw and --answers must be different files",
            ));
        }
        Ok(options)
    }
    fn prompts(&self) -> io::Result<Vec<String>> {
        let prompts: Vec<_> = if let Some(path) = &self.prompts {
            std::fs::read_to_string(path)?
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.replace("\\n", "\n"))
                .collect()
        } else if self.search {
            vec![SEARCH.into()]
        } else {
            PROOFREADING.iter().map(|s| (*s).to_owned()).collect()
        };
        if prompts.is_empty() {
            return Err(io::Error::other("no prompts"));
        }
        Ok(prompts)
    }
}

/// Williams design: each variant occupies each position, and follows every
/// other variant, equally often in a complete block (two blocks for odd n).
pub fn balanced_orders(count: usize) -> Vec<Vec<usize>> {
    let mut first = vec![0];
    let (mut low, mut high) = (1, count - 1);
    while low <= high {
        first.push(low);
        low += 1;
        if low <= high {
            first.push(high);
            high -= 1;
        }
    }
    let mut orders: Vec<_> = (0..count)
        .map(|shift| {
            first
                .iter()
                .map(|index| (index + shift) % count)
                .collect::<Vec<_>>()
        })
        .collect();
    if count % 2 == 1 {
        let reversed: Vec<_> = orders
            .iter()
            .map(|o| o.iter().rev().copied().collect())
            .collect();
        orders.extend(reversed);
    }
    orders
}

struct Orders {
    block: Vec<Vec<usize>>,
    next: usize,
    count: usize,
    state: u64,
}
impl Orders {
    fn new(count: usize, seed: u64) -> Self {
        Self {
            block: vec![(0..count).collect()],
            next: 0,
            count,
            state: seed,
        }
    }
    // SplitMix64 and Fisher-Yates, stable across targets and compiler versions.
    // This intentionally does not reproduce Python's Mersenne Twister sequence.
    fn random(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut n = self.state;
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
        n ^ (n >> 31)
    }
    fn next(&mut self) -> Vec<usize> {
        if self.next == self.block.len() {
            self.block = balanced_orders(self.count);
            for i in (1..self.block.len()).rev() {
                let bound = (i + 1) as u64;
                let threshold = bound.wrapping_neg() % bound;
                let j = loop {
                    let n = self.random();
                    if n >= threshold {
                        break (n % bound) as usize;
                    }
                };
                self.block.swap(i, j);
            }
            self.next = 0;
        }
        let order = self.block[self.next].clone();
        self.next += 1;
        order
    }
}

fn open_output(path: Option<&str>) -> io::Result<Option<std::fs::File>> {
    path.map(|p| {
        // Open without truncation: path spelling, symlinks, and hard links can
        // identify the same file. Check the actual handles before discarding
        // any existing evidence, then write through those same handles.
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(p)
    })
    .transpose()
}

type OutputFiles = (Option<std::fs::File>, Option<std::fs::File>);

fn output_files(raw_path: Option<&str>, answers_path: Option<&str>) -> io::Result<OutputFiles> {
    let raw = open_output(raw_path)?;
    let answers = open_output(answers_path)?;
    if let (Some(raw), Some(answers)) = (&raw, &answers) {
        let raw_identity = same_file::Handle::from_file(raw.try_clone()?)?;
        let answers_identity = same_file::Handle::from_file(answers.try_clone()?)?;
        if raw_identity == answers_identity {
            return Err(io::Error::other(
                "--raw and --answers must be different files",
            ));
        }
    }
    for file in raw.iter().chain(answers.iter()) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.set_len(0)?;
    }
    Ok((raw, answers))
}
fn write_row(file: &mut Option<std::fs::File>, value: &impl serde::Serialize) -> io::Result<()> {
    if let Some(file) = file {
        serde_json::to_writer(&mut *file, value).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.flush()?;
    }
    Ok(())
}

pub fn prompts_command(args: &[String]) -> io::Result<()> {
    if args == ["--help"] {
        println!("{PROMPTS_USAGE}");
        return Ok(());
    }
    let options = Options::parse(args)?;
    let prompts = options.prompts()?;
    let prompt_count = (!options.search).then_some(prompts.len());
    if let Some(path) = &options.from_raw {
        let rows = claude_report::read(path)?;
        print!(
            "{}",
            claude_report::report(&rows, &claude_report::variants(&rows), prompt_count)
        );
        return Ok(());
    }
    let executable = live_executable()?;
    let scratch = Scratch::new()?;
    let (mut raw, mut answers) = output_files(options.raw.as_deref(), options.answers.as_deref())?;
    let mut orders = Orders::new(options.variants.len(), options.seed);
    let mut rows = Vec::new();
    for round in 0..=options.rounds.unwrap() {
        for (position, index) in orders.next().into_iter().enumerate() {
            let variant = &options.variants[index];
            let cwd = if options.fresh {
                let path = private_fs::unique_child(&scratch.0, "cwd");
                private_fs::create_private_dir(&path)?;
                path
            } else {
                scratch.0.clone()
            };
            let (mut row, _, _) = run_once(
                &executable,
                &Turn {
                    variant,
                    prompt: &prompts[round % prompts.len()],
                    cwd: &cwd,
                    scratch: &scratch.0,
                    search: options.search,
                    short: &options.short,
                    timeout: options.timeout,
                    term: false,
                    extra_env: &[],
                },
            )?;
            row.round = round;
            row.position = Some(position + 1);
            write_row(&mut raw, &row)?;
            if round > 0 {
                write_row(
                    &mut answers,
                    &json!({"round": round, "variant": variant, "answer": row.answer}),
                )?;
            }
            let note = if row.complete() {
                String::new()
            } else {
                format!(
                    " (no answer: {})",
                    row.error.as_deref().unwrap_or("no text or usage")
                )
            };
            println!(
                "round {round} {variant:8} first text {:6.0} ms{note}",
                row.first_text_ms.unwrap_or(0.0)
            );
            io::stdout().flush()?;
            rows.push(row);
        }
    }
    println!(
        "\n{}",
        claude_report::report(&rows, &options.variants, prompt_count)
    );
    if rows.iter().any(|r| !r.complete()) {
        return Err(io::Error::other(
            "some runs failed or returned no answer; available rows have been saved and reported",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn williams_design_balances_positions_and_predecessors() {
        for count in 2..=8 {
            let orders = balanced_orders(count);
            let mut positions = vec![vec![0; count]; count];
            let mut follows = vec![vec![0; count]; count];
            for order in &orders {
                for (position, &variant) in order.iter().enumerate() {
                    positions[variant][position] += 1;
                }
                for pair in order.windows(2) {
                    follows[pair[0]][pair[1]] += 1;
                }
            }
            let expected = if count % 2 == 0 { 1 } else { 2 };
            assert!(positions.iter().flatten().all(|&n| n == expected));
            for (a, row) in follows.iter().enumerate() {
                for (b, &n) in row.iter().enumerate() {
                    assert_eq!(n, if a == b { 0 } else { expected });
                }
            }
        }
    }
    #[test]
    fn seeded_orders_are_reproducible_and_keep_whole_blocks() {
        let mut a = Orders::new(4, 0);
        let mut b = Orders::new(4, 0);
        let mut c = Orders::new(4, 2);
        assert_eq!(a.next(), vec![0, 1, 2, 3]);
        b.next();
        c.next();
        let mut first = Vec::new();
        let mut other = Vec::new();
        for _ in 0..12 {
            let order = a.next();
            assert_eq!(order, b.next());
            first.push(order);
            other.push(c.next());
        }
        assert_ne!(first, other);
        for block in first.chunks(4) {
            let mut sorted = block.to_vec();
            sorted.sort();
            let mut expected = balanced_orders(4);
            expected.sort();
            assert_eq!(sorted, expected);
        }
    }
    #[test]
    fn invalid_inputs_fail_before_discovery_or_a_live_launch() {
        for args in [
            vec!["0"],
            vec!["1", "--variants", "base,base"],
            vec!["1", "--timeout", "0"],
            vec!["1", "--raw"],
            vec!["--from-raw", "file", "1"],
            vec!["1", "--raw", "same", "--answers", "same"],
        ] {
            assert!(
                Options::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err()
            );
        }
    }
}
