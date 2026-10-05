# Shared hub performance (F-01 through F-04)

This slice runs filesystem work away from the shared hub, schedules applications fairly, and stops polling an idle hub every 5 ms. It keeps one-shot provider execution. **Slice E is deferred as of 2026-10-04, pending the decision about Codex app-server.** No persistent-provider adapter or application pin change is part of F.

## Session durability

One ledger worker owns an ordered, bounded stream of insert/remove mutations. The hub permits at most 32 outstanding ledger operations. The worker batches already queued mutations into one file-sync and replacement, rolls a failed batch back in reverse order, and skips writes for batches with no changes. The `sessions.json` representation remains compatible with existing installations; its native-session and app-count indexes are rebuilt on restart.

A writer panic follows the same failure acknowledgement and rollback path as an I/O error, so subsequent mutations can still be persisted.

A new native session reserves its place in the hub's session cap immediately. Its exchange pauses at that boundary until persistence succeeds. A shared native session joins the existing pending write. The hub sends the opaque token before allowing `Started` and answer updates through, and never acknowledges an undurable token. The process's bounded output buffers provide backpressure during slow storage; the hub does not accumulate answer text. Durable token reuse requires no write. Continuations stay serialized within their app and provider.

A failed insert removes the reservation, sends one `SESSION_STORE_FAILED`, and cancels the exchange. Its occupied generation slot remains until the supervisor finishes cleanup; its request ID is reusable after that failure. Cancellation bypasses the persistence gate and remains serviceable even if the writer is stalled. A mutation already submitted still finishes: a cancelled request may leave an unannounced durable token, which stays scoped to its original app and participates in the existing caps and revocation pruning.

Forget/cleanup waits for provider deletion and then durable token removal before `Completed` and its in-memory completion callback. A failed removal keeps tokens and retry state. Pending removal also continues to exclude conflicting generation/cleanup on the same app/provider. Two missing-grant sweeps still precede pruning; pending inserts are included once their commit resolves. No-op cleanup does not rewrite the ledger.

Tokens pending revocation removal cannot be resumed or acknowledged again, including if the app is reauthorized before the removal finishes. A new native session in that interval receives a separate token and ordered insert; a failed removal retains the original durable record for a later retry.

Storage uses the existing private-file, file-sync and replacement contract. Filesystem operations already accepted may continue after their client disconnects. Hub shutdown drains exchanges and filesystem acknowledgements within its existing four-second shutdown budget; it does not wait indefinitely on stalled storage. This does not add a power-loss journal or change Windows's existing replacement behavior.

## Gemini transcript cleanup

A shared Rust filesystem pool runs at most two jobs and reserves at most 32 jobs, including future work. Gemini reserves cleanup **before launching** its child. Exhausted capacity returns `CLEANUP_BACKLOG_FULL` before any generation. Explicit companion cleanup uses the same pool.

A turn with durable cleanup records temporarily reserves a second slot to write and fsync its workspace marker on the pool. Admission returns while that job is pending; polling waits for its result before spawning the child. Cancellation or dropping the exchange before launch waits for the marker job and removes its files without scanning provider transcripts. A failed spawn uses that same deletion boundary. If the child starts but initial input fails, it is reaped and transcript cleanup runs before reporting the failure. A transient pool initialization failure is retried on the next request.

After the child exits or is reaped, scanning, deletion, cleanup-record removal and workspace deletion run in that pool. The exchange's terminal update waits for their result. Success preserves its original `Completed`, `Failed` or `Stopped`; a cleanup error yields one retryable `CLEANUP_FAILED`. Cancellation during cleanup still waits for the deletion boundary. A supervisor that abandons an exchange still submits its reserved cleanup through `Drop`.

The supervisor preserves `CLEANUP_FAILED` even after cancellation or timeout, so failed deletion remains visible. Its existing forced-stop budget still applies: if filesystem work outlives that budget, the supervisor releases the exchange while the reserved cleanup continues and durable markers retain restart retry evidence. A forced timeout or stop is therefore not a deletion acknowledgement.

