# Measuring performance

How Seatline's startup and request overhead is measured, what a measurement may and may not be used to claim, the baseline captured before any speedup work, and the budgets that baseline sets. It is the method behind tracker items B-01 to B-03; the [implementation tracker](performance-implementation-tracker.md) says what is done.

Two parts make it up:

- [**Phase telemetry**](telemetry.md): optional timing the broker records for each request, with precisely defined phase boundaries.
- **`seatline-bench`**: a harness that runs named scenarios against the real broker and joins what the application saw with what the broker recorded.

## What is measured

A request's latency depends on what is already warm. The harness measures **states**, each named for what is warm and what is fresh, and never lets one stand in for another:

| State | What it is | Scenarios |
| --- | --- | --- |
| `cold_broker_fresh_provider` | No broker is running. The application's client starts one, connects, and sends one request; the provider is a fresh process. | `cold-broker`, and `cold-three-app` (three applications start together) |
| `warm_broker_fresh_provider` | A broker is running. Each request is a new connection, as the shipped client makes it, and runs a fresh provider process. | `warm-send`, `warm-send-adapter`, `warm-send-shared`, `warm-send-probe`, `warm-status`, `short-isolated-paced`, `three-app-short`, `short-contended` |
| `resumed_context` | As warm, but each request continues the provider-side conversation the last one started, in a fresh process. | `resumed-context` |
| `reused_process` | One provider process serving several requests. | `reused-process`: **unsupported** until a persistent-provider adapter exists (tracker E-02). It is reported as unsupported with that reason, never as a number. |

**Preparation is reported apart from submit-to-text.** `prepare` is everything the application does before it can send: connecting (starting the broker, when cold) and the authentication handshake. `submit→text` runs from sending the request to the first visible answer text. `start→text` is the two together, from the application's own start of the request; it is the one figure that means the same thing over the wire and through the shipped `RemoteProvider`, which cannot time its connect apart. Cold work stays visible: the cold scenario reports `prepare`, it does not hide it.

**Two views of every request.** The *client* view (`client_*` metrics) is what the application saw, from its own monotonic clock. The *broker* view (`broker_*` metrics) is the broker's own [phase telemetry](telemetry.md): `queue_wait`, `sign_in_probe`, `provider_init`, `first_text`, `completion`, `cleanup`, the `tail` after the provider's final result (where the adapter reports one; it overlaps `completion` and is not a phase), and the broker side of the handshake. The harness matches them by request ID (a request through `RemoteProvider` names every request `request`, so those are matched by order, and not at all if the counts disagree). Their difference bounds what the socket and the application's own scheduling add.

**Process and probe counts** come from two independent sources that must agree: the broker's telemetry (`probes`, `launches` per request) and, with the fake provider, the fake CLI's own invocation log. The harness's tests assert that they do.

### Local overhead versus network and model latency

