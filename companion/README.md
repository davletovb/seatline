# Shared Seatline companion

Install Seatline once per OS user. Apps have independent grants, sessions and
workspaces; provider execution and admission limits are shared. There is no
local HTTP listener. Linux/macOS use a private Unix socket, Windows a named
pipe. Chrome connects to `com.seatline.host` and passes its exact extension
origin. The bridge selects a locally registered app worker, never a remote
executable. Direct clients use protocol 1 over IPC or `connect APP` over stdio.

The distribution contains the broker, a bundled Node runtime, TabBeam's native
worker, Conclave's stdio engine, and the website relay helper. Product engines
retain their own conversation state. App-specific workers are installed inside
this distribution, not as separate companion applications.

## Development

Build `cargo build --workspace --locked`. Authorize each app locally:

```sh
seatline-companion authorize tabbeam codex,claude,gemini,grok chrome-extension://YOUR_EXTENSION_ID/ --cache-title=TabBeam
seatline-companion register-worker tabbeam native /absolute/path/tabbeam-host
seatline-companion authorize conclave codex,claude,gemini,grok https://YOUR_CONCLAVE_ORIGIN
seatline-companion register-worker conclave rpc /absolute/path/node /absolute/path/conclave/apps/server/dist/companion.js
seatline-companion manifest
```

Save `manifest` output as `com.seatline.host.json` in Chrome's per-user native
host directory, or register its absolute path under Chrome's HKCU native host
key on Windows. App credentials remain in private local files and never enter
extension messages. `revoke APP` cancels that app's connected turns within one
second. A local process with access to the user's account can read those files;
the broker isolates app protocol requests, not malicious code running as the user.

Run `node relay/helper.mjs conclave https://YOUR_RELAY https://YOUR_CONCLAVE_ORIGIN`
and open the private pairing link it prints. The packaged launcher does this
with its bundled Node runtime. Pairing lasts 24 hours, is scoped to the exact
website origin, and uses separate credentials for website and helper. The
browser removes credentials from the URL and keeps them in tab session storage.
The encryption key is generated locally and is never sent to the relay.

## Cloudflare

Edit `relay/wrangler.jsonc` to set the real Conclave origin, then deploy this
Worker and Durable Object using your Cloudflare account. Deploy Conclave's
`apps/web/dist` as a static Cloudflare Pages site, built with
`VITE_SEATLINE_RELAY=https://YOUR_RELAY`. The website reconnects to the app engine;
its existing run event cursor handles replay. Lost acknowledgements for mutating
requests are surfaced, never automatically resubmitted. Model runs continue when
the website disconnects, while losing the local worker interrupts the run.

The relay sees connection metadata, encrypted frame lengths and timing. Prompts,
results and app API messages are AES-GCM encrypted with direction-bound sequence
numbers. Provider credentials stay in the provider's supported CLI. Existing
one-process-per-turn adapters are unchanged; this does not add provider warming.

## Protocol and limits

Frames are a four-byte little-endian length followed by UTF-8 JSON, up to 1 MiB.
Authenticate with `{version:1,app,token}`; receive `{type:"ready",version:1}`.
Requests have `id`, `provider`, `method` and `params`. Methods are `status`, `send`
(the neutral Seatline `Turn`), `forget` (`sessions`), `cleanup` (`group`) and
`cancel` (`target`, confined to the same connection). Responses carry `id` and
`event`. Persistent handles are random broker identifiers mapped to one app and
provider; raw provider session identifiers never leave the broker.

Admission allows 8 concurrent operations, 2 per app, 2 per provider, and at most
64 queued requests (8 per app), with 32 authenticated connections. Turns are
bounded to 15 minutes. Output queues and frames are bounded; a slow or vanished
client is disconnected and its exchanges are cancelled. Cancellation never
releases a concurrency slot before the provider has actually stopped.

Release distribution still needs publisher signing/notarization, a configured
Cloudflare origin and relay, and a published extension ID. The build workflow
produces a reviewable portable installer bundle; it does not publish or deploy.
