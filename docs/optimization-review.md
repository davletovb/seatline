# Seatline Code & Architecture Review (`davletovb/seatline`)

**Audited Revision:** `db809b6` (all crates: `seatline-core`, `platform`, `providers`, `scheduler`, `service`, `companion`, workspace manifests, CI workflows, and docs)  
**Audit Lens:** Speed & resource efficiency, UX & developer/operator ergonomics, and overall reliability, security, and architecture — respecting Seatline's strict design boundaries (Rust-only, zero JS runtime, `#![forbid(unsafe_code)]`, runtime-vs-product-policy separation, and measurement-first rigor).

> [!NOTE]
> Items below are explicitly tagged **`[New]`** (newly identified in this review, including post-Oct-14 PRs `#20`–`#23`) or **`[Still Open — #18/#19]`** (confirmed still present at `db809b6` from prior architectural reviews `#18`/`#19` or [performance-implementation-tracker.md](performance-implementation-tracker.md)). Performance gains are framed as source-backed hypotheses to validate via `seatline-bench` and phase telemetry rather than unmeasured live-provider claims.

---

## Executive Summary & Priority Matrix

Seatline is already unusually disciplined for an early-stage systems codebase: workspace-wide `#![forbid(unsafe_code)]`, fail-closed security boundaries, secret-scrubbed phase telemetry, proper `default-features = false` client separation, fuzzing + runtime-independence CI gates, and empirical optimization discipline (`Reaper`, Claude `--safe-mode`, readiness caching).

