# Seatline

Seatline is a reusable Rust runtime for running subscription-authenticated AI provider CLIs safely and consistently.

It provides:

- `seatline-core`: process, stream, discovery, turn, prompt, search, and neutral runtime types;
- `seatline-platform`: environment allowlists, private workspaces/files, cleanup, and layout;
- `seatline-providers`: adapters for Codex, Claude, Gemini through Antigravity, and Grok;
- `seatline-scheduler`: bounded concurrent turn scheduling and panic isolation;
- `seatline-service`: a threaded in-process service API, with a bounded queue per turn for slow consumers;
- `seatline-companion`: one shared native installation with app-scoped IPC (including a reusable Rust client, `remote::RemoteClient`, with one connection per app) and encrypted outbound web transport;
- `seatline-fake-provider`, `seatline-tests`, and `seatline-fuzz`: deterministic test and fuzz infrastructure;
- `seatline-bench`: a reproducible benchmark harness for request overhead (fake providers by default, live providers opt-in).

Applications own conversation/product policy. Seatline owns provider execution mechanics.

See the [shared companion setup and protocol](companion/README.md) for using one
installation from multiple apps and extensions. The companion has no local HTTP
listener and includes no JavaScript runtime or product orchestration.

## Status

Seatline is pre-1.0 and its Rust source API may change between pinned revisions. It is currently consumed by Git revision rather than crates.io releases.

## Requirements

- Rust 1.85 or newer

## Development

Startup, latency, and shared-companion work is tracked in the
[performance implementation plan](docs/performance-implementation-tracker.md).

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The fuzz target is a separate workspace:

```bash
cargo +nightly fuzz run --fuzz-dir seatline-fuzz stream_lines
```

Real-provider smoke tests live under `seatline-tests/tests/live_*.rs` and are opt-in.

## Measuring performance

Request overhead is measured, not guessed. The broker can record privacy-safe
[phase timings](docs/telemetry.md) for each request when asked
(`SEATLINE_TELEMETRY_FILE`), and `seatline-bench` runs named scenarios against
the real broker and joins what an application saw with what the broker recorded:

```bash
cargo build --release --locked -p seatline-companion -p seatline-bench
target/release/seatline-bench run --output after.json
target/release/seatline-bench compare before.json after.json
```

[Measuring performance](docs/performance-measurement.md) gives the method, what
a measurement may and may not be used to claim, the baseline, and the budgets it
sets. Live-provider runs are opt-in and send real prompts.

## Provenance

This repository was extracted from the provider-runtime boundary in [TabBeam](https://github.com/davletovb/TabBeam), source commit `b4bfd5bd0f3ca9db461963b971b8af0177754e5d`.

The extracted runtime files were transferred as identical Git blobs, so their blob SHAs match the source snapshot. The GitHub connector used for the extraction did not expose a history-filter/push operation equivalent to `git filter-repo`; the original pre-extraction history therefore remains in TabBeam and this repository records the exact source commit instead of fabricating rewritten author/date history.

## License

Licensed under either of:

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT License (`LICENSE-MIT`)

at your option.


## Consuming Seatline

Until the 0.x API settles, applications should pin the Seatline Git repository to an exact commit revision and update that pin deliberately after their own integration tests pass.

Readiness caching and prompt-free preparation are available through the explicit provider and companion APIs; see the [readiness and preparation contract](docs/readiness-and-preparation.md).