Private, file-synced workspace markers permit retry after restart, including cancellation before the adapter read an init event. An explicit group cleanup validates marker paths against that application's workspace base before acting. Known transcript IDs, transcript directories, conversation databases and SQLite sidecars retain the existing deletion rules. Other applications' transcripts remain untouched.

Group cleanup prepares its base once, compares canonical forms of both existing parents, and validates the stored parent too. Transcript matching retains the original workspace string. Malformed or out-of-scope markers move to the private `.quarantine/<group>` directory, with their evidence retained. Cleanup processes the remaining valid markers and known IDs, reports the first failure at the end, and can complete on a later retry. It never deletes the workspace named by an invalid marker.

Fallback scanning visits the entire shared transcript directory instead of silently stopping after 256 entries. Files are streamed in 64 KiB chunks with overlap for a workspace string split between chunks; the old 1 MiB prefix limit no longer hides a match. A conversation tree exceeding the existing six-level/512-entry traversal bounds reports failure and keeps retry evidence, rather than claiming success after an incomplete scan. Workspace-marker writes/fsync, large scans and deletion run on the pool. Workspace/agent-file creation and init-ID records still perform small synchronous private writes on the polling path.

## Scheduling contract

`generation` (`send`, `send_ready`, `send_ready_with_policy`), `readiness` (`status`, `readiness`, `prepare`) and `cleanup` (`forget`, `cleanup`) have distinct limits. Ready requests rotate across apps; FIFO order is retained within an app and priority tier. Readiness and requests explicitly marked interactive receive preference for a bounded burst, after which eligible regular work must be admitted. Priorities do not preempt an already running request, bypass continuation/cleanup exclusion, or extend a queue deadline. A busy provider can still make a short generation wait for a generation slot.

Optional owner configuration lives in `scheduling.json` beside the broker's grants. It is read on startup; unknown fields and values outside the documented ceilings fail startup. The default is:

```json
{
  "max_running": 8,
  "max_app_running": 2,
  "max_provider_running": 2,
  "max_readiness_running": 2,
  "max_cleanup_running": 2,
  "interactive_burst": 3,
  "queue_timeout_ms": 30000
}
```

The global ceiling covers **all** classes. App/provider limits apply **within each class**, so two long generations no longer occupy the readiness allowance. Generation's global allowance is `max(1, max_running - max_readiness_running)`: six by default, leaving readiness capacity. Cleanup has a separate two-job ceiling and uses available global capacity. Smaller global limits still win over class limits. The hard ceilings are 8 globally, 2 per app/provider/class, 2 readiness jobs, 2 cleanup jobs, an interactive burst of 1–8, and a queue timeout of 1–900,000 ms. The queue remains capped at 64 requests globally and 8 per app; session caps remain 10,000 globally and 2,000 per app. Effective policy and worker limits appear in the broker telemetry record.

This changes the old aggregate app/provider caps: a provider can now run two generation processes plus two readiness processes concurrently, for four processes, subject to the global ceiling. Cleanup has its own allowance but existing conflict rules still exclude it from generation on the same app/provider.

A queued `cleanup` or `forget` establishes a drain barrier for its app/provider once its class/global/app/provider admission capacity is available. If another app occupies the cleanup lane, generations may continue until cleanup has a slot. Existing conflicting generations and earlier queued generations may finish, but newer generations for that pair cannot refill a freed slot before cleanup runs. Readiness and other apps/providers remain eligible. Cancellation or queue expiry removes the queued barrier; an admitted cleanup remains exclusive through filesystem and ledger acknowledgement. This prevents staggered generations from starving cleanup behind continually arriving work. The barrier persists through the last conflicting completion, so interactive work cannot overtake cleanup. A single-conversation `forget` conservatively drains all generations for the app/provider, not only that conversation. A long generation can therefore hold cleanup and newer generations until the queue deadline (30 seconds by default); cancellation/expiry releases queued work. The queue deadline still bounds the wait if existing work itself runs too long.

An optional top-level request field carries bounded hints:

```json
"scheduling": {
  "interactive": true,
  "queue_timeout_ms": 5000,
  "events": true
}
```