The highest-leverage opportunities now fall into three clusters:
1. **Eliminate synchronous disk/CPU hot-path stalls on the single-threaded Hub & Web Service loops** (unthrottled per-poll disk writes in Grok, redundant full-file hashing on cached readiness hits, synchronous `fsync` on Gemini `Init`, and blocking `std::thread::sleep` inside the Tokio `current_thread` web daemon).
2. **Fix post-PR `#20`–`#23` operational reliability & security gaps** (broken `autostart` stop/restart on Windows & non-systemd Linux, un-detached cold-started broker dying on caller `Ctrl+C`, transient `try_send` disconnects on Hub bursts, and TOCTOU `0o644` → `0o600` private file writes).
3. **Close UX & adoption gaps** (CLI observability like `status`/`doctor`/`apps`/`links`, non-destructive `authorize`, multi-browser native messaging, Grok token streaming, typed readiness methods on [`RemoteClient`](../companion/src/remote.rs#L140-L235), and release packaging).

| Priority | Category | Finding | Effort | Tag |
| :--- | :--- | :--- | :--- | :--- |
| **P0** | Speed | Grok writes `.my-app-owner` to disk on *every* poll tick | Tiny | `[New]` |
| **P0** | Reliability | [`links::change`](../companion/src/links.rs#L109-L138) calls blocking `std::thread::sleep` (up to 10s) inside single-threaded Tokio | Small | `[New]` |
| **P0** | Reliability | Cold-started broker in [`start_companion`](../companion/src/startup.rs#L104-L129) not detached from process group / Windows console | Tiny | `[New]` |
| **P0** | Security | [`write_private_file`](../platform/src/private_fs.rs#L23-L31) creates files with default umask (`0o644`) before `chmod 0o600` | Tiny | `[New]` |
| **P0** | Reliability | [`autostart`](../companion/src/autostart.rs#L275-L397) cannot stop/restart running `web` service on Windows or non-systemd Linux | Medium | `[New]` |
| **P1** | Speed | Cached readiness re-reads & re-hashes full config/auth files synchronously on Hub thread | Medium | `[New]` / `[Still Open — #19]` |
| **P1** | Speed | Gemini runs synchronous `sync_all()` (`fsync`) on Hub thread inside [`Turn::on_line`](../providers/src/gemini/mod.rs#L973-L993) (`Init`) | Small | `[New]` |
| **P1** | Speed | Hub & providers double-clone prompts and hold up to 8 MiB of prompt DOM in [`Active`](../companion/src/hub.rs#L180-L201) | Small | `[New]` |
| **P1** | Speed | Continuous idle background disk I/O (1s + 2s polling timers) in [`web.rs`](../companion/src/web.rs#L351-L364), [`links.rs`](../companion/src/links.rs#L257-L298), [`hub.rs`](../companion/src/hub.rs#L871-L883) | Small | `[New]` |
| **P1** | Reliability | [`connection_loop`](../companion/src/main.rs#L541-L553) disconnects whole client via `try_send` if Hub inbox (128) is transiently full | Small | `[New]` |
| **P1** | UX | Missing `status`, `apps`, `links`, `doctor`, and `config` CLI commands + rough `--help`/errors | Medium | `[New]` |
| **P1** | UX / Speed | Grok disables streaming (`CAPABILITIES.streaming = Unsupported`) while paying `--include-partial-messages` parse cost | Small | `[New]` |
| **P1** | Correctness | [`ToolPolicy::ProviderDefault`](../seatline-core/src/turn.rs#L11-L24) silently violated by Claude, Gemini, Grok (and partially Codex) | Medium | `[Still Open — #19]` |
| **P2** | Speed | Event-driven wakeups for Hub, Service, [`Process`](../seatline-core/src/process.rs#L304-L321), and single shared [`Reaper`](../seatline-core/src/process.rs#L641-L672) thread | Medium | `[Still Open — #19]` |
| **P2** | Speed | Typed zero-DOM JSON parsing in provider `output::parse()` + pass-through framing in [`bridge`](../companion/src/main.rs#L624-L666) | Medium | `[New]` |
| **P2** | UX | Typed readiness APIs on [`RemoteClient`](../companion/src/remote.rs#L217-L235), `#[non_exhaustive]` on [`Update`](../seatline-core/src/exchange.rs#L31), multi-browser `install` | Medium | `[New]` |
| **P2** | Build / UX | Add `[profile.release]` (`lto`, `codegen-units`, `strip`) + GitHub Releases workflow & installers | Small | `[New]` |

---

## 1. Speed, Latency & Resource Efficiency

### 1.1 Hot-Path Disk I/O & Single-Thread Hub Stalls

#### 1. Grok writes its owner heartbeat file to disk on *every single poll tick* `[New]`
- **Where:** [`StatusCheck::next`](../providers/src/grok/mod.rs#L570-L572), [`ProbeWorkspace::touch`](../providers/src/grok/mod.rs#L680-L682), [`TurnWorkspace::touch`](../providers/src/grok/mod.rs#L732-L734), [`Turn::next`](../providers/src/grok/mod.rs#L914-L917)
- **What happens:** Both `Turn::next` and `StatusCheck::next` unconditionally execute `fs::write(self.path.join(&self.owner), b"live")` every time the scheduler polls them. Inside [`Scheduler::poll_into`](../scheduler/src/lib.rs#L340-L353), `next()` is called repeatedly for every single emitted `Update` and on every 1–5 ms Hub tick — producing **hundreds to thousands of synchronous file open/truncate/write/close syscalls per second** on the Hub thread just to refresh a heartbeat whose staleness timeout (`STALE_WORKSPACE_AFTER` in [grok/mod.rs](../providers/src/grok/mod.rs#L52)) is **15 minutes**.
- **Fix:** Store `last_touch: Instant` on [`ProbeWorkspace`](../providers/src/grok/mod.rs#L659-L664) and [`TurnWorkspace`](../providers/src/grok/mod.rs#L699-L706) and only rewrite when `last_touch.elapsed() >= Duration::from_secs(60)`.

#### 2. Cached readiness re-reads and re-hashes full config/auth files synchronously on the Hub thread `[New]` / `[Still Open — #19]`
- **Where:** [`Key::watch`](../providers/src/readiness.rs#L43-L79), [`CachedHit::next`](../providers/src/readiness.rs#L527-L585), [`Codex::readiness_key`](../providers/src/codex/mod.rs#L272-L300), [`Codex::capabilities`](../providers/src/codex/mod.rs#L317-L324), [`context_configuration_is_safe`](../providers/src/codex/mod.rs#L445-L481), [`workspace::prepare_for`](../platform/src/workspace.rs#L60-L72)
- **What happens:**
  1. [`CachedHit::next`](../providers/src/readiness.rs#L533-L572) validates `self.ready.provider.readiness_key()` at both consumption boundaries — at `cursor == 0` (before emitting `Update::Status`) and again at `cursor == 1` (before emitting `Update::Completed` / launching a turn). That second check is required because those polls are not guaranteed to occur in the same scheduler slice and external config/account edits do not bump the readiness epoch (covered by [`delayed_cached_exchanges_revalidate_invalidation_and_configuration_before_launch`](../providers/src/readiness.rs#L1287-L1304)), which means any uncached I/O inside `readiness_key()` is paid twice per cached hit.
  2. [`Key::watch`](../providers/src/readiness.rs#L60-L72) opens every watched file, reads up to 1 MiB (`WATCH_BYTES`) into a fresh heap `Vec<u8>`, and hashes it from scratch on every call, with **no `(FileStamp, digest)` cache** (unlike [`AccountFile`](../providers/src/claude/readiness.rs#L48-L54) in Claude readiness, which already memoizes by `FileStamp`).
  3. [`Codex::readiness_key`](../providers/src/codex/mod.rs#L272-L300) calls `self.capabilities()` → [`context_configuration_is_safe`](../providers/src/codex/mod.rs#L445-L481), performing a *second* `fs::read_dir(CODEX_HOME)` and *second* full `fs::read_to_string(config.toml)` + `.to_ascii_lowercase()`. Across both consumption-boundary checks, a single cached Codex readiness hit performs **4× `read_dir`, 4× `config.toml` reads, 2× `auth.json` reads, and 2× workspace ancestor walks** synchronously on the single-threaded Hub.
  4. [`workspace::prepare_for`](../platform/src/workspace.rs#L60-L72) chains `dir.ancestors().skip(1)` and `resolved.ancestors().skip(1)` even when `dir == resolved` (calling `symlink_metadata` on every ancestor twice) and calls `private_group()` (`getpwuid_r`/`getgrgid_r`) per group-writable ancestor on every single turn launch.
- **Fix:**
  - **Retain `readiness_key()` validation at both consumption boundaries** (`cursor == 0` and the completion/launch boundary at `cursor == 1`) so external account or configuration edits between polls still return `READINESS_CHANGED` and launch zero turns, and make each check cheap rather than skipping the second validation.
  - Add a small `FileStamp → digest` memoization layer inside [`readiness.rs`](../providers/src/readiness.rs#L43-L79) (matching Claude's `AccountFile` pattern) so unchanged watched files only pay a metadata check instead of re-reading and re-hashing full files, and compute Codex's `context_configuration_is_safe` from the same single read pass (cached by `FileStamp`).
  - Skip the second ancestor chain in [`workspace::prepare_for`](../platform/src/workspace.rs#L60-L72) when `resolved == dir`, and cache current UID's private group check once per process via `OnceLock`.

#### 3. Gemini runs synchronous `fsync` (`sync_all`) on the Hub thread during `Line::Init` `[New]`
- **Where:** [`Turn::register_cleanup_id`](../providers/src/gemini/mod.rs#L948-L959), [`Turn::on_line`](../providers/src/gemini/mod.rs#L991-L992), [`write_cleanup_record`](../providers/src/gemini/mod.rs#L1365-L1368), [`record_pending_cleanup_id`](../providers/src/gemini/mod.rs#L1473-L1483)
- **What happens:** Pre-launch workspace recording was carefully offloaded to `Worker::cleanup()`, yet when `Line::Init` arrives during streaming, `register_cleanup_id()` calls `write_cleanup_record()` → `sync_all()` synchronously on the single-threaded Hub thread. On APFS / busy disks, `fsync` stalls for 5–50+ ms, pausing token delivery for *all* other concurrent providers and apps on the broker. Similarly, [`TurnWorkspace::create`](../providers/src/gemini/mod.rs#L439) runs ~5 synchronous directory/file creation calls directly inside `send()`.
- **Fix:** Dispatch `record_pending_cleanup_id` asynchronously onto `Worker::cleanup()` (or buffer the ID in memory and await durability only before `clean_runtime` / completion), and consider offloading `TurnWorkspace::create` onto the same prepare stage where `record_workspace` already runs.

#### 4. Continuous idle background disk I/O in `links`, `web`, and `hub` `[New]`
- **Where:** [`links::serve`](../companion/src/links.rs#L257-L261), [`Service::reconcile`](../companion/src/links.rs#L274-L298), [`web::connection`](../companion/src/web.rs#L351-L364), [`Hub::authorized`](../companion/src/hub.rs#L386-L391), [`Hub::admit`](../companion/src/hub.rs#L1162)
- **What happens:**
  - `links::serve` wakes every 2 seconds indefinitely to scan `apps/`, open and JSON-parse every grant file, and open and JSON-parse every `web/*.json`.
  - *Every active browser link* inside [`web::connection`](../companion/src/web.rs#L351-L364) independently runs its own 1-second timer calling `config::load_grant()` — redundant when running under `links::serve` (8 links = 8+ extra file opens & JSON parses per second on idle laptops).
  - [`Hub::authorized`](../companion/src/hub.rs#L386-L391) reads and JSON-parses the grant file synchronously twice per request (at enqueue + admit) and once per connection every second, even when multiple connections share an app.
- **Fix:** Cache `(FileStamp, Grant)` by app name in `Hub` (checking `stat` metadata before re-reading/re-parsing), and let `links::Service` own revocation detection (passing a `watch` / `CancellationToken` to child `web::run_channel` tasks, or comparing directory/file `FileStamp` before reading JSON).

#### 5. Claude `StatusCheck` waits for CLI exit even after complete JSON is printed `[New]`
- **Where:** [`StatusCheck::next`](../providers/src/claude/mod.rs#L633-L670)
- **What happens:** PR `#20` added `reported_sign_in(output)` on timeout/unknown exit because Claude CLI prints its JSON status payload (`loggedIn`, `authMethod`) before taking noticeable extra time to tear down and exit. However, `StatusCheck::next` still waits for `Event::Exited` (or the full `give_up` timeout) instead of accepting a complete, valid JSON status payload on `Event::Stdout` and releasing the exiting process to `Reaper` (just as `Turn::release_finished` already does for turns).
- **Fix:** On `Event::Stdout`, once `output` parses as a complete JSON object with a definitive `reported_sign_in(output)` (and `sign_in` classification), emit `Update::Status` + `Update::Completed` immediately and hand `process` to `Reaper::shared().release(...)`. Benchmark against `seatline-bench` to measure the probe-latency win.

---

### 1.2 Allocations, Cloning & Serialization Overhead

#### 6. Prompt double-cloning & up-to-8 MiB long-lived RAM retention on the Hub `[New]`
- **Where:** [`Active`](../companion/src/hub.rs#L180-L201), [`Hub::enqueue`](../companion/src/hub.rs#L499), [`Hub::eligible`](../companion/src/hub.rs#L1123-L1129), [`Hub::build`](../companion/src/hub.rs#L1365)
- **What happens:** When a turn arrives, `value["params"].clone()` copies the entire prompt JSON tree into `Request.params`; `Hub::build` clones `request.params` *again* to deserialize `Turn`; and `Active` then retains the entire `Request.params` DOM (up to 1 MiB × 8 concurrent turns = 8 MiB of `serde_json::Value` nodes, often 3–5× larger in RAM than raw UTF-8) for up to 15 minutes solely so `eligible()` can inspect `request.continuation()`.
- **Fix:** Extract `continuation: Option<String>` once when constructing `Request`, move `value["params"].take()` instead of `.clone()`, and `.take()` `request.params` inside `Hub::build` so the prompt DOM is freed as soon as the provider turn starts.
- **Related prompt copies:**
  - [`claude/mod.rs`](../providers/src/claude/mod.rs#L870-L878) and [`gemini/mod.rs`](../providers/src/gemini/mod.rs#L474-L480) construct `serde_json::json!({ ... &self.prompt }).to_string() + "\n"` (cloning the entire prompt string into a `Value` and then reallocating to append `\n`). Use a tiny borrowed `#[derive(Serialize)]` struct serialized straight into a `Vec<u8>` + `push(b'\n')`.
  - [`prompt::render_with_intro`](../seatline-core/src/prompt.rs#L72-L97) calls `serde_json::json!({ "role": role, "text": message.text }).to_string()` per historical message. Use a borrowed `#[derive(Serialize)]` struct serialized directly into a pre-sized `String`/`Vec<u8>`.

#### 7. Double JSON serialization & redundant parse/re-serialize hops on IPC and native messaging `[New]`
- **Where:** [`RemoteClient::request_with_scheduling`](../companion/src/remote.rs#L172-L181), [`write_to_extension`](../companion/src/main.rs#L608-L622), [`bridge`](../companion/src/main.rs#L624-L666), [`wire::write_frame`](../companion/src/wire.rs#L100-L108)
- **What happens:**
  - `request_with_scheduling` calls `serde_json::to_vec(&frame)` solely to check `> wire::MAX_FRAME`, throws away the bytes, and then `wire::write_frame` serializes the exact same `Value` a second time.
  - Native-messaging [`bridge`](../companion/src/main.rs#L648-L662) deserializes every frame from Chrome stdin into a full `serde_json::Value` DOM only to re-serialize it verbatim to the broker socket (and does the reverse on stdout). Passing raw validated `Vec<u8>` frames eliminates two full JSON DOM round-trips per streamed token.
  - Both [`wire::write_frame`](../companion/src/wire.rs#L105-L107) and [`write_to_extension`](../companion/src/main.rs#L613-L616) issue 2 separate `write_all` syscalls (4-byte length prefix, then body) instead of writing into one contiguous `[len_bytes | body]` buffer.
  - [`remote.rs`](../companion/src/remote.rs#L578), [`client.rs`](../companion/src/client.rs#L252), and [`wire.rs`](../companion/src/wire.rs#L181-L192) clone owned `Value` subtrees where `.take()` or `remove()` works with zero allocation.

#### 8. Per-token `serde_json::Value` DOM allocation across all 4 provider parsers & per-frame AAD allocations `[New]`
- **Where:** `providers/src/{claude,codex,gemini,grok}/output.rs`; [`secure::to_hex`](../companion/src/secure.rs#L60-L62), [`secure::data_aad`](../companion/src/secure.rs#L79-L86), [`config::random_token`](../companion/src/config.rs#L171-L175)
- **What happens:**
  - Every streaming JSON line from Claude, Codex, Gemini, and Grok is parsed into an untyped `serde_json::Value` (allocating `BTreeMap`s + `String`s for every key and nested field) just to extract a small text delta. This is particularly wasteful in **Grok** ([grok/mod.rs](../providers/src/grok/mod.rs#L760), [grok/output.rs](../providers/src/grok/output.rs#L102)), which passes `--include-partial-messages` so the CLI emits a JSON event per token, full-DOM-parses every single one, and then throws the text away as `Line::Activity`!
  - In [`secure::data_aad`](../companion/src/secure.rs#L79-L86), every encrypted WebSocket frame calls `to_hex()` twice — and `to_hex()` does `bytes.iter().map(|byte| format!("{byte:02x}")).collect()` (16 separate heap allocations per nonce + outer `String` + `format!()` = **~35 heap allocations per frame**).
- **Fix:**
  - Precompute the static epoch AAD prefix `"seatline:2:{direction}:{helper_hex}:{browser_hex}:"` once per handshake (or format into a stack `[u8; 128]` buffer), and rewrite `to_hex` with a single `String::with_capacity(bytes.len() * 2)` lookup table.
  - Replace hot-path `serde_json::Value` parsing in `output.rs` with lightweight borrowed `#[derive(Deserialize)]` structs (falling back to `Value` only for rare `init`/`result`/error lines if needed).

#### 9. Remaining Hub, Ledger, Discovery & Forget micro-inefficiencies `[Still Open — #19]` / `[New]`
- [`Hub::enqueue`](../companion/src/hub.rs#L503-L519): Linearly scans both `active` and `queue` for duplicate `(connection, id)` on every request; a `HashSet<(u64, String)>` makes lookup `O(1)`.
- [`Hub::next_request`](../companion/src/hub.rs#L1132-L1138) → [`Hub::eligible`](../companion/src/hub.rs#L1092-L1097): Allocates a fresh `running: Vec<_>` *per queued request* (`O(Q × A)` allocations per tick). Hoist `running` once in `next_request()`.
- [`Connection::hold`](../companion/src/hub.rs#L149): Serializes non-delta events with `serde_json::to_string` solely to measure byte length for the backpressure budget. Use a zero-allocation `io::Write` byte-counting sink (`CountingWriter`).
- [`Sessions::resolve`](../companion/src/sessions.rs#L27-L38): Allocates 3 `String`s to build the lookup key `(app, provider, token)`; use a borrowed lookup key trait (`Equivalent` / composite key) for zero-allocation lookups.
- [`Ledger::record`](../companion/src/ledger.rs#L33): Rewrites and `fsync`s the entire `sessions.json` (up to 10,000 entries) on every new persistent session. Moving to append-only JSONL (with periodic compaction) turns `O(N)` full-file rewrites into `O(1)` appends.
- [`discovery/cache.rs`](../seatline-core/src/discovery/cache.rs#L116-L117) calls `is_executable` (`stat`) and immediately calls `FileStamp::read` (a second `stat`); [`discovery.rs`](../seatline-core/src/discovery.rs#L44-L53) does not deduplicate duplicate `PATH` entries; [`platform::layout::data_dir`](../platform/src/layout.rs#L144-L146) collects *every* process environment variable into a `Vec<(OsString, OsString)>` just to read 1–2 keys (`SEATLINE_DATA_DIR`, `XDG_DATA_HOME`/`HOME`).
- [`forget::head_lines`](../platform/src/forget.rs#L114-L134) reads up to 1 MiB into a `Vec<String>` when callers ([`claude/mod.rs`](../providers/src/claude/mod.rs#L602), [`codex/mod.rs`](../providers/src/codex/mod.rs#L562)) typically match on line 1. A streaming `BufReader::read_line` callback stops immediately after the first match.

---

### 1.3 Polling Jitter, Process Lifecycle & Build Profile

#### 10. Timer-based polling jitter & per-process Reaper threads `[Still Open — #19]` / `[New]`
- **Where:** [`Runtime` loop](../service/src/lib.rs#L308) (10 ms `recv_timeout`), [`Hub::wait_time`](../companion/src/hub.rs#L850-L868) (1→5 ms ramp), [`Process::next_event`](../seatline-core/src/process.rs#L304-L321) (10 ms `POLL_INTERVAL`), [`Reaper::release`](../seatline-core/src/process.rs#L641-L672), [`join_threads`](../seatline-core/src/process.rs#L746-L756)
- **What happens:**
  - While an active turn waits for output, `Process::next_event` wakes every 10 ms to call `try_wait()`, even while stdout/stderr pipes are still open (`open_streams > 0`). Combined with the 1–5 ms Hub tick and 10 ms Service tick, there is continuous background CPU wakeup while a model thinks, plus up to 5–10 ms jitter on first-token delivery.
  - [`Reaper::release`](../seatline-core/src/process.rs#L641-L672) spawns a **new OS thread** (plus a `sync_channel(1)` polling every 20 ms) for *every* released process, and [`join_threads`](../seatline-core/src/process.rs#L746-L756) spins on 1 ms `thread::sleep`.
- **Fix:**
  - Replace per-process Reaper threads with **one single persistent background reaper thread** draining a bounded list of released children.
  - Add a lightweight optional wakeup hook (e.g., `Arc<(Mutex<bool>, Condvar)>` or `tokio::sync::Notify`) signaled by `Process` pipe-reader threads when bytes/EOF arrive, so `Hub` and `Runtime` sleep with 0 Hz wakeups during model think time and wake in <100 µs on delta arrival.

#### 11. Untuned `[profile.release]` & dependency graph duplicates `[New]`
- **Where:** Root [Cargo.toml](../Cargo.toml#L1-L13) and `Cargo.lock`
- **What we verified:**
  - The workspace has **no `[profile.release]`** section. Adding `lto = "thin"`, `codegen-units = 1`, and `strip = "symbols"` (or `"debuginfo"`) substantially shrinks `seatline-companion` binary size (critical since `install` copies the binary per version into `versions/`) and improves hot-path inlining across crate boundaries (`seatline-core` ↔ `providers` ↔ `companion`).
  - `Cargo.lock` contains 199 packages (very lean overall — single `rustls` + `ring` TLS stack, `reqwest`/`tokio-tungstenite` properly gated behind the `web` feature), with 8 minor duplicate crates (`getrandom` ×3: 0.2/0.3/0.4, `rand`/`rand_core` ×2–3, `syn` ×2, `windows-sys` ×2, `cpufeatures`, `r-efi`, `synstructure`) mainly from `aes-gcm`/`ring`/`reqwest`/`interprocess` transitive boundaries. Worth tracking periodically via `cargo tree -d` as upstream crypto crates converge on `getrandom 0.3`/`0.4`.

---

## 2. UX, CLI & Developer/Operator Experience

### 2.1 CLI & Operator Experience

#### 1. Missing observability & self-diagnosis commands `[New]`
- **Where:** [`companion/src/main.rs`](../companion/src/main.rs#L34-L308)
- **Gap:** Today a user or support engineer cannot inspect what Seatline is doing without manually digging through OS-specific JSON files (`apps/*.json`, `web/*.json`, `scheduling.json`, lockfiles). There is no:
  - `seatline-companion status` — show install path, running vs installed broker version, whether `web` service & `autostart` are active, connected link count, and provider discovery/auth summary.
  - `seatline-companion apps` (or `list`) — list authorized apps, allowed providers, origins, relays, and active browser link counts (redacting tokens).
  - `seatline-companion doctor` — validate `scheduling.json` syntax/bounds, check native messaging manifest paths, verify socket/lock health, and detect CLI binaries (`claude`, `codex`, `gemini`/`agy`, `grok`) on `PATH`.
  - `seatline-companion config` (or `scheduling`) — validate and update `scheduling.json` safely instead of requiring hand-editing JSON (where a single typo currently fails broker startup silently behind a 5-second timeout in [`startup.rs`](../companion/src/startup.rs#L127)).

#### 2. Manual CLI parsing, unreadable `--help`, and stale error hints `[New]`
- **Where:** [`main.rs`](../companion/src/main.rs#L34-L308), [`web.rs`](../companion/src/web.rs#L251), [`web.rs`](../companion/src/web.rs#L331)
- **Issues:**
  - `--help` / `-h` / `help` exits with **status code 1** and prints a single dense ~320-character usage line ([`main.rs`](../companion/src/main.rs#L294-L305)).
  - `revoke <unknown-app>` ([`main.rs`](../companion/src/main.rs#L206)) bubbles up raw OS `No such file or directory (os error 2)` instead of `app "<name>" is not authorized`.
  - [`grant_ended`](../companion/src/web.rs#L251) and [`connection`](../companion/src/web.rs#L331) still instruct users to `"run \`pair\` again"` even when the user connected via the newer `seatline-companion link` workflow.
  - `version` / `--version` is not exposed as a CLI flag.

#### 3. Destructive `authorize` behavior & `link --open` friction `[New]`
- **Where:** [`main.rs`](../companion/src/main.rs#L157-L183), [`main.rs`](../companion/src/main.rs#L106-L121), [`web.rs`](../companion/src/web.rs#L486-L520)
- **Issues:**
  - Re-running `authorize` to add a provider or origin silently rotates the token and **wipes** prior origins, relays, `register-native` adapter registration, and all active browser links (`links::load` checks token match). Adding additive flags (`--add-provider`, `--add-origin`, `--keep-links`) or printing an explicit summary of what was reset/invalidated avoids surprising users.
  - `link --open` unconditionally prints the raw secret pairing URL to terminal stdout (polluting scrollback/logs), sleeps for a hardcoded **5 seconds** before exiting, and uses a temp `.html` `<meta http-equiv="refresh">` file that remains on disk with the secret if the process is interrupted and can be blocked or have `#fragment` stripped by strict browser security profiles. Consider a `--quiet` / `--open-only` option, an explicit cleanup trap / short-lived local one-shot redirect (or clipboard option), and starting `web` automatically or offering a prompt when `web` isn't running.

#### 4. Native messaging `install` only supports Google Chrome `[New]`
- **Where:** [`install::register`](../companion/src/install.rs#L39-L78)
- **Gap:** `install` writes `com.seatline.host.json` strictly into Google Chrome's `NativeMessagingHosts` directory/registry key, omitting Chromium, Brave, Microsoft Edge, Vivaldi, Arc, and Chrome Beta/Canary — all of which use the exact same Chrome Native Messaging JSON schema. Detecting installed Chromium-family browser config directories (or supporting `--browser=...` / registering in every existing Chromium-family host dir) removes a major friction point for extension users.

---

### 2.2 Provider Feature & Streaming UX

#### 5. Grok forces non-streaming UX while already requesting partial messages `[New]`
- **Where:** [`grok/mod.rs`](../providers/src/grok/mod.rs#L56), [`grok_args`](../providers/src/grok/mod.rs#L760), [`grok/output.rs`](../providers/src/grok/output.rs#L102)
- **Impact:** Grok declares `streaming: Capability::Unsupported`, so users stare at a blank indicator until the entire completion finishes — even though `grok_args` *already* passes `--include-partial-messages` and receives every `stream_event` delta!
- **Fix:** Either parse `content_block_delta` / `text_delta` from `stream_event` (handling deduplication against the final `assistant` message cleanly, as Claude already does) and promote Grok to `Capability::Supported`, **or** remove `--include-partial-messages` if Grok's partial stream is intentionally untrusted so Seatline stops paying the serialization + DOM-parse tax for discarded tokens.

#### 6. Codex returns an empty suggested-models list & `Capabilities::reasoning_effort` lacks level granularity `[New]`
- **Where:** [`codex::status_update`](../providers/src/codex/mod.rs#L495), [`Capabilities`](../seatline-core/src/protocol.rs#L109-L120)
- **Impact:**
  - Codex returns `models: Cow::Borrowed(&[])`, forcing every host app to hardcode Codex model IDs if it renders a model selector (unlike Claude/Gemini/Grok which populate `models`).
  - `Capabilities::reasoning_effort` is a single tri-state `Capability` flag, so apps cannot know from capability discovery that Claude rejects `ReasoningEffort::None` (failing at runtime with `REASONING_EFFORT_UNSUPPORTED`) or which levels each provider accepts. Adding an optional `reasoning_efforts: Cow<'static, [ReasoningEffort]>` (or similar additive capability metadata) lets UIs render valid controls cleanly without hardcoding provider quirks.

---

### 2.3 Rust Consumer / SDK Ergonomics & Distribution

#### 7. [`RemoteClient`](../companion/src/remote.rs#L140-L235), [`RemoteProvider`](../companion/src/client.rs#L93-L212), [`seatline-core`](../seatline-core/src/exchange.rs#L31) & [`seatline-service`](../service/src/lib.rs) ergonomics `[New]`
- **Missing Slice C methods on `RemoteClient`:** [`RemoteClient`](../companion/src/remote.rs#L217-L235) exposes `status`, `send`, `cleanup`, and `request_with_scheduling`, but has no typed `readiness(...)`, `prepare(...)`, or `send_ready(...)` helpers — forcing Rust consumers to manually assemble raw JSON frames for readiness-aware flows.
- **Eager thread + persistent broker pin:** `RemoteClient::new()` eagerly spawns a dedicated OS thread + Tokio runtime + persistent socket connection before any request is made, preventing the shared broker from idle-exiting even while the app sits idle. Optional lazy connect + `idle_timeout` would eliminate idle background resources.
- **`RemoteProvider::with_client` awkwardness & silent cleanup error drop:** [`RemoteProvider::with_client`](../companion/src/client.rs#L93) requires callers to construct a full local `&dyn Provider` just to copy static metadata (`id`, `supports_persistent_session`, `supports_preparation`), and [`run_cleanup`](../companion/src/client.rs#L200-L212) discards the `Update::Failed` error reason instead of surfacing it.
- **Core API evolution & error traits:**
  - [`Update`](../seatline-core/src/exchange.rs#L31) is missing `#[non_exhaustive]` (unlike [`ErrorCode`](../seatline-core/src/protocol.rs#L11)), so any new wire/lifecycle event is a breaking change for downstream Rust matches.
  - [`TurnError`](../seatline-core/src/turn.rs#L211) does not implement `std::fmt::Display` or `std::error::Error`, making `?` ergonomics clumsy in `anyhow`/typed-error applications.
- **`seatline-service` parity gap:** [`Runtime`](../service/src/lib.rs) only handles `send` / `cancel` and lacks readiness/prepare/status/cleanup/telemetry parity with `seatline-companion` for embedded single-app deployments.

#### 8. Packaging & release distribution gap `[New]`
- **Where:** `.github/workflows/` (only `ci.yml` and `live-*.yml`; zero GitHub Releases)
- **Impact:** End users who want to pair a web app or Chrome extension currently must install a Rust toolchain and compile from source. Adding a tag-triggered release workflow building stripped cross-platform archives (macOS arm64/x64, Linux x64/arm64, Windows x64) with SHA-256 checksums, plus a curl/PowerShell installer or Homebrew tap, will dramatically lower onboarding friction once the wire protocol stabilizes.

---

## 3. Overall: Correctness, Reliability, Security & Architecture

### 3.1 Post-PR `#20`–`#23` Bugs & Reliability Issues

#### 1. Blocking `std::thread::sleep` inside single-threaded Tokio runtime freezes all browser links `[New]`
- **Where:** [`links::change`](../companion/src/links.rs#L109-L124), [`Service::keep`](../companion/src/links.rs#L325), [`main.rs`](../companion/src/main.rs#L82-L85)
- **What happens:** `seatline-companion web` runs on `tokio::runtime::Builder::new_current_thread()` + `LocalSet`. When any link finishes or errors, `Service::keep()` calls synchronous `links::change()`, which retries `links.lock` with **`std::thread::sleep(Duration::from_millis(20))` for up to 10 seconds (`LOCK_WAIT`)** — blocking the *only* Tokio worker thread and freezing pings, handshakes, and streaming deltas for every other browser link!
- **Fix:** Provide an async `change_async()` using `tokio::time::sleep` (or run blocking lock+file I/O via `tokio::task::spawn_blocking`).

#### 2. `autostart` stop/restart is broken on Windows and non-systemd Linux `[New]`
- **Where:** [`autostart.rs`](../companion/src/autostart.rs#L275-L321), [`autostart.rs`](../companion/src/autostart.rs#L365-L397), [`links::serve`](../companion/src/links.rs#L233-L250)
- **What happens:** Unlike macOS (`launchctl bootout`) and systemd Linux (`systemctl --user restart`/`disable --now`), Windows and non-systemd Linux have **no way to signal or stop** a running `seatline-companion web` process:
  - During `seatline-companion install` (upgrade), `start_detached(new_exe)` launches the new binary, hits `web-service.lock` held by the *old* binary, sleeps 10 seconds, prints `"the web service is already running"`, and exits — silently leaving the stale version running!
  - During `autostart off` or `revoke` of the last web app, the login entry is deleted, but the running background process keeps running until logout.
- **Fix:** Mirror `control::stop` (already implemented for `serve` via a local control socket/pipe in [control.rs](../companion/src/control.rs)) with a `web-control` socket or shutdown sentinel watched by `links::serve` so `install`, `autostart off`, and `revoke` cleanly stop/replace the web service on all platforms.

#### 3. Cold-started broker is not detached from the caller's process group or Windows console `[New]`
- **Where:** [`startup::start_companion`](../companion/src/startup.rs#L104-L129)
- **What happens:**
  - Unlike [`autostart.rs`](../companion/src/autostart.rs#L270) (`.process_group(0)`) and [`autostart.rs`](../companion/src/autostart.rs#L360) (`CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`), `start_companion` sets neither flag.
  - On Unix, pressing `Ctrl+C` in any terminal app that happened to cold-start the shared broker sends `SIGINT` to the entire process group, **killing the shared broker and aborting active turns for every other connected app/extension**.
  - On Windows GUI apps, spawning `seatline-companion.exe serve` without `CREATE_NO_WINDOW` flashes a visible console window.
  - `start_companion` also discards `stderr(Stdio::null())`, so fatal startup errors (e.g., invalid `scheduling.json` or bad permissions) are hidden behind a 5-second timeout and generic connection error.
- **Fix:** Apply `.process_group(0)` on Unix and `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` on Windows; optionally capture the first ~4 KiB of `stderr` (via a short pipe closed once ready, or a bounded `broker.log` file) to surface actionable startup diagnostics if the child exits early.

#### 4. Transient Hub inbox saturation disconnects the entire client connection `[New]`
- **Where:** [`connection_loop`](../companion/src/main.rs#L541-L553)
- **What happens:** While PR `#21` fixed outbound 64-slot delta drops in `Hub` and `bridge()`, inbound `Command::Request` in `connection_loop` still uses non-blocking `hub.try_send(...)` into a 128-slot channel and tears down the entire shared connection with `"broker busy"` if momentarily full (unlike `Command::Close` and `Command::Quiet`, which await/retry). A burst of concurrent cancels/requests across apps can needlessly kill healthy connections.
- **Fix:** Use `tokio::time::timeout(..., hub.send(...))` (e.g., 2–5s) so brief Hub scheduling/I/O pauses apply backpressure rather than disconnecting the app.

#### 5. Web relay `connection()` missing broker handshake timeouts, head-of-line blocking, and frame-size mismatch `[New]`
- **Where:** [`web::send_within`](../companion/src/web.rs#L303-L311), [`web::connection`](../companion/src/web.rs#L372-L448)
- **Issues:**
  - `wire::write_frame` ([`web.rs:422`](../companion/src/web.rs#L422), [`web.rs:448`](../companion/src/web.rs#L448)) and `wire::read_frame` ([`web.rs:423`](../companion/src/web.rs#L423)) during the broker handshake/forwarding have **no timeout**, unlike [`write_frame_within`](../companion/src/main.rs#L564). A wedged local IPC call hangs the relay loop indefinitely.
  - Calling `send_within(&mut socket, ..., timing.dead_after).await?` directly inside the `tokio::select!` arms ([`web.rs:372, 381, 436`](../companion/src/web.rs#L372-L382)) head-of-line blocks the entire connection for up to `dead_after` (40–75s) when outbound TCP buffers fill — preventing incoming `cancel` frames, pings, or revocation checks from running. Splitting the WebSocket into `Sink` + `Stream` with a bounded outbound task fixes this cleanly.
  - Frame-size asymmetry: local IPC allows `wire::MAX_FRAME` = 1 MiB ([`wire.rs:8`](../companion/src/wire.rs#L8)), while the relay loop rejects incoming text frames `> 768 KiB` ([`web.rs:399`](../companion/src/web.rs#L399)) and has no pre-send size guard for AES-GCM + base64 expansion (~1.33×). A ~600 KiB prompt passes local validation, inflates past 768 KiB over the relay, and triggers `End::retry` (an endless reconnect loop rather than a structured `INVALID_REQUEST` / `FRAME_TOO_LARGE` error).

---

### 3.2 Contract & Edge-Case Bugs

#### 6. `ToolPolicy::ProviderDefault` still silently disabled across providers `[Still Open — #19]`
- **Where:** [`claude/mod.rs`](../providers/src/claude/mod.rs#L763-L767), [`gemini/mod.rs`](../providers/src/gemini/mod.rs#L419), [`gemini/mod.rs`](../providers/src/gemini/mod.rs#L1027-L1042), [`grok/mod.rs`](../providers/src/grok/mod.rs#L454), [`grok/mod.rs`](../providers/src/grok/mod.rs#L769-L776), [`codex/mod.rs`](../providers/src/codex/mod.rs#L618-L619)
- **What happens:** Even when an app explicitly authorizes `--allow-provider-default` and requests `ToolPolicy::ProviderDefault`, Claude still passes `--tools "" --strict-mcp-config --disallowedTools mcp__*`, Gemini still injects `PLAIN_AGENT_DEFINITION` (`tools: []`) and fails on any tool event, Grok still disables all tools/MCP, and Codex still forces `--sandbox read-only` and `web_search="disabled"`.
- **Fix:** Either honor `ToolPolicy::ProviderDefault` on each provider (explicitly relaxing those flags only when the authorized request asks for `ProviderDefault`) or reject it cleanly with a clear `TOOLS_UNSUPPORTED` capability/error until implemented — never silently downgrade it to `ToolPolicy::None`.

#### 7. Claude inline `Turn` probe missed PR `#20`'s `reported_sign_in` fix `[New]`
- **Where:** [`claude::Turn::next`](../providers/src/claude/mod.rs#L1156-L1172)
- **What happens:** `StatusCheck` captures stdout and falls back to `reported_sign_in(output)` when `claude auth status` exits slowly or with an ambiguous exit code, but `Turn`'s `Stage::Probing` completely discards `Event::Stdout(_)` — so a slow-exiting unauthenticated Claude probe times out and wastes time spawning a full turn just to fail authentication.
- **Fix:** Share the same bounded stdout capture + `reported_sign_in(output)` check between `StatusCheck` and `Turn::Stage::Probing`.

#### 8. Other verified edge-case bugs `[New]`
- **Grounded Wikipedia/parenthesized URLs broken in Codex:** [`codex_message_sources`](../seatline-core/src/search.rs#L164) locates the end of `[title](url)` using `.find(')')` (first closing paren) and [`search.rs:183-188`](../seatline-core/src/search.rs#L183-L188) strips trailing `')'` unconditionally. Any URL containing balanced parentheses — e.g., `https://en.wikipedia.org/wiki/Rust_(programming_language)` — is truncated to `..._(programming_language` and breaks both the link and grounding accuracy. Track parenthesis nesting depth instead of `.find(')')`.
- **Chars vs bytes limit bug in Gemini model validation:** [`gemini/mod.rs:714`](../providers/src/gemini/mod.rs#L714) checks `label.chars().count() <= MAX_MODEL_LABEL_BYTES` (allowing up to 4× the byte limit for multi-byte UTF-8). Use `label.len() <= MAX_MODEL_LABEL_BYTES`.
- **Global `Reaper::stop_all` on single `Runtime` drop:** [`service::Runtime::drop`](../service/src/lib.rs#L373) calls `Reaper::shared().stop_all(...)`, which kills background-exiting processes belonging to *other* `Runtime` instances in the same process. Scope reaping to processes owned by that `Runtime` (or pass an owner token to `Reaper`).
- **Panic recovery bypasses `MAX_FINISHED`:** [`Scheduler::take_all_after_panic`](../scheduler/src/lib.rs#L452) pushes directly into `finished` without calling `self.keep()`, bypassing the `MAX_FINISHED` bound.
- **Grok spawns unbounded raw cleanup threads & runs startup sweep synchronously:** [`grok/mod.rs`](../providers/src/grok/mod.rs#L280-L282), [`ProbeWorkspace::drop`](../providers/src/grok/mod.rs#L695), [`TurnWorkspace::drop`](../providers/src/grok/mod.rs#L748) call `forget::remove_in_background` (which spawns an unbounded OS thread per drop in [`platform/src/forget.rs:156`](../platform/src/forget.rs#L156)) instead of routing through the bounded `seatline_core::work::Worker::cleanup()` used by Gemini.
- **Missing `XDG_*_HOME` vars in Unix inherited environment:** [`platform/src/environment.rs:20-47`](../platform/src/environment.rs#L20-L47) omits `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, and `XDG_STATE_HOME` from `INHERITED`, causing provider CLIs installed or authenticated under custom XDG locations on Linux to appear unauthenticated/unconfigured.

---

### 3.3 Security & File-Permission Hardening

#### 9. TOCTOU world-readable window in `private_fs::write_private_file` `[New]`
- **Where:** [`private_fs::write_private_file`](../platform/src/private_fs.rs#L23-L31)
- **What happens:** Calls `fs::write(path, contents)?` — creating files containing sensitive user prompts, page context, or cleanup markers under the process's default `umask` (typically `0o644`, world-readable on shared Linux/macOS machines) — and *then* calls `fs::set_permissions(0o600)`.
- **Fix:** On Unix, create/open with `OpenOptions::new().write(true).create(true).truncate(true).mode(0o600)` (via `std::os::unix::fs::OpenOptionsExt`) before writing bytes, and still `set_permissions(0o600)` on existing files in case an earlier file had wider permissions. (Also consider atomic temp-file + rename as [`config::write_private`](../companion/src/config.rs#L186) does for grants.)

#### 10. `install::register` tightens permissions on Chrome's shared `NativeMessagingHosts` directory `[New]`
- **Where:** [`install::register`](../companion/src/install.rs#L75) → [`config::write_private`](../companion/src/config.rs#L186)
- **What happens:** Commit `89aa95b` carefully fixed `autostart.rs` so it does not `chmod 0o700` shared OS directories like `~/.config/systemd/user` or `~/Library/LaunchAgents`. However, `install::register` still calls `config::write_private` on Chrome's shared `NativeMessagingHosts` directory, which calls `create_private_dir` → `set_permissions(0o700)` on Chrome's own directory.
- **Fix:** Use a non-mutating registration writer (just as `autostart::write_registration` does) for `com.seatline.host.json`, since the native messaging host manifest contains no secrets.

#### 11. `revoke` leaves orphaned `web/<app>.json` credential files on disk `[New]`
- **Where:** [`main.rs`](../companion/src/main.rs#L204-L212)
- **What happens:** `seatline-companion revoke <app>` deletes `apps/<app>.json` but never removes `web/<app>.json`, leaving behind stored pairing keys and relay tokens on disk indefinitely.
- **Fix:** Remove `web/<app>.json` (ignoring `NotFound`) inside `revoke` while holding the lock.

---

## 4. Recommended Execution Roadmap

Aligned with Seatline's slice-based and measurement-first workflow:

### Phase 1 — Quick P0 Hotfixes (1–2 small PRs, immediate payoff)
1. Throttle Grok `workspace.touch()` to 60s ([`grok/mod.rs`](../providers/src/grok/mod.rs#L680-L734)).
2. Fix `private_fs::write_private_file` Unix creation mode `0o600` ([`private_fs.rs`](../platform/src/private_fs.rs#L23-L31)), `install::register` shared-dir permissions ([`install.rs`](../companion/src/install.rs#L75)), and `revoke` cleanup of `web/<app>.json` ([`main.rs`](../companion/src/main.rs#L204-L212)).
3. Detach cold-started broker in [`startup::start_companion`](../companion/src/startup.rs#L104-L129) (`.process_group(0)` / `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`).
4. Replace blocking `std::thread::sleep` in [`links::change`](../companion/src/links.rs#L109-L124) when called from async `Service::keep()`, and replace `try_send` disconnects in [`connection_loop`](../companion/src/main.rs#L541-L553) with bounded `send` timeouts.
5. Add `[profile.release]` (`lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`) to root [Cargo.toml](../Cargo.toml) and fix small correctness bugs (Codex balanced-paren URLs in [`search.rs`](../seatline-core/src/search.rs#L164), Gemini `chars().count()` vs `len()`, Claude `Stage::Probing` stdout capture, Unix `XDG_*_HOME` inheritance).

### Phase 2 — Measured Latency & Resource Optimization Slice (validate with `seatline-bench`)
1. Preserve completion-boundary validation in [`CachedHit`](../providers/src/readiness.rs#L527-L585) while making `readiness_key()` cheap via `FileStamp → digest` memoization in [`Key::watch`](../providers/src/readiness.rs#L43-L79), and deduplicate Codex's config scan + `workspace::prepare_for` ancestor walk.
2. Offload Gemini `Init` `sync_all()` ([`gemini/mod.rs`](../providers/src/gemini/mod.rs#L956-L958)) and Grok background removals ([`grok/mod.rs`](../providers/src/grok/mod.rs#L695)) to `Worker::cleanup()`.
3. Release Claude `StatusCheck` to `Reaper` as soon as complete JSON prints ([`claude/mod.rs`](../providers/src/claude/mod.rs#L633-L670)); enable Grok token streaming (or drop `--include-partial-messages`).
4. Eliminate Hub prompt double-cloning & 8 MiB retention, cache `(FileStamp, Grant)` in `Hub` and push revocations from `links::Service` instead of per-link 1s disk polling, and precompute `secure::data_aad` prefixes.
5. Consolidate [`Reaper`](../seatline-core/src/process.rs#L641-L672) to one shared thread and introduce event-driven pipe wakeups to remove 1–10 ms polling jitter.

### Phase 3 — UX, Reliability Parity & Ecosystem Polish
1. Add a cross-platform `web` service control/stop mechanism so `install`, `autostart off`, and `revoke` cleanly replace/stop the web daemon on Windows and non-systemd Linux, and split the WebSocket read/write loop in [`web.rs`](../companion/src/web.rs#L360-L450) with broker handshake timeouts and consistent frame-size handling.
2. Add `status`, `apps`, `doctor`, and `version` subcommands, clean up `--help` (exit 0), fix stale `"pair"` messages, add additive `authorize` flags, and support Chromium/Brave/Edge/Arc in `install`.
3. Resolve `ToolPolicy::ProviderDefault` contract across providers, add typed readiness/prepare helpers to [`RemoteClient`](../companion/src/remote.rs#L217-L235), mark [`Update`](../seatline-core/src/exchange.rs#L31) `#[non_exhaustive]`, implement `Error`/`Display` on [`TurnError`](../seatline-core/src/turn.rs#L211), and add a GitHub Releases workflow for prebuilt binaries.
