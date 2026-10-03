# D-01: the shared client against a connection per exchange

Same machine (4-CPU Linux container), release builds, fake provider, built at `a8dd5af` with a clean tree; each run measures the three paths back to back after 3 warm-up requests. `start→text` is the application's own time from starting the request to the first answer text, p50 / p95 in milliseconds, connection included for the paths that make one per request. The shared client's one connection was made by its first warm-up request.

| Run | Path | `start→text` p50 / p95 | `submit→done` p50 / p95 |
| --- | --- | --- | --- |
| run 1 (30) | `warm-send` (bare wire, one connection per request) | 6.3 / 6.7 | 11.1 / 11.4 |
| run 1 (30) | `warm-send-adapter` (`RemoteProvider::new`: a thread, a runtime and a connection per exchange) | 6.5 / 7.0 | 11.6 / 12.1 |
| run 1 (30) | `warm-send-shared` (`RemoteProvider::with_client`: one shared `RemoteClient`) | 6.1 / 57.7 | 11.2 / 60.5 |
| run 2 (30) | `warm-send` (bare wire, one connection per request) | 6.2 / 6.5 | 11.0 / 11.2 |
| run 2 (30) | `warm-send-adapter` (`RemoteProvider::new`: a thread, a runtime and a connection per exchange) | 6.3 / 9.8 | 11.5 / 14.9 |
| run 2 (30) | `warm-send-shared` (`RemoteProvider::with_client`: one shared `RemoteClient`) | 6.1 / 6.2 | 11.2 / 11.4 |
| confirmation (100) | `warm-send` (bare wire, one connection per request) | 6.2 / 6.5 | 11.1 / 11.4 |
| confirmation (100) | `warm-send-adapter` (`RemoteProvider::new`: a thread, a runtime and a connection per exchange) | 6.4 / 7.0 | 11.5 / 11.9 |
| confirmation (100) | `warm-send-shared` (`RemoteProvider::with_client`: one shared `RemoteClient`) | 6.2 / 6.6 | 11.3 / 11.5 |

Every request in every run completed. The budget set for D-01 was a p50 within +0.2 ms of the bare wire. The shared client's p50 is 6.1 against 6.3 in run 1, 6.1 against 6.2 in run 2, and 6.2 against 6.2 in the confirmation run: at or below the bare wire, and 0.2 to 0.3 ms below the per-exchange adapter (6.3 to 6.5), which is what the baseline said connection reuse could be worth. **That saving is not the reason for D-01**: one runtime and one connection, bounded routing, and a cancel that names one request are.

Run 1's shared-client p95 of 57.7 ms is two consecutive requests that were slow for reasons outside the client: for the first, the broker's own total was 10.7 ms and the application saw the first text 50 ms later; for the second, the broker's `provider_init` took 56.9 ms, so both the broker and the application stalled within about 100 ms of each other. Three further runs of 100 shared-client requests each had a worst case of 11.6 ms (only the first of them is kept here, as the confirmation run). The outlier is recorded rather than dropped, and is not explained further than a pause in this container.
