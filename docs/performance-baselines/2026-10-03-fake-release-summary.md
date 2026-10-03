fake provider, release build, run 1 — mode `fake`, linux x86_64, 4 CPUs, harness release, companion release builds, revision 071ab6d662ca

30 measured requests per application after 3 warm-up; p50 / p95 in milliseconds.

### `cold-broker` — cold_broker_fresh_provider

No broker is running. The application's client starts one, connects, and sends one request; the provider is a fresh process. Each sample uses a new broker and data directory. Preparation includes starting the broker.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | single | 30 / 0 | 102.2 / 102.4 | 6.1 / 6.3 | 108.4 / 108.7 | 11.3 / 11.5 | 0.0 / 0.0 | 5.7 / 5.9 | 0.0 / 0.0 | 5.2 / 5.2 | 0.0 / 0.0 |

Requests 33; broker counted 0 sign-in probes and 30 provider launches for the measured ones; the fake provider saw 0 probe and 33 turn processes in all.

### `warm-send` — warm_broker_fresh_provider

A broker is already running. Each request is made on a new connection, as the shipped client does, and runs a fresh provider process. No sign-in probe.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | single | 30 / 0 | 0.3 / 0.4 | 5.9 / 6.1 | 6.2 / 6.5 | 11.0 / 11.2 | 0.0 / 0.0 | 5.5 / 5.6 | 0.0 / 0.0 | 5.2 / 5.2 | 0.0 / 0.0 |

Requests 33; broker counted 0 sign-in probes and 30 provider launches for the measured ones; the fake provider saw 0 probe and 33 turn processes in all.

### `warm-send-adapter` — warm_broker_fresh_provider

As `warm-send`, but through the shipped `RemoteProvider` the way an application's adapter calls it, which starts a thread, a runtime and a connection for every exchange. Only what shows from outside is timed, from the call that starts the exchange, so connecting and the handshake are not reported apart.

| app | role | ok / failed | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | single | 30 / 0 | 6.3 / 6.6 | 6.3 / 6.6 | 11.5 / 11.7 | 0.0 / 0.0 | 5.5 / 5.6 | 0.0 / 0.0 | 5.2 / 5.2 | 0.0 / 0.0 |

Requests 33; broker counted 0 sign-in probes and 30 provider launches for the measured ones; the fake provider saw 0 probe and 33 turn processes in all.

### `warm-send-probe` — warm_broker_fresh_provider

As `warm-send`, but each request asks the adapter to check the sign-in first, which runs the provider's own status command before the turn.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | probe | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | single | 30 / 0 | 0.3 / 0.4 | 16.6 / 17.2 | 16.9 / 17.7 | 21.7 / 22.2 | 0.0 / 0.0 | 10.6 / 10.7 | 5.6 / 5.7 | 0.0 / 0.0 | 5.2 / 5.2 | 0.0 / 0.0 |

Requests 33; broker counted 30 sign-in probes and 30 provider launches for the measured ones; the fake provider saw 33 probe and 33 turn processes in all.

### `warm-status` — warm_broker_fresh_provider

A running broker answering `status` requests: the readiness check on its own.

| app | role | ok / failed | prepare | submit→done | queue wait | probe | completion | cleanup |
|---|---|---|---|---|---|---|---|---|
| bench-a | single | 30 / 0 | 0.3 / 0.4 | 10.9 / 11.2 | 0.0 / 0.0 | 10.6 / 10.8 | 0.0 / 0.0 | 0.0 / 0.0 |

Requests 33; broker counted 30 sign-in probes and 0 provider launches for the measured ones; the fake provider saw 33 probe and 0 turn processes in all.

### `resumed-context` — resumed_context

A running broker. Each request continues the provider-side conversation the previous one started, in a fresh provider process. Resuming a conversation is not reusing a process.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | single | 30 / 0 | 0.3 / 0.4 | 5.9 / 6.0 | 6.2 / 6.4 | 11.0 / 11.1 | 0.0 / 0.0 | 5.5 / 5.6 | 0.0 / 0.0 | 5.2 / 5.2 | 0.0 / 0.0 |

Requests 34; broker counted 0 sign-in probes and 30 provider launches for the measured ones; the fake provider saw 0 probe and 34 turn processes in all.

### `reused-process` — reused_process

One provider process serving several requests.