A fake provider answers at once, and timed directly from outside a turn takes about 1.75 ms and a probe 1.6 ms (an upper bound: it includes the timing script's own process spawn), so a fake-provider run contains **only what Seatline adds**: the broker, scheduling, process start and IPC. It has no network or model latency by construction and says nothing about how soon a real answer begins. That is the *local floor*. A live run (below) contains the provider's own start-up, the network and the model, which Seatline cannot see. Only `queue_wait`, `cleanup` and the handshake are purely local in a live run; compare the live run of a scenario with the fake run of the same scenario to estimate what is not Seatline's.

## Rules for claims

These come from the tracker and bind every performance claim made about Seatline:

1. **Compare like with like.** A second request, a warm broker or a reused connection is not a faster first request. Quote a number only for the state it was measured in, and compare only the same scenario, machine, build profile of the harness *and* of the companion (the broker is what is timed), and mode. `seatline-bench compare` warns when platform, either profile or mode differ, and a report names both profiles.
2. **No claim from reuse alone.** A request that is faster because something was already warm has not made the cold path faster. Show the cold path.
3. **Fake runs cannot support latency claims about real answers.** They support claims about Seatline's own overhead, and they are labelled `fake`.
4. **Know the noise.** The broker's hub polls every 5 ms when idle, so many metrics move in steps of about 5 ms. Two identical runs of the baseline below agree to within 0.2 ms at p50 and 0.6 ms at p95 in every single-application scenario. The multi-application scenarios (`three-app-short`, `short-contended`) depend on how independent processes line up and differ by up to about 5.5 ms at p50 and 6.3 ms at p95. A p95 over 30 samples is the 29th value, so two requests landing on a slow tick move it by a whole step: an earlier run on the same machine did that to a single-application p95. A change smaller than this noise is not evidence; repeat the run to know what it is on your machine.
5. **p95 needs at least 20 samples.** With fewer the 95th percentile is the maximum (nearest rank), and the report says so.
6. **Publish before and after.** Run the same scenarios on the same machine before and after a change, keep both JSON files and quote `compare`'s output.

## Running it

```sh
cargo build --release --locked -p seatline-companion -p seatline-bench
target/release/seatline-bench run --samples 30 --warmup 3 \
    --label "my change" --output after.json --markdown after.md
target/release/seatline-bench compare before.json after.json
target/release/seatline-bench overhead      # scheduler cost of phase timing
```

Run from the repository root so the report records the revision and whether the tree has uncommitted changes. `seatline-companion` and the fake provider are looked for next to the harness binary; `--companion` and `--fake-provider` override that. `--scenario NAME` (repeatable) picks scenarios. A scenario that cannot run does not discard the others: it is kept in the report as `failed` with its reason, the run goes on, and the command exits non-zero once the report is written. A report is JSON with every measured request in it, so any summary can be recomputed; `seatline-bench report FILE.json` renders the tables.

Reports hold no path, user name, host name or credential: only the OS and architecture, the CPU count, the compiler and companion versions, the revision, the broker's limits and the measurements. A test asserts that the scratch and source paths and the word "token" do not appear.

### How a run works

- Each simulated application is **a process of its own** (the harness runs itself as `seatline-bench app`), so a cold start goes through the shipped client's own `connect`, which starts the broker when none is running, and so that several applications compete as separate programs do.
- **`cold-broker`** gives every sample a new data directory and broker, started by the application's client; the broker leaves by itself a second after it is idle, and the harness waits for it before the next sample.
- **Warm scenarios** start one broker and keep it up (it would leave after 120 s idle, so one orphaned by a killed harness does not linger). **Warm-up requests** are made first, not counted, and still appear in process counts (which say so).
- **Three-application scenarios** start all applications together after each says it is ready. In `short-contended`, two applications issue back-to-back long requests (the fake provider's `slow` behavior, about 300 ms; the second starts half a request later so the two do not finish in lockstep) until the short application is done.
- Everything runs under a scratch directory in the user's cache directory, not `/tmp`: a provider refuses to run in a directory another user could change, and `/tmp` is one. `--scratch` changes it (the path must stay short enough for a Unix socket).
- The harness changes no environment of its own: each process it starts gets the data directory, telemetry file, provider search path and home it needs.

### Live providers

```sh
SEATLINE_BENCH_LIVE=1 target/release/seatline-bench run --live codex \
    --samples 10 --warmup 1 --label "codex live" --output codex-live.json
```

`--live PROVIDER` (`codex`, `claude`, `gemini` or `grok`) measures the real CLI installed on the machine instead of the fake. It **sends real prompts** ("Reply with the single word: ok", tools off) and uses a little of the account's quota, so it needs the explicit `SEATLINE_BENCH_LIVE=1` as well, and defaults to 5 samples with 1 warm-up. It supports the scenarios that do not need requests of a known length: `cold-broker`, `warm-send`, `warm-send-adapter`, `warm-send-shared`, `warm-send-probe`, `warm-status`, `resumed-context` and the unsupported `reused-process`. A scenario a provider's adapter cannot run is reported as **unsupported** with its reason, never dropped, never aborting the run and never measured as something else: `resumed-context` for Gemini and Grok (their one-shot modes keep no session, so the broker refuses a persistent turn) and `warm-send-probe` for Gemini and Grok (they run no sign-in probe on the send path, so it would be an ordinary send under the wrong name). It uses your real home and provider configuration (so that you are signed in) with a scratch broker data directory, and records the provider's `--version`. **No live run was made in this slice**: nothing here claims a live latency.

A live run also shows where the *answer* time went, which a fake run cannot: each sample carries the broker's `tail` (from the provider's final result to the terminal update, the wait through its process leaving) and the token counts the provider reported, including how many of the input came from its prompt cache and how many of the output were reasoning (see [phase telemetry](telemetry.md#beside-the-phases-the-tail-and-the-usage)). Read them before changing anything about how a provider is launched or ended.

## Baseline

Measured at revision `071ab6d662ca` with a clean tree, a release build of the broker and the harness, a fake provider, 30 measured requests per application after 3 warm-up, on a 4-CPU Linux container (rustc 1.97). Raw evidence, with every request: [run 1](performance-baselines/2026-10-03-fake-release-run1.json), [run 2](performance-baselines/2026-10-03-fake-release-run2.json); tables: [run 1 summary](performance-baselines/2026-10-03-fake-release-summary.md); how far two identical runs differ: [run 1 against run 2](performance-baselines/2026-10-03-fake-release-repeat-comparison.md). The [scheduler overhead](performance-baselines/2026-10-03-scheduler-overhead.json) is a separate measurement.

This predates the lifecycle slice (A-01 to A-04): it is the baseline those changes, and everything after, are measured against. All values are milliseconds, p50 / p95, as the range over the two runs.

| Scenario | Metric | p50 | p95 |
| --- | --- | --- | --- |
| `cold-broker` | `prepare` (connect + handshake) | 102.2–102.3 | 102.4–102.6 |
| | of which `connect` | 101.7–101.8 | 101.9 |
| | of which handshake | 0.5 | 0.7 |
| | `start→text` | 108.4–108.6 | 108.7–109.0 |
| `warm-send` | `prepare` | 0.3 | 0.4 |
| | `submit→text` | 5.9 | 6.1 |
| | `start→text` | 6.2–6.3 | 6.5 |
| | `submit→done` | 11.0–11.1 | 11.2–11.3 |
| | broker `provider_init` | 5.5 | 5.6 |
| | broker `completion` | 5.2 | 5.2 |
| `warm-send-adapter` | `start→text` (through `RemoteProvider`) | 6.3–6.4 | 6.6–6.8 |
| `warm-send-probe` | `start→text` | 16.8–16.9 | 17.1–17.7 |
| | broker `sign_in_probe` | 10.6 | 10.7 |
| `warm-status` | `submit→done` | 10.9–11.0 | 11.2–11.4 |
| | broker `sign_in_probe` | 10.6–10.7 | 10.8–10.9 |
| `resumed-context` | `start→text` | 6.2–6.3 | 6.4–6.6 |
| `short-isolated-paced` | `start→text` | 6.4–6.5 | 6.8 |
| | broker `queue_wait` | 0.0 | 0.0 |
| `three-app-short` (each app) | `start→text` | 7.3–12.8 | 17.4–23.7 |
| | broker `queue_wait` | 0.0–5.3 | 10.6–10.9 |
| `short-contended` (the short app) | `start→text` | 106.1–111.1 | 125.4–126.5 |
| | broker `queue_wait` | 99.0–104.2 | 118.7–119.5 |
| `short-contended` (a long app) | `submit→done` | 320.1–320.4 | 321.5–322.8 |
| | broker `queue_wait` | 10.4 | 10.6 |

Every request in every scenario completed. Process counts agree between the broker and the fake CLI: one provider process per request, plus one probe process per request in `warm-send-probe` and per status check in `warm-status`.

### What it shows

- **A cold start costs a fixed 100 ms.** Of the 108 ms from the application's start to first text, `connect` is 101.7 ms: the client sleeps 100 ms before it first looks for the broker it just started, and the whole connect is that sleep plus 1.7 ms. The broker's true start time is invisible below 100 ms until that wait is removed (tracker A-04).
- **A warm connection and handshake cost 0.3 ms** (0.2 ms of it on the broker). The shipped adapter's thread, runtime, connection and handshake per exchange add about 0.1 ms to `start→text` over the bare wire. Reusing a connection can save at most that locally, so it is not a latency win to claim (D-01, G-02).
- **Most of the ~6 ms to first text is polling, not the provider.** The fake provider starts and answers in at most about 1.75 ms when timed directly. The broker's hub polls every 5 ms when idle, so what an application waits for is mostly the next tick: `completion` (first text to terminal) is 5.2 ms, one tick, because the fake exits within about a millisecond of answering; a sign-in probe whose process takes 1.6 ms is 10.6 ms. Slice F-04 targets this.
- **The sign-in probe costs one process and about 10.6 ms per request**, on every request that asks for it: 16.8–16.9 ms to first text against 6.2–6.3 ms without (C-02 targets the repeat probes).
- **Resuming a conversation costs nothing locally.** The fake has no context to load, so `resumed-context` equals `warm-send`. A live run is needed to price it.
- **Three applications sharing a provider wait for slots.** With three applications each sending back-to-back, the broker admits two per provider: p95 `queue_wait` is about 10.8 ms (two ticks) and p95 `start→text` 17–24 ms against 6.5 ms alone.
- **A short request behind long ones waits a long time.** While two other applications keep both provider slots busy with 300 ms requests, the short application's first text arrives at p50 106–111 ms against 6.4–6.5 ms alone, about 17 times later, and 99–104 ms of it is `queue_wait`. The long requests are unaffected (their `queue_wait` p50 is 10.4 ms: the short request is admitted first). F-03 targets this.

## Slice D: the shared client and coordinated startup

Measured at `a8dd5af` (clean tree, release builds, fake provider, the same machine as the baseline); the evidence is checked in beside the baseline: [D-02 comparison](performance-baselines/2026-10-03-d02-cold-start-comparison.md) and its four reports, and [D-01 summary](performance-baselines/2026-10-03-d01-shared-client-summary.md) with three reports. Both are against the state after A-01 to A-04, not the baseline above, which predates them.

**D-02: cold start.** Two pairs of runs, p50 / p95 in milliseconds:

| Scenario | Metric | Before (A-04 client) | After (start claim, 1 ms polling) |
| --- | --- | --- | --- |
| `cold-broker` | `prepare` | 12.0 / 12.4 (12.0 / 12.2) | 3.1 / 3.3 (3.1 / 3.3) |
| `cold-broker` | `start→text` | 18.1 / 18.8 (18.2 / 18.5) | 9.1 / 9.7 (9.2 / 10.4) |
| `cold-three-app` (each app) | `prepare` | 11.8–11.9 / 13.4–14.6 | 5.7–6.0 / 6.1–7.0 |
| `cold-three-app` | companion starts per cold start | 3.0 (99 for 33) | 1.0 (33 for 33) |

- **A-04 removed the 100 ms sleep but left the polling: 10 of the 12 ms is a backoff step.** A companion listens about 2 ms after it is started (timed by polling its socket from outside every 0.2 ms, which includes the timing script's own process spawn, so an upper bound: p50 1.9 ms), yet the client looked for it after 10, 30, 70 ms, so a 2 ms start was found at 10 ms. Looking every millisecond first finds it at the next step: `prepare` falls from 12.0 to 3.1 ms, which is the spawn, the 2 ms start, at most one step and the 0.5 ms handshake.
- **An explicit readiness signal was considered and not built.** The tracker asks for one if it improves measured startup over A-04. A signal (a pipe or file the companion writes to once it listens) could save at most the one polling step that remains, under a millisecond, and needs a way for waiting clients to receive it as well; polling at a millisecond reaches the floor within that step without a new protocol, a blocked reader that cannot be cancelled, or a file to clean up. If a later measurement shows the step matters, the claim already is the place to hang one.
- **Three applications that start together start one companion.** Before, each of the three started its own (99 starts for 33 cold starts), and since the broker's own lock lets only one serve, two of the three processes had nothing to do. Now the client that holds the claim starts it and the others look for the broker without starting one (33 starts for 33). Their `prepare` falls by about 6 ms; it is higher than a single client's 3.1 because three programs and a companion contend for four CPUs, and the harness's stand-in for the companion adds one process hop to every start, before and after. The provider slots are the same two per app and provider, so the p95 `queue_wait` of about 11 ms (two polling ticks) is unchanged.

**D-01: the shared client.** The path through one `RemoteClient`, against a connection per exchange and the bare wire:

| `start→text` p50 / p95 | Bare wire (`warm-send`) | Per-exchange adapter | Shared client |
| --- | --- | --- | --- |
| run 1 (30 requests) | 6.3 / 6.7 | 6.5 / 7.0 | 6.1 / 57.7 |
| run 2 (30) | 6.2 / 6.5 | 6.3 / 9.8 | 6.1 / 6.2 |
| confirmation (100) | 6.2 / 6.5 | 6.4 / 7.0 | 6.2 / 6.6 |

The shared client meets its budget (p50 within +0.2 ms of the bare wire) and is 0.2 to 0.3 ms below the per-exchange adapter, which is what connection reuse was worth in the baseline and **not the reason for D-01**. Run 1's p95 is two requests that stalled for reasons outside the client (see the summary); three further runs of 100 requests each had a worst case of 11.6 ms.

## Budgets

Improvement budgets set from that baseline. A budget is what a slice must demonstrate, with before and after reports from the same machine; it can be revised with a stated reason, not quietly. p50 budgets are checked at p50 (noise 0.1 ms; about 5 ms for `short-contended`); p95 budgets in whole polling steps.

| Slice | Scenario and metric | Baseline p50 (p95) | Budget | Basis |
| --- | --- | --- | --- | --- |
| A-04 | `cold-broker` `prepare` | 102.3 (102.6) | p50 ≤ 35, p95 ≤ 80 | 100 of the 102 ms is the fixed wait; the allowance covers a broker that needs one retry step. |
| D-02 | `cold-three-app` companion starts per cold start | 3.0 (99 starts for 33 cold starts) | exactly 1 (bounded at 2 per client), every application served | One start claim: only the client that holds it starts the companion. **Met**: 1.0 (33 for 33). |
| D-02 | `cold-broker` `prepare` | 12.0 (12.4) after A-04 | no regression from the A-04 budget (p50 ≤ 35, p95 ≤ 80); the explicit-readiness comparison below | **Met**: 3.1 (3.3). |
| F-04 | `warm-send` broker `completion` | 5.2 (5.2) | p50 ≤ 2 | One polling tick is the whole baseline; the fake exits within about a millisecond. |
| F-04 | `warm-send` `submit→text` | 5.9 (6.1) | p50 ≤ 3.5 | The provider process is at most about 1.75 ms and the IPC 0.3 ms; at least 2.4 ms of the polling share goes. |
| F-04 | `warm-status` broker `sign_in_probe` | 10.7 (10.9) | p50 ≤ 4 | A 1.6 ms process plus one hop. |
| F-04 | idle CPU and wakeups | **not measured** | F-04 adds its own measurement | The harness measures latency, not idle cost; F-04's acceptance already asks for it. |
| F-03 | `short-contended`, short app broker `queue_wait` | 99–104 (119) | p95 ≤ 35, with every long request still completing and the long apps' `submit→done` p50 ≤ 360 | Bounded interactive wait without starving long work. |
| F-03 | `three-app-short` broker `queue_wait` | 0.0–5.3 (10.9) | no regression: p95 ≤ 12 | Fairness must not slow the case that already works. |
| C-02 | `warm-send-probe` probes per request | 1.0 (30 of 30; the fake saw 33 probes for 33 requests) | repeats inside the freshness window: 0; concurrent requests: at most 1; a fresh request still probes | Counted, not timed. |
| C-02 | `warm-send-probe` `start→text` | 16.8–16.9 | cached repeats p50 within +1 of `warm-send` (6.2–6.3) | The probe is the whole difference. |
| D-01, G-02 | `warm-send-shared` against `warm-send`, `start→text` | `warm-send-adapter` was 6.3–6.4 against 6.2–6.3 (+0.1); **met by D-01**: `warm-send-shared` 6.1–6.3 against 6.1–6.3 (see [Slice D](#slice-d-the-shared-client-and-coordinated-startup)) | the reusable client p50 within +0.2 of the bare wire | Connection reuse is worth at most about 0.3 ms locally: justify D-01 by bounded routing, one runtime and cancellation, not latency. |
| B-01 | scheduler cost per update, phase timing off | 105–133 ns; `main`'s scheduler 116–139 | indistinguishable from `main` | See [telemetry cost](telemetry.md#cost). |
| B-01 | scheduler cost per update, phase timing on | +11 to +15 ns | ≤ +30 ns | |
| E-02 to E-04 | persistent provider processes | no local baseline | **no budget set** | The fake provider has no start-up time to save, so a local baseline cannot price a reused process. Set it from a `--live` baseline and report `reused-process` as its own state. |

Slices A-01 to A-03 have no latency budget: their gates are lifecycle bounds.

F-01 through F-04 add [hub-only before/after evidence](performance-baselines/2026-10-04-f-hub-summary.md) on 2026-10-04, including idle CPU/wakeup counters and adaptive 1–5 ms active polling. It measures a different entry point and contention workload, so it does not replace the socket-based percentile budgets above. In particular, an interactive generation cannot interrupt the two provider slots already occupied by long generations: priority changes the next admission opportunity, not their remaining execution time. Check the original IPC scenarios separately before marking their budgets met.

## What this does not cover

- **No live-provider baseline.** Network and model latency, a real provider's start-up and the real cost of resuming a context are unmeasured. The live mode exists and is untested against a real provider here; the budgets that depend on it (E-02 to E-04) are left open.
- **One machine.** A virtualized 4-CPU Linux container. macOS and Windows (named pipes, different process start costs) are measured by the CI matrix only for correctness, not speed.
- **Long requests are fake and fixed.** The 300 ms `slow` behavior is the only long request; contention results scale with it.
- **Idle cost** (CPU, wakeups) and **memory** were not measured in the original B baseline. The F follow-up above measures idle cost; memory remains unmeasured.
- **A genuinely cold machine** (empty page cache, first start after installation) is not reproduced: `cold-broker` is a cold *broker* on a warm machine.
