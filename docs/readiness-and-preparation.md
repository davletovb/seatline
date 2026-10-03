# Readiness and preparation

C-01 through C-04 define when a readiness result may be reused, provide prompt-free preparation, and remove repeated executable and native-session scans. Applications choose when to prepare and what authentication/billing modes to accept.

## Freshness and the provider contract

`Provider::status()` remains a fresh check. `Provider::readiness(Freshness)` and `Provider::prepare(Freshness)` allow a caller to request:

| Policy | Behavior |
| --- | --- |
| `Freshness::Fresh` | Start a new check, even if verified evidence or another check exists. Refresh executable discovery too. |
| `Freshness::Cached { max_age_ms }` | Reuse verified evidence no older than the requested duration, capped at 30 seconds. Concurrent misses for the same effective configuration share a check. Zero means fresh. |

Local applications enable caching by retaining one `seatline_providers::readiness::Ready` wrapper around an adapter. The companion installs one wrapper per authorized app/provider. `RemoteProvider` exposes the same methods through authenticated IPC; it uses the existing per-exchange connection/runtime mechanism. Persistent IPC/runtime reuse is a separate D-01 change.

Explicit readiness status contains `readiness: { source, age_ms }`. `source` is `fresh`, `cached`, or `shared`; `shared` means another caller started the check. Age uses a monotonic clock, begins when the status result was observed, and is rounded up to milliseconds. Holding an exchange does not renew its evidence. A delayed cache hit is revalidated when consumed; a configuration change or expired result before generation fails safely without launching a turn. Legacy status without this field still decodes.

Only **available and authenticated** results from a successfully completed check are cached. Authentication classification may still be `unknown`: the application must inspect `sign_in` separately when billing mode matters. Unauthenticated, unknown authentication, unavailable, missing executables, timeouts, cancelled checks, and failures are never cached. `prepare` reports status and then completion even for a missing or signed-out provider: completion means the check ended, not that the provider is ready. The explicit send API refuses these unverified states.

| Provider | Fresh readiness operation | Authentication/classification | Legacy `send` with `check_sign_in = true` |
| --- | --- | --- | --- |
| Codex | `codex login status` | Exit 0 authenticated, 1 unauthenticated, other exit/timeout unknown. A bounded output prefix recognizes the known ChatGPT or API-key status phrases; otherwise classification is unknown. | Fresh inline check; no `Status` update. Unknown/probe failure permits the turn to diagnose its own failure. |
| Claude | `claude auth status` | Same exit mapping. Bounded JSON `authMethod`: `claude.ai` subscription, `api_key`/`api_key_helper` API key, `third_party` cloud, otherwise unknown. Older CLI output remains supported. | Fresh inline check; no `Status` update. Unknown/probe failure permits the turn to diagnose its own failure. |
| Gemini (Antigravity) | `agy models` | Successful catalog check authenticated/cloud. Recognized authentication error unauthenticated. Other errors or timeout unavailable/unknown. | No inline check; generation reports authentication failures. |
| Grok | `grok models` in an isolated probe workspace | Known OAuth login text authenticated/cloud. Known signed-out or disallowed key text unauthenticated. Unrecognized output unknown. Other failures or timeout unavailable/unknown. | No inline check; generation reports authentication failures. |

Missing executables are `not_found/unknown`; an executable that cannot be launched or an invalid workspace is `unavailable/unknown`. Probe output and credential contents are never included in errors, status, or telemetry. The classification is a fixed enum; no email, account name, token or key prefix is exposed.

