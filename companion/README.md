# Shared Seatline companion

Seatline remains a provider-neutral runtime. The companion exposes the same
provider contract to multiple independently authorized apps. It contains no
Conclave orchestration, product storage, website hosting, or JavaScript runtime.

The per-user broker uses a private Unix socket on Linux/macOS and a Windows
named pipe. There is no local HTTP listener. App grants control providers, tool
policy and exact extension/website origins. Sessions, cancellation and cleanup
are scoped to the authenticated app; all apps share bounded scheduling.

Build with `cargo build --release --locked -p seatline-companion`, then run the
resulting executable with `install` once. CI attaches native binaries for review;
signed installers and automatic updates are not included in this change. Users
still install and sign in to their chosen provider CLIs separately.

```sh
seatline-companion install
seatline-companion authorize my_app codex,claude https://app.example.com --relay=https://relay.example.com
seatline-companion pair my_app https://relay.example.com https://app.example.com --open
```

`pair` opens the hosted app with a private link and keeps an outbound encrypted
connection active. It never opens a local HTTP port. Keep that process running
while using the website. Pairing authorization is scoped to the exact approved
website and relay origins. Treat pairing links as secrets; do not share them.

Extensions use the same installation:

```sh
seatline-companion authorize my_extension codex chrome-extension://abcdefghijklmnopabcdefghijklmnop/
```

Chrome uses `com.seatline.host`; the registry selects the app by its exact
extension origin. Product-specific adapters remain in their own repositories.
`register-native` is an optional compatibility adapter for a product's existing
native protocol. Apps using the neutral protocol need no native adapter.
`revoke APP` removes authorization and closes active requests. Reauthorizing an
app rotates its credentials and resets any compatibility adapter registration.

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
