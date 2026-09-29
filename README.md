# Seatline

Seatline is a reusable Rust runtime for running subscription-authenticated AI provider CLIs safely and consistently.

It provides:

- `seatline-core`: process, stream, discovery, turn, prompt, search, and neutral runtime types;
- `seatline-platform`: environment allowlists, private workspaces/files, cleanup, and layout;
- `seatline-providers`: adapters for Codex, Claude, Gemini through Antigravity, and Grok;
- `seatline-scheduler`: bounded concurrent turn scheduling and panic isolation;
- `seatline-service`: a threaded in-process service API;
- `seatline-fake-provider`, `seatline-tests`, and `seatline-fuzz`: deterministic test and fuzz infrastructure.

Applications own conversation/product policy. Seatline owns provider execution mechanics.

## Status

Seatline is pre-1.0 and its Rust source API may change between pinned revisions. It is currently consumed by Git revision rather than crates.io releases.

## Requirements

- Rust 1.85 or newer

## Development

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

## Provenance

This repository was extracted from the provider-runtime boundary in [TabBeam](https://github.com/davletovb/TabBeam), source commit `b4bfd5bd0f3ca9db461963b971b8af0177754e5d`.

The extracted runtime files were transferred as identical Git blobs, so their blob SHAs match the source snapshot. The GitHub connector used for the extraction did not expose a history-filter/push operation equivalent to `git filter-repo`; the original pre-extraction history therefore remains in TabBeam and this repository records the exact source commit instead of fabricating rewritten author/date history.

## License

Licensed under either of:

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT License (`LICENSE-MIT`)

at your option.