**unsupported**: No provider process is reused across requests: every `send` starts a fresh one, and a persistent-provider adapter does not exist yet (tracker item E-02). There is nothing to measure, so nothing is reported; resumed context (`resumed-context`) is a different state.

### `short-isolated-paced` — warm_broker_fresh_provider

One application makes short requests, paced, with the broker to itself. The reference for `short-contended`.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | short | 30 / 0 | 0.5 / 0.6 | 5.9 / 6.2 | 6.4 / 6.8 | 11.1 / 11.5 | 0.0 / 0.0 | 5.5 / 5.7 | 0.0 / 0.0 | 5.2 / 5.3 | 0.0 / 0.0 |

Requests 33; broker counted 0 sign-in probes and 30 provider launches for the measured ones; the fake provider saw 0 probe and 33 turn processes in all.

### `three-app-short` — warm_broker_fresh_provider

Three applications, as three processes, start together and each makes short requests back to back.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | short | 30 / 0 | 0.4 / 1.7 | 7.4 / 23.2 | 9.0 / 23.7 | 11.5 / 23.2 | 0.1 / 10.8 | 5.6 / 12.0 | 0.0 / 0.0 | 5.2 / 5.6 | 0.0 / 0.0 |
| bench-b | short | 30 / 0 | 0.4 / 1.7 | 6.4 / 19.4 | 7.6 / 19.6 | 11.3 / 22.7 | 0.0 / 10.8 | 5.8 / 10.7 | 0.0 / 0.0 | 5.2 / 5.3 | 0.0 / 0.0 |
| bench-c | short | 30 / 0 | 0.4 / 1.7 | 11.4 / 18.0 | 12.8 / 18.5 | 12.4 / 23.2 | 5.3 / 10.9 | 5.6 / 6.4 | 0.0 / 0.0 | 5.2 / 10.5 | 0.0 / 0.0 |

Requests 99; broker counted 0 sign-in probes and 90 provider launches for the measured ones; the fake provider saw 0 probe and 99 turn processes in all.

### `short-contended` — warm_broker_fresh_provider

As `short-isolated-paced`, while two other applications keep the provider's slots busy with long requests (about 300 ms each) for as long as the short one is measured.

| app | role | ok / failed | prepare | submit→text | start→text | submit→done | queue wait | init | text wait | completion | cleanup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bench-a | short | 30 / 0 | 0.5 / 0.6 | 105.4 / 125.9 | 106.1 / 126.5 | 110.6 / 131.1 | 99.0 / 119.5 | 6.0 / 6.3 | 0.0 / 0.0 | 5.2 / 5.3 | 0.0 / 0.0 |
| bench-b | long | 16 / 0 | 0.4 / 0.5 | 16.4 / 16.7 | 16.8 / 17.2 | 320.1 / 322.8 | 10.4 / 10.6 | 5.6 / 5.9 | 0.0 / 0.0 | 303.8 / 306.2 | 0.0 / 0.0 |
| bench-c | long | 16 / 0 | 0.4 / 0.7 | 16.3 / 16.6 | 16.7 / 17.2 | 320.2 / 322.9 | 10.4 / 10.6 | 5.6 / 5.8 | 0.0 / 0.0 | 303.8 / 306.6 | 0.0 / 0.0 |

Requests 65; broker counted 0 sign-in probes and 62 provider launches for the measured ones; the fake provider saw 0 probe and 65 turn processes in all.

### Limitations

- Broker marks are taken when its hub thread observes an update, so they include the hub's polling interval (5 ms when idle) and cannot resolve differences smaller than that; client timings include the socket and the application's own scheduling.
- Only compare results from the same machine, build profile and mode; a shared or virtualized machine adds noise of its own. `compare` warns when these differ.
- `prepare` is connecting and the handshake. There is no provider-side preparation to measure yet: readiness is not cached (C-02) and there is no prepare API (C-03).
- Resumed context continues a conversation in a fresh provider process; it is not process reuse, which is unsupported until a persistent-provider adapter exists (E-02).
- A scenario's first request after warm-up is still the first of its kind in this run's broker; a genuinely cold machine (empty page cache, first start after installation) is not reproduced.
- These runs use a fake provider: they measure what Seatline adds (broker, scheduling, process start, IPC) and contain no network or model latency, and a real provider's own start-up time is not in them. They say nothing about how soon a real answer begins.
