# Seatline performance implementation tracker

## Goal and scope

Reduce first-visible-text latency and recurring local overhead while keeping one shared companion safe for Tabbeam, Conclave, and Lineleaf. A warm broker, a resumed conversation, and a reused provider process are different states; report them separately.

Seatline owns provider execution, transport, readiness, scheduling, and cleanup mechanics. Applications retain prompts, conversation policy, orchestration, editor behavior, and product-specific preparation triggers. Keep the runtime Rust-based; do not add a JavaScript runtime or move Conclave features into Seatline.

Source review baseline: [`dc1086582c8b98498aa48dae91c8d174bc3cfc3c`](https://github.com/davletovb/seatline/tree/dc1086582c8b98498aa48dae91c8d174bc3cfc3c), rechecked against main on 2026-10-03. These are source-backed findings, not measured live-provider speedups.

## Workflow

- **READY**: dependencies satisfied; implementation may start.
- **IN PROGRESS**: actively being implemented.
- **BLOCKED**: finish the listed dependencies or protocol investigation first.
- **IMPLEMENTED — VERIFY**: code and local checks complete; PR/platform/integration validation remains.
- **DONE**: merged and the stated acceptance evidence is recorded.
- **EXTERNAL**: tracked here, but implementation belongs in an application repository.

Update this file in every implementation PR. Record commit/PR, relevant checks, measured evidence, and limitations. Verify each finding against the current code before changing it. Complete coherent slices; do not mark plans, prototypes, skipped live tests, or app work as finished library behavior.

## Implementation order

1. A: fix lifecycle bounds and remove the fixed startup delay.
2. B: establish reproducible phase measurements before claiming speedups.
3. C and D: define readiness freshness and reuse transport safely.
4. E: prototype persistent Codex, then evaluate Claude behind explicit capability gates.
5. F: move blocking work off the hub, improve fairness, and reduce idle overhead.
6. G: validate and adopt the changes in all three applications.

D-01 and parts of B can proceed independently of C. Persistent-provider production rollout depends on B, C, and isolation/recovery evidence, rather than prototype existence alone.

## A — Lifecycle and immediate startup fixes

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| A-01 | READY | Enforce Codex's post-answer finish deadline before reading additional output, including continuous stdout/stderr. Preserve a completed answer while terminating and reaping the lingering child. | None | Fake CLI completes, then floods output; exactly one completion arrives within the finish/cleanup bound and the child is reaped. Cover failed outcomes and ordinary exit. |
| A-02 | READY | Track whether the hub has already emitted a terminal event. On session-cap/storage failure, suppress later deltas, session handles, and terminal events while retaining the occupied slot until supervisor cleanup ends. | None | Forced ledger write failure and full ledger each emit one failure; unrelated app still completes; active slots are eventually released. |
| A-03 | READY | Replace blocking event delivery in the remote client's single-thread Tokio runtime with bounded asynchronous delivery. Keep cancellation serviceable when the consumer does not drain events. | None | Fill the event buffer without draining it, cancel, and observe the cancel frame at the broker within a bounded deadline; receiver drop also closes work without a stuck worker. |
| A-04 | READY | Retry connecting immediately after broker spawn, then use short bounded backoff under the existing overall startup budget. | None | Deterministic readiness/retry tests show no unconditional 100 ms wait; real IPC startup and timeout/error behavior still work. |

Starting slice: A-01 through A-04. Authentication caching and persistent-provider changes wait for their contracts and measurement coverage.

## B — Measurement and baseline

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| B-01 | IMPLEMENTED — VERIFY | Define optional, privacy-safe phase telemetry: connection/auth handshake, queue wait, sign-in probe, provider initialization, first visible text, completion, and cleanup. Use monotonic durations and request correlation; do not log tokens, prompts, account names, or native handles. | None | Document precise phase boundaries, unsupported/missing phases, cancellation outcomes, and disabled-path overhead; test event ordering without relying on wall-clock thresholds. |
| B-02 | IMPLEMENTED — VERIFY | Add a reproducible benchmark harness with deterministic fake providers and an opt-in live-provider mode. Capture samples, p50/p95, process/probe counts, versions, platform, and effective configuration. | B-01 | Results distinguish cold broker + fresh provider, warm broker + fresh provider, resumed context, and reused process. Preparation is reported separately from submit-to-text latency. |
| B-03 | IMPLEMENTED — VERIFY | Capture single-app and simultaneous three-app baselines. Separate local overhead from network/model latency and compare isolated short requests with competing long requests. | B-02 | Checked-in methodology and sanitized evidence; no claims based solely on a second request or connection reuse. Set quantitative improvement budgets from measured baseline. |

Slice B implemented together (B-01 to B-03): optional [phase telemetry](telemetry.md) with defined boundaries (core `telemetry` module, scheduler stamping, Codex and Claude probe spans, hub records, the broker's `SEATLINE_TELEMETRY_FILE`), the `seatline-bench` harness, and the [measurement method, baseline and budgets](performance-measurement.md) with sanitized evidence in [`performance-baselines/`](performance-baselines/). Baselines are of **local overhead with fake providers only**: the network and model half of B-03 needs `seatline-bench run --live`, which exists and has not been run against a real provider, so B-03 is not complete until a live baseline is recorded. Persistent-provider budgets (E-02 to E-04) are deliberately unset for the same reason.

## C — Authentication/readiness contract and preparation

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| C-01 | READY | Define fresh vs cached readiness and the meaning of `check_sign_in` across Codex, Claude, Gemini, and Grok. Specify classification and whether send-time status is emitted; preserve callers that request a fresh probe. | None | Contract and provider conformance tests cover authenticated, unauthenticated, unknown, unavailable, and probe timeout; Codex classification examines sanitized probe output where needed. |
| C-02 | BLOCKED | Cache verified readiness with explicit freshness bounds and single-flight probes keyed by effective provider/account/workspace/environment configuration. | C-01, B-01 | Repeated and concurrent requests avoid duplicate probes; fresh requests bypass the cache; account/config changes, executable changes, revocation, and authentication failures invalidate it; no cross-app credential leakage. |
| C-03 | BLOCKED | Add a generic `prepare(provider)` API for executable resolution, connection/readiness, and supported provider initialization. Preparation must not send synthetic model prompts or consume generation quota. | C-01, C-02, D-01 | Idempotent bounded preparation, concurrent deduplication, cancellation, idle expiry, and unsupported-adapter behavior tested; apps choose when to trigger it. |
| C-04 | BLOCKED | Cache executable discovery and index native-session lookup with explicit invalidation rather than repeated scans. | B-01, C-01 | Benchmarks demonstrate benefit; missing/replaced executables and changed configuration recover; ledger caps, rollback, and app scope remain correct. |

## D — Reusable client and bounded delivery

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| D-01 | BLOCKED | Add an app-scoped reusable Rust remote client with one runtime, persistent authenticated IPC, unique request IDs, and bounded request/event routing. Retain compatibility adapters. | A-03, B-01 | Concurrent requests route correctly; cancel targets one request; slow consumers, disconnect, reconnect, grant changes, and shutdown do not affect another app. No transparent replay of a generation after ambiguous disconnect. |
| D-02 | BLOCKED | Coordinate simultaneous broker startup so callers do not all spawn a companion; use an explicit readiness signal if it improves measured startup over A-04. | A-04, B-02 | Multi-client cold-start test produces one serving broker and bounded spawn attempts; stale locks, failed startup, upgrade, and timeout recover. |
| D-03 | BLOCKED | Bound in-process service text delivery without blocking the scheduler/cancellation path or silently dropping answer text. Define slow-consumer policy. | A-03 | Sustained output under a paused consumer stays within the documented memory bound; cancellation and shutdown remain responsive; terminal events are delivered consistently. |

## E — Persistent provider processes

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| E-01 | READY | Investigate the current official Codex app-server protocol, CLI version compatibility, delta events, account operations, reset/resume semantics, and process failure behavior. | None | Record protocol sources and supported versions; fake protocol fixtures cover initialization, turns, cancellation, malformed output, and crash. |
| E-02 | BLOCKED | Prototype a Rust stdio Codex app-server adapter with incremental text and reused initialization, behind an explicit capability/version gate and the existing exec fallback. | E-01, A-01, A-02, B-01, C-01 | Multiple turns use one child; cold/fresh/reused results are measured; cancellation/crash recovery and unsupported-version fallback tested; do not replay a possibly executed turn. |
| E-03 | BLOCKED | Define bounded process ownership/pooling and idle shutdown. Isolate apps, accounts, workspaces, tool policies, and independent conversations such as Lineleaf fields. | E-02, C-02, B-02 | Cross-app/context isolation tests, max process/queue limits, revocation, cleanup and reaping tests, restart/upgrade behavior; quota and resource effects documented. |
| E-04 | BLOCKED | Evaluate Claude's documented streaming-input mode for a long-lived adapter after validating context reset and supported versions. Investigate other providers only where a documented mechanism exists. | E-03 | Official protocol evidence, deterministic lifecycle/isolation tests, and comparative measurements; retain single-run fallback where safe reuse is unsupported. |

## F — Shared hub, filesystem work, and scheduling

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| F-01 | BLOCKED | Move session-ledger writes to a bounded worker and sequence mutations/rollback. A session handle is acknowledged only after durable persistence; avoid rewriting the whole ledger unnecessarily. | A-02, B-01 | Inject slow writes and failures while another app runs; durability, restart recovery, revocation, caps, and cleanup ordering hold. |
| F-02 | BLOCKED | Move Gemini transcript scanning/deletion out of exchange completion into bounded cleanup work, preserving deletion guarantees and terminal semantics. | A-02, B-01 | Large transcript trees and injected I/O failures do not stall unrelated exchanges; cleanup completion/failure is explicit and no transcripts escape the app scope. |
| F-03 | BLOCKED | Add configurable fair scheduling, bounded interactive priorities, queue deadlines, and explicit queued/admitted events. Distinguish status/readiness work from long generation and cleanup. | B-01, B-03, C-01 | Three-app contention tests bound short-request wait without starving long work; enforce global/app/provider limits; no hard-coded app names. |
| F-04 | BLOCKED | Replace idle 5 ms hub polling with event-driven wakeups or adaptive waiting where practical. Preserve timely cancellation, cleanup, revocation checks, and queue admission. | B-03, F-01 | Measured idle CPU/wakeup improvement and active latency comparison; shutdown and timers retain their bounds. |

## G — Application adoption and release validation

These changes are owned by the applications and are not silently bundled into Seatline.

| ID | Status | Work | Dependencies | Acceptance evidence |
| --- | --- | --- | --- | --- |
| G-01 | EXTERNAL | Lineleaf: retain the native connection, reuse verified readiness, and prepare only likely providers on enabled editor/popup interactions. Preserve independent context per field. | C-02, C-03, D-01 | Extend its existing provider benchmark; count status/generation subprocesses and compare first/subsequent checks; disconnection and field isolation tests. |
| G-02 | EXTERNAL | Tabbeam: adopt the reusable Rust client rather than one thread/runtime/connection per exchange. | D-01 | Concurrent turn/cancel integration tests and measured first-text latency; upgrade the exact Seatline revision deliberately. |
| G-03 | EXTERNAL | Conclave: use the agreed readiness contract instead of unconditional duplicate fresh probes; expose optional preparation from app-owned UI triggers. | C-01, C-02, C-03 | Status UX and authentication-failure recovery tested; account/config changes invalidate readiness; app orchestration remains in Conclave. |
| G-04 | BLOCKED | Validate Linux, macOS, Windows, Rust 1.85, and the client-only companion build; run app integration tests before updating pins or enabling persistent mode by default. | Applicable completed slices; G-01 through G-03 for broad rollout | Existing CI gates, opt-in live-provider smoke tests, three-app evidence, migration notes and fallback instructions. Explicitly distinguish CI/fake-provider success from live latency validation. |

## Verification and release gates

- Required for each Rust slice: formatting, workspace Clippy, workspace tests with the lockfile, affected regression tests, and the client-only companion build.
- Existing CI remains responsible for OS matrix, minimum Rust 1.85, release tests, runtime independence, and fuzz smoke. Real-provider tests remain opt-in.
- Lifecycle gates: one terminal event per accepted request, bounded cancellation/finish, eventual child reaping, no leaked occupied slots, no cross-app context or grant leakage.
- Performance gates: publish reproducible before/after evidence; preparation time and total cold work remain visible. A genuinely cold machine still has startup work, and network/model latency is outside Seatline's direct control.
- No automatic application pin updates or default persistent-mode rollout until the corresponding integration evidence exists.

## Evidence log

| Date | Slice | Evidence | Remaining validation |
| --- | --- | --- | --- |
| 2026-10-03 | Planning | Main matches the reviewed `dc108658` baseline; no existing open Seatline PR conflicts. Tracker created before implementation. | All implementation and benchmark acceptance criteria remain open. |
| 2026-10-03 | B-01 through B-03 | Implementation [d8d3fa8](https://github.com/davletovb/seatline/commit/d8d3fa8), [51265f7](https://github.com/davletovb/seatline/commit/51265f7), [54c9578](https://github.com/davletovb/seatline/commit/54c9578), [071ab6d](https://github.com/davletovb/seatline/commit/071ab6d). New tests: core telemetry (17), scheduler timelines (8), adapter probe spans against the fake CLIs (7), hub records (8), sink and mapping (7), real-broker telemetry (2), harness end to end (8) and unit (11). Format, workspace Clippy with `-D warnings`, client-only Clippy and tests (28), the client-only dependency boundary, the runtime-independence check, and all workspace tests pass on stable Rust 1.97; the workspace tests also passed on Rust 1.85. Baseline captured at `071ab6d` (clean tree, release builds, fake provider, 30 measured requests after 3 warm-up, 4-CPU Linux container), two runs: a cold broker's `prepare` is 102.3 ms, 101.7 ms of it the client's fixed 100 ms wait (A-04); a warm connection plus handshake is 0.3 ms; warm `start→text` is 6.2–6.3 ms of which most is the hub's 5 ms polling (F-04); a sign-in probe adds one process and 10.6 ms to each request that asks for one (C-02); a short request behind two long ones reaches first text at p50 106–111 ms against 6.4–6.5 ms alone (F-03); the shipped adapter costs about 0.1 ms over the bare wire (D-01). Scheduler cost with phase timing off is indistinguishable from `main`'s; on it is +11 to +15 ns per update. | No live-provider run, so no network or model latency is measured and B-03's live half is open. CI matrix (macOS, Windows), the release-profile and fuzz jobs, and the `--no-default-features` job on a clean runner. Idle CPU and memory are not measured (F-04 must add idle CPU). Budgets in the measurement document are proposals until the slices they gate are reviewed. This slice's tracker, README and companion README edits overlap those of A-01 to A-04's branch and need a mechanical merge. |

## Next work

Finish CI and review verification for slice B, and record a live baseline (`SEATLINE_BENCH_LIVE=1 seatline-bench run --live codex`, and the other providers) before setting persistent-provider budgets. Re-run the baseline after A-01 to A-04 merge: A-04 should bring `cold-broker` `prepare` under its budget. Next, implement C-01, then choose caching and persistent-provider defaults against the measured budgets.
