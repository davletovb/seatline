# Application adoption (G-01 to G-04)

How Tabbeam, Lineleaf and Conclave use the performance changes, what was checked before each moved to the new Seatline revision, how to migrate, and how to fall back. The applications own their changes; nothing here is Seatline library behavior. The record of what is open is the [performance tracker](performance-implementation-tracker.md).

**Read this first.** Everything below was verified with fake or stand-in providers, on one Linux container, and by the applications' own tests. No live provider (Codex, Claude, Gemini or Grok) was available or run: **there is no live latency, quota, sign-in or cold-start result for any of the three applications, and none is claimed.** The counts of provider processes are real process launches of a stand-in; the milliseconds measure the application, its bridge, the broker and process starts, not a provider's own latency. The applications' branches are pushed, not merged, and no pull request is open, so their CI has not run (see [open items](#open-before-a-broad-rollout)).

## What each application does now

All three move from the pinned revision `dc1086582c8b98498aa48dae91c8d174bc3cfc3c` (the audited baseline) to [`0cb105e4c4d753abf8fb305d8ccedeeb64dd0ef4`](https://github.com/davletovb/seatline/commit/0cb105e4c4d753abf8fb305d8ccedeeb64dd0ef4), the merge of slice D, deliberately and with its own checks.

| Application | Branch and commits | What changed |
| --- | --- | --- |
| Tabbeam (G-02) | [`claude/perf-improvement-g02-shared-client`](https://github.com/davletovb/tabbeam/tree/claude/perf-improvement-g02-shared-client), [74ef4b7](https://github.com/davletovb/tabbeam/commit/74ef4b7) | One `RemoteClient` for the native host, shared by all four providers through `RemoteProvider::with_client`, instead of a thread, a runtime and a connection per exchange. |
| Lineleaf (G-01) | [`claude/perf-improvement-g01-retained-readiness`](https://github.com/davletovb/lineleaf/tree/claude/perf-improvement-g01-retained-readiness), [133e4bc](https://github.com/davletovb/lineleaf/commit/133e4bc), [eac2dec](https://github.com/davletovb/lineleaf/commit/eac2dec) | The native connection is kept between checks (closed after a minute idle, on pause and reset). Each check asks `readiness` (cached up to 30 s), enforces Lineleaf's own subscription and tool-isolation policy, and sends with `send_ready` and `check_sign_in: false`. `prepare` runs only for the configured provider on an enabled site's likely-check moments. Writing stays ephemeral per field. |
| Conclave (G-03) | [`claude/perf-improvement-g03-readiness-contract`](https://github.com/davletovb/conclave/tree/claude/perf-improvement-g03-readiness-contract), [f0a5853](https://github.com/davletovb/conclave/commit/f0a5853) | The same readiness contract in the browser app: `readiness`, then `send_ready`, with the app's cache bounded by the age of the evidence, a Retry that asks for a new check, and `prepare` for the providers a run is likely to use, from the prompt's focus and from adding a provider to the selection. Orchestration stays in Conclave. |

The pattern is the same in all three and is Seatline's contract ([readiness and preparation](readiness-and-preparation.md)): the application asks for readiness, applies its own account policy to the answer (Seatline does not restrict accounts), and sends under that readiness. If Seatline refuses before starting a turn because the evidence changed or lapsed (`READINESS_CHANGED`, `READINESS_EXPIRED`, `READINESS_UNVERIFIED`), nothing ran, so Lineleaf and Conclave repeat once from a fresh readiness; any other failure is final. A companion without the API answers the new methods as an unknown request (`INVALID_REQUEST`); Lineleaf and Conclave then fall back to `status` and `send` with `check_sign_in: true` and try the API again when the connection is re-established. Tabbeam's host does not use the readiness methods (its change is the shared client alone) and works with either companion.

## Evidence

Each application ran its own full suites before its pin or contract moved, and then its real-broker checks against a companion built from the new revision. What each check does and its limits are in the application's own documentation, linked in the last column.

| Application | Suites run here | Real-broker checks (stand-in provider) | Measured | Not run |
| --- | --- | --- | --- | --- |
| Tabbeam | `cargo test --workspace --locked` on stable and on Rust 1.85; Clippy `-D warnings` and format, with and without the shared-companion feature; the pin check script | The existing brokered-ask script still passes. A new script keeps one host open against a real broker and a fake `codex`: quick questions finish in about 100 ms beside a slow one that holds the second of the broker's two slots; one cancel ends one request; the host holds exactly one broker connection throughout (sampled from `/proc`); after re-authorizing and revoking the host carries on. The same script fails against a host that connects per request. | First text, fake `codex`, 8 rounds of 20 questions: 21.6 ms median first question, 21.5 ms later, against 22.0 and 21.7 ms with a connection per request: **no measurable difference**. Six requests in flight: 1 connection and 3 threads, against 4 and 6. | macOS and Windows packages, the extension jobs, the live Codex workflow |
| Lineleaf | 37 Python, 100 Node unit, 198 browser (Chromium) and 7 installed-extension tests; evaluation, packaging and typing tools. Every behavior added was mutation-checked. Run on Node 22.22 (the repository asks for Node 24) and the sandbox's Chromium build through `LINELEAF_CHROMIUM_PATH`, not the build its pinned Playwright installs. | The production controller through a real broker with a stand-in Codex that records its launches; the provider benchmark in two readiness modes with Seatline's fake Codex; authorization and coexistence at the new revision; the fallback against a real companion at `dc10865`. | Eight checks: before, 8 native host processes, 16 sign-in probes and 8 turns; now, 1 host, 1 probe and 8 turns; with a preparation first, the first check starts no host and runs no probe. Against `dc10865` the connection is still kept and each check costs the two probes it always did. | CI on Linux and macOS, the live provider benchmark |
| Conclave | Web 128 tests (17 new; every added behavior mutation-checked), server 129, typecheck, build, the web-bundle boundary check | The existing real-companion round trip (real `pair` helper, encrypted protocol 2, stand-in relay) now also shows over the encrypted path: one probe serves a fresh readiness, a cached readiness, a preparation and a checked send; an account-file change makes the next cached check a new one; a plain `send` with `check_sign_in: true` (what each step used to be) probes again inside its turn. With `--legacy` it checks that `dc10865` refuses the new methods as invalid requests. | A step inside the window now costs its turn and no probe, where it cost a probe each. The extra round trips and running-slot use of a readiness request are not timed. | CI, any live provider |

Details and reproduction commands: Tabbeam `docs/architecture/shared-companion.md` and `docs/performance/g02-shared-client.json`; Lineleaf [`docs/evidence/readiness-reuse-local.json`](https://github.com/davletovb/lineleaf/blob/claude/perf-improvement-g01-retained-readiness/docs/evidence/readiness-reuse-local.json) and `docs/architecture/seatline-integration.md`; Conclave `docs/shared-companion.md`.

### Seatline's own gates at the pinned revision

[CI run 37169150758](https://github.com/davletovb/seatline/actions/runs/37169150758) on `0cb105e` passed all six jobs: Rust on Linux, macOS and Windows (Clippy, tests, the client-only companion build and the companion build on each), the Linux release tests and the check that the client-only build links no network or crypto stack, the minimum-Rust job (`cargo +1.85 test --workspace --locked`), runtime independence, and the fuzz smoke. That is the validation of Linux, macOS, Windows, Rust 1.85 and the client-only companion build for the revision the applications pinned. It says nothing about the applications' own platform matrices (Tabbeam's macOS and Windows packages, Lineleaf's macOS job), which have not run.

### CI and stand-in success is not live validation

A stand-in provider answers at once, so it cannot show how long Codex takes to start, what a model costs, whether a real sign-in is classified as expected, how a real CLI behaves when its account changes, or how the 30-second readiness window fares against a real keyring. Seatline's cache is validated against fake CLIs' own invocation logs; the applications' counts are launches of a stand-in. Before any of this is called a latency improvement, the opt-in live runs must be made by someone with provider accounts:

- Seatline: `SEATLINE_BENCH_LIVE=1 seatline-bench run --live codex` (and the other providers), recording a live baseline; verify Claude's readiness key against a real install.
- Tabbeam: the manual `live-codex` workflow (it needs an `OPENAI_API_KEY` secret).
- Lineleaf: `python3 -m tools.benchmark_provider --companion … --provider codex --model MODEL_ID --samples 5 --readiness cached`, and again with `--readiness legacy`, on an authenticated subscription (the existing A-03 procedure and its cold/warm metadata requirements apply).
- Conclave: no live workflow exists; its integration job is a stand-in.

Persistent provider processes (slice E) were not touched: the decision about Codex's app-server is open, and nothing here enables a persistent mode by default.

## Migration notes

**Order.** Update the companion first, then the applications. A new application works against an old companion through its fallback; an old application works against a new companion unchanged (Lineleaf's earlier controller and Tabbeam's per-exchange host were both run against the new companion in the measurements above). Users keep their grants: the authorization command is unchanged, and authorizing again is only needed if an application reports that Seatline is unavailable after an update.

**Pins.** Update an application's Seatline pin deliberately, in the same change as the code the new revision requires, with the application's suites and its real-broker checks passing at the new revision. The one source change slice C forced on Tabbeam is `ProviderState.readiness` (an `Option`, absent on legacy status). Keep every Seatline crate and the companion a CI installs on the same exact revision (Tabbeam's pin check does this); record the oldest companion an application still supports (Lineleaf's `config/seatline-contract.json` has `minimum_revision`; Conclave's CI runs the older revision in `--legacy` mode).

**Adopting the readiness contract in another client.** Use `readiness` (or `prepare`) with a freshness of `{"mode":"fresh"}` or `{"mode":"cached","max_age_ms":N}` (30 000 at most), apply your own account policy to `sign_in`, and send with `send_ready` and `check_sign_in: false`; treat an unknown method (`INVALID_REQUEST`) as an older companion only until the API has answered once on that connection; forget that when the connection is re-established. Do not let a cache of your own add to Seatline's: bound it by the age Seatline reports for its evidence.

**Adopting the shared Rust client.** One `RemoteClient` per application, `RemoteProvider::with_client` for each provider. An application whose tests close a host's input right after a request must hold the input open until the answer arrives: the shared connection answers a moment later (Tabbeam's `cli.rs` needed this).

## Fallback instructions

- **Tabbeam:** in `native/host/src/providers/mod.rs`, make `installed_provider` build `RemoteProvider::new(APP, &metadata)` and drop `Link`. Nothing else depends on the shared connection, and the new pin works with either.
- **Lineleaf:** there is no setting. A companion without the readiness API is handled automatically; the retained connection closes after a minute idle, on pause and on reset; and the previous extension build is the way back to a connection per check.
- **Conclave:** the same automatic fallback for an older companion; reverting the application commit restores the per-step `status` and `send`.
- **A companion update that misbehaves:** every application still works with `dc1086582c8b98498aa48dae91c8d174bc3cfc3c`, so reinstalling it through the companion's normal procedure is a way back; the grants are unaffected.
- **Seatline:** nothing in this adoption changes the library; there is no persistent-provider mode to turn off.

## What was found along the way

- **Re-authorizing or revoking an app while its host holds a connection.** The broker closes the connections of the old grant on its next one-second sweep. A request sent in that second fails once with `COMPANION_DISCONNECTED` (retryable); the next one reconnects with the new grant, or is refused with `APP_NOT_AUTHORIZED` after a revoke. The earlier connection-per-request arrangement had no such window. Closing the old connections at authorization time instead of at the sweep would remove it; that is a possible follow-up in Seatline, not done here.
- **The broker runs two turns per app and per provider at a time and queues eight.** Two slow turns hold both slots, and quick ones wait behind them in order; this is policy, not the client.
- **Hosts and apps keep their own latency floors.** Tabbeam's host polls its requests every 10 ms, which is why its fake-provider first text is about 21 ms with either client; that is not Seatline's to change, and it is why the shared client's gain there is structural (one connection and one thread for any number of requests in flight), not a faster first text.
- **A tool that counted `send` frames broke silently.** Lineleaf's typing probe counted `send` frames and would have failed in CI once turns went out as `send_ready`; it now reads the turns the companion received by either method. Any tool that classifies requests by method should be checked the same way when it moves to the readiness methods.

## Open before a broad rollout

- Open the application pull requests so their CI runs: Tabbeam's macOS and Windows packages and the pin job, Lineleaf's Linux and macOS jobs and the two-revision companion job, Conclave's two-revision round trip. None has run.
- The live runs above, by someone with provider accounts.
- Review of the three branches by their owners; they were written and verified by one author in one session.
- Slice E and F are untouched and decide whether a persistent mode and the shared hub changes ever become the default.