The exit/auth-mode contract is grounded in the [official Codex CLI reference](https://developers.openai.com/codex/cli/reference/) and [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), checked 2026-10-03. Gemini and Grok retain their existing adapter-specific model-list probes and deterministic fixtures; no stronger undocumented provider guarantee is assumed.

## Preparation and sending

Preparation resolves the executable, validates/creates the private workspace, and checks readiness/catalog support. It runs **no synthetic model prompt**, starts no generation process, allocates no conversation/session handle, and keeps no provider process warm. All four shipped adapters support this level of preparation. Custom adapters default to `PREPARATION_UNSUPPORTED` unless they opt in. The underlying provider initialization for a model turn still occurs on send; reused initialization belongs to E.

```rust
use seatline_core::readiness::Freshness;
use seatline_providers::{Provider, readiness::Ready};

// Keep this wrapper for one app/account/workspace/environment configuration.
let provider = Ready::new(adapter);
let policy = Freshness::Cached { max_age_ms: 5_000 };
let preparation = provider.prepare(policy);
// Drive the Exchange, inspect its Status, and apply the app's account policy.

// An explicit checked send emits Status before Launched on every adapter.
// Set check_sign_in=false to permit verified reuse; true always requires Fresh.
let exchange = provider.send_with_readiness(turn, policy);
```

`send_with_readiness` performs readiness first, emits `Status`, and launches only when authentication and availability are verified. It suppresses the second inline Codex/Claude probe. The turn's `check_sign_in = true` overrides a cached policy with a new fresh check, preserving callers that require freshness. Ordinary `send` retains the legacy behavior in the table; a successful unchecked send does not manufacture cached readiness.

An application that restricts billing modes should use `prepare`/`readiness`, inspect `sign_in`, and enforce its policy before submitting. Seatline does not add product-specific account restrictions. A fresh status check is evidence of credentials at that moment; neither fresh nor cached status guarantees that a remote model request will succeed or that credentials will never change afterward.

The readiness exchange has a 30-second overall check limit; shipped CLI probe limits remain 10 seconds by default. Gemini/Grok expose `with_probe_timeout`, and Codex/Claude use their existing probe limits. Normal filesystem operations still run synchronously, as on the existing adapter path. The companion's queue and scheduler bounds also apply. Cancellation detaches one subscriber; another subscriber may finish the shared check. Cancelling/dropping the last subscriber stops/releases the check. There is one cached result and one weak reference to the joinable check per wrapper, with at most one status and one terminal event retained per check. Evidence expires on access; preparation retains no idle child requiring shutdown.

## Invalidation and isolation

The wrapper's immutable adapter instance defines provider, account-home/auth location, workspace, search configuration, and inherited environment. Rebuild it to change those settings. Each app's companion grant owns its own adapter and cache, even when two apps use the same provider executable. No readiness result or credential material is shared across app caches.

Before reuse, the adapter revalidates its workspace and fingerprints the executable and relevant credential/configuration files:

| Adapter | Watched account/configuration inputs |
| --- | --- |
| Codex | Effective `CODEX_HOME` or home `.codex`: `auth.json`, `config.toml`, additional `*.config.toml` files and the directory; current tool-isolation capabilities. |
| Claude | Effective config directory: `.credentials.json`, `settings.json`, `settings.local.json`, directory, and home `.claude.json`. |
| Gemini | Home `.gemini/oauth_creds.json`, `settings.json`, and Antigravity CLI `auth.json`/`config.json`. |
| Grok | Effective `GROK_AUTH_PATH`, `GROK_HOME/auth.json`, or home `.grok/auth.json`. |

File identity/canonical target, metadata and bounded content digests detect removal, replacement, account/config edits and creation of previously missing files. Digests are kept in memory and never serialized or debug-printed. Unreadable or oversized configuration disables caching. OS-keyring changes and server-side revocation may have no local file signal, so the 30-second ceiling still applies; request a fresh check after login/logout or an account change. `invalidate_readiness()` explicitly drops local cached evidence and executable discovery. Authentication failures from a subsequent turn also invalidate evidence. Failed/obsolete probes cannot repopulate an invalidated cache.

The hub compares the grant's workspace title as well as its token/provider/tool permissions, checks authorization again at queue admission, and drops the app's adapters/evidence when its grant changes or is revoked. Another app's cache and exchanges remain independent.

## Companion wire methods

Protocol version 1 gains additive methods. Update the companion to use them; an older broker refuses an unknown method. Existing `status` and `send` shapes remain accepted.

| Method | `params` |
| --- | --- |
| `readiness` | `{"mode":"fresh"}` or `{"mode":"cached","max_age_ms":5000}` |
| `prepare` | Same freshness object. |
| `send_ready` | `{"turn":<existing Turn object>,"freshness":<freshness object>}` |

All methods require the provider in the app's grant and use existing request IDs, limits, cancellation and terminal delivery. Malformed freshness is `INVALID_REQUEST`; unsupported preparation is `PREPARATION_UNSUPPORTED`. Configuration changes while checking or before launch are `READINESS_CHANGED`; an expired shared result is `READINESS_EXPIRED`; an unverified explicit send is `READINESS_UNVERIFIED`. None of those failures launches a model turn.

Fresh checks count as readiness probes in telemetry. Cache hits and shared subscribers count zero new probes; their local readiness work/wait belongs to `provider_init`, with the remaining completion/cleanup phases unchanged. The initiating explicit send reports the readiness span for all four adapters.

## Executable and session lookup

First-directory executable hits retain the original one-check lookup to avoid adding cache overhead. Later positive executable discovery is cached for at most five seconds and revalidates the chosen executable's identity, permissions and symlink target on every lookup. Misses are never cached, so installation/removal recovers immediately. Replacement invalidates the positive hit. A newly installed higher-priority executable appears within five seconds, or immediately after explicit invalidation/fresh readiness. A provider's search path is immutable; rebuild the adapter when changing search configuration.

The companion's session ledger now indexes `(app, provider, native session)` to broker token, and maintains per-app counts. Its on-disk JSON format is unchanged. Insertion/removal/replacement and rollback update both indexes; restart rebuilds them. Legacy duplicate native entries retain the old lexicographically first-token behavior. Existing global/app caps and app/provider checks still apply. This changes in-memory lookup; durable whole-ledger writes remain F-01.

## Reproducible local evidence

Run the release microbenchmarks separately from ordinary tests:

```sh
cargo +1.85.0 test -p seatline-core -p seatline-companion --lib --release --locked -- --ignored --nocapture
```

One Linux x86-64 container run, Rust 1.85, optimized build, 10,000 lookups per loop:

| Workload | Original scan | Cached/indexed | Mean per lookup, before → after |
| --- | --- | --- | --- |
| Executable in the last of 128 directories | 627.387 ms | 22.246 ms | 62.74 µs → 2.22 µs |
| Native session at the end of 10,000 ledger entries | 715.510 ms | 3.177 ms | 71.55 µs → 0.32 µs |

These are synthetic lookup microbenchmarks, not p50/p95 request latency or live-provider gains. The directory workload deliberately exercises a late discovery hit; a first-directory hit may gain little. Provider conformance tests cross-check actual fake CLI logs: concurrent preparation plus a repeated cached preparation runs one readiness command per provider and no generation; a cached explicit send adds no probe, while a fresh-required send adds exactly one.

Tests cover all four adapters' authenticated, unauthenticated, unknown, missing, unavailable and probe-timeout cases; sanitized classification; concurrent sharing; cancellation; expiry; changes during checks; grant/workspace scope; executable recovery; session restart/caps/rollback; and telemetry phase sums. Real IPC and platform checks use CI; live-provider latency and application adoption remain unverified until those opt-in runs are recorded.
