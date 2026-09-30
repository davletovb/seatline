# Shared Seatline companion

Seatline remains a provider-neutral runtime. The companion exposes the same
provider contract to multiple independently authorized apps. It contains no
Conclave orchestration, product storage, website hosting, or JavaScript runtime.

The per-user broker uses a private Unix socket on Linux/macOS and a Windows
named pipe. There is no local HTTP listener. App grants control providers, tool
policy and exact extension/website origins. Sessions, cancellation and cleanup
are scoped to the authenticated app; all apps share bounded scheduling.

Build with `cargo build --workspace --locked -p seatline-companion`. Install the
binary once, then approve apps with `authorize APP codex,claude,gemini,grok`.
Chrome uses `com.seatline.host`; the registry selects the app by its exact
extension origin. Product-specific adapters remain in their own repositories.
`register-native` is an optional compatibility adapter for a product's existing
native protocol. Apps using the neutral protocol need no native adapter.

Protocol 1 uses four-byte little-endian length-prefixed UTF-8 JSON, max 1 MiB.
Authenticate with `{version:1,app,token}`; receive `{type:"ready",version:1}`.
Requests carry `id`, `provider`, `method`, `params`; responses carry `id`, `event`.
Methods: `status`, `send` (neutral Turn), `forget` (`sessions`), `cleanup`
(`group`), `cancel` (`target`, confined to the same connection). Random broker
session tokens replace raw provider handles and cannot cross app/provider scope.

The limits are 8 running operations, 2 per app and 2 per provider, 64 queued
requests (8 per app), 32 connections, and 15 minutes per turn. Output is bounded;
slow/disconnected clients are cancelled. Revoking a grant cancels its connected
requests within one second. A process running as the same user can read local
app credentials; app isolation does not sandbox malicious local code.

Provider adapters still launch one process per turn. This change centralizes
installation and execution; it does not change warming or provider sign-in.