Clients may shorten the owner's queue timeout, never lengthen it. The clock is monotonic and starts at receipt; expiry yields one retryable `QUEUE_TIMEOUT` before provider launch. Queued cancellation yields `Stopped`. Invalid hint fields are refused. Queue and admission events are opt-in, preserving existing protocol-v1 clients:

```json
{"id":"r1","event":{"type":"queued","ahead":2,"timeout_ms":5000}}
{"id":"r1","event":{"type":"admitted"}}
```

`ahead` is the queue length at receipt, **not** a guaranteed admission order. `Queued` and `Admitted` are also available as neutral `Update` variants. Rust callers opt in through `RemoteProvider::with_scheduling(Hints { ... })` on either client path, or `RemoteClient::request_with_scheduling`. Queue events do not count as provider activity or extend provider timeouts. Cleanup work already admitted is a deletion obligation and continues if its requester disconnects.

The default 30-second queue deadline applies even when a client sends no scheduling hint. Previously those requests could wait indefinitely; they now receive retryable `QUEUE_TIMEOUT`. Clients can request a shorter wait; only owner configuration can increase it.

Adding `Update::Queued` and `Update::Admitted` is a Rust source compatibility change because `Update` is exhaustive. Before updating an application pin, add arms to every exhaustive match, including TabBeam's `native/host/src/host.rs` match. Applications own progress presentation and may ignore these two hints:

```rust
Update::Queued { .. } | Update::Admitted => {}
// Keep the application's existing arms for provider and terminal updates.
```

Scheduling events remain opt-in on the wire; opting out does not remove this compile-time migration requirement. No application pin is updated in this PR.

## Adaptive waiting and evidence

Commands still wake the hub's bounded inbox immediately. When no exchange or filesystem acknowledgement needs polling, the hub waits until the next authorization sweep (one second with connections), revocation prune (60 seconds), or queue expiry. With active exchanges, pending ledger work or cleanup, polling starts at 1 ms and ramps to 5 ms over four quiet ticks. Commands, admissions, exchange updates and filesystem results restart the 1 ms burst. Short turns avoid the old fixed 5 ms hops; long quiet provider/storage waits settle at the previous cadence. Earlier authorization/prune/queue timers still win. Command batching, cancellation, revocation checks and shutdown remain bounded.

The 1–5 ms values are requested waits, not an operating-system wakeup guarantee. Windows timer resolution can round these waits up; this slice does not change system timer resolution. The measured polling/latency improvements below are Linux evidence only. Tests await state with seconds-scale deadlines instead of assuming millisecond timer precision.

Windows stress validation reproduced the earlier named-pipe lock-poison failure in the pinned `interprocess` 2.2.3 cleanup dispatcher. The dependency is now pinned consistently to 2.4.4: upstream replaced that dispatcher with a runtime-independent linger pool in [2.3.0](https://github.com/kotauskas/interprocess/releases/tag/2.3.0) and fixed the subsequent linger-pool leak in [2.4.0](https://github.com/kotauskas/interprocess/releases/tag/2.4.0). This addresses a matching upstream failure mechanism; repeated Windows IPC CI supplies the product verification. It does not establish that F's wakeup changes had no effect on the old race.

The new Linux hub-only benchmark exercises the same hub and real fake-provider subprocesses without IPC, using an isolated fake home/configuration. It records aggregate thread CPU from `/proc/self/task/*/schedstat`, voluntary context switches, first-text/completion samples, a fresh readiness check while both generation slots are busy, and an interactive request behind six queued long requests. All eight long burst requests must also finish. It excludes initialization from the idle windows. It sends no real-provider prompts and measures no transport/network/model latency.

```sh
cargo build --release --locked -p seatline-bench -p seatline-companion
seatline-bench hub --idle-ms 3000 --samples 60 --label "F slice" --output hub.json
```

Before/after evidence and reproduction details: [F hub measurements](performance-baselines/2026-10-04-f-hub-summary.md). Socket-based benchmark scenarios and IPC tests still need an environment supporting AF_UNIX sockets. Linux measurements do not replace the existing macOS, Windows, MSRV, release, fuzz and application integration gates.
