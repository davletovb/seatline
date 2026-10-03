# Phase telemetry

Optional, privacy-safe timing of what happens to a request between the moment the broker receives it and the moment its provider process is gone. It exists so that performance work can say *where* time goes before claiming a speedup (tracker item B-01); the [measurement method](performance-measurement.md) uses it, and so can any host that runs the scheduler.

It is **off unless asked for**. A broker that is not asked keeps no timelines, takes no extra clock reads and writes nothing.

## Turning it on

| Where | How |
| --- | --- |
| The broker (`seatline-companion serve`) | Start it with `SEATLINE_TELEMETRY_FILE=<path>`. It appends one JSON line per record to that file. On Unix the file is created readable only by its owner, and one that already exists is tightened to that (or, if it is not the user's to change, or is not a regular file, telemetry stays off with a message on standard error). The variable is read once, at start: a broker that is already running must be restarted (it exits by itself when idle, see `SEATLINE_BROKER_IDLE_SECS`). If the file cannot be opened the broker says so on standard error and runs with telemetry off; it never refuses to serve over it. |
| A host that runs the hub itself | `seatline_companion::hub::start_with(root, Some(sink))` with any `seatline_core::telemetry::Sink`. `seatline_core::telemetry::Memory` keeps records in memory. |
| A host that runs the scheduler itself | Start a turn with `Supervisor::start_timed(.., Timeline::new(kind, received))` and take the finished timeline with `Supervisor::take_timeline(id)` once the turn has ended. The scheduler keeps at most 4,096 untaken timelines and drops the oldest past that. |

The broker's file is bounded and never blocks a request: records go through a 1,024-entry queue to a writer thread, a full queue drops the record and counts it, and the file stops growing at 16 MiB. The count of lost records is written as a `dropped` record before the next record that is kept.

## Phases and their boundaries

A timeline holds **marks**, monotonic instants at which a boundary was observed. A **phase** is the span between two marks. The phases of a request tile it: they add up, to the microsecond, to `total_us`, with nothing counted twice and nothing left over. A phase that did not happen, or that the adapter cannot observe, is **absent** from the record; absence never means "instant".

| Phase | From | To | Notes |
| --- | --- | --- | --- |
| *(handshake)* | the broker reads the authentication frame | `ready` is on the wire | One `connection` record per connection, not part of a request's phases. Local IPC only. |
| `queue_wait` | `received` | `admitted` | In the hub's queue, waiting for a slot. |
| `sign_in_probe` | probe start | probe end | Inside the next window. Only when an adapter ran one (below). |
| `provider_init` | `admitted` | `started`, less `sign_in_probe` | Finding the executable, preparing the workspace, starting the process, and the provider's own start-up until it accepts the turn. |
| `first_text` | `started` | `first_text` | Waiting for the first answer text. Includes the model and the network. |
| `completion` | `first_text` | `terminal` | The rest of the answer, until the terminal update. |
| `cleanup` | `terminal` | `released` | Dropping the exchange: killing and reaping the provider process and joining its reader threads. |

### The marks

All times in a record are microseconds since `received`. A mark is set once; a later repeat of the same boundary changes nothing.

| Mark | Taken when | Where in the code |
| --- | --- | --- |
| `received` | The hub dequeues the request command. Time 0. | `Hub::command` |
| `admitted` | The request leaves the queue and the adapter is asked for its exchange. | `Hub::admit` |
| `built` | The adapter returned its exchange. Between `admitted` and this the hub's thread was busy; for a send it holds the executable lookup, the workspace check and the first process spawn. | `Telemetry::hand_off` |
| `probe_started` | Just before the adapter starts its sign-in probe, so the probe's own spawn is inside it. | Codex and Claude `send` |
| `probe_ended` | The probe exited, was killed at its limit, or the request ended while it ran. | Codex and Claude `Turn::start` / `Turn::end` |
| `launched` | `Update::Launched`: the provider process for the turn was started. | Scheduler |
| `started` | `Update::Started`: the provider accepted the turn. | Scheduler |
| `status` | The first `Update::Status` of a `status` request. | Scheduler |
| `first_text` | The first non-empty `Update::Delta`. | Scheduler |
| `stop_requested` | A cancel, a timeout or a shutdown was asked for. | Scheduler (`begin_stop`) |
| `terminal` | The terminal update was observed, or, for an exchange that never produced one, the host gave up on it. | Scheduler |
| `released` | The exchange was dropped. | Scheduler |

A mark is taken when **the host's thread observes** the boundary, not when the provider produced it. The hub wakes every 5 ms when idle, so a mark can trail the event by that much; this is the time an application actually waits, and it means differences smaller than the polling interval cannot be resolved. Marks are `Instant`s: no wall-clock time is stored, so a clock adjustment cannot make a duration negative.

### Phases for early endings

The span up to the terminal update belongs to the phase the request was in; later phases are absent.

| The request… | …so these phases are present |
| --- | --- |
| was refused or cancelled in the queue | `queue_wait` only, ending at the refusal or cancel |
| failed during `provider_init` (executable missing, signed out) | `queue_wait`, `sign_in_probe` if one ran, `provider_init`, `cleanup` |
| completed with no answer text | up to `first_text`, which absorbs the wait; `completion` is absent |
| is a `status` request | `queue_wait`, `sign_in_probe` (the whole check), `completion`, `cleanup` |
| is a `forget` or `cleanup` request | `queue_wait`, `cleanup` (the work itself) |

### Cancellation and other endings

`outcome` is one of five values; `detail` is always a static name, never provider output.

| Outcome | `detail` | When |
| --- | --- | --- |
| `completed` | none | The provider finished. |
| `failed` | the failure reason, such as `LOGIN_REQUIRED` or `QUEUE_FULL` | The provider or the runtime failed it, or it was refused before it ran. |
| `cancelled` | none | A `cancel` request, a closed connection or a revoked grant, while queued or running; or a hub shutdown. `stop_requested` says when the stop was asked for and `terminal` when it took effect. A stop requested before the terminal update is observed ends as `cancelled` even if the provider had already finished: the scheduler suppresses the late update. |
| `timed_out` | `start`, `idle` or `absolute` | A limit ended it. `stop_requested` is when the limit was found to be exceeded. |
| `aborted` | `adapter_panicked`, `scheduler_panicked` or `stopped_unexpectedly` | A bug, or a process that stopped on its own. |

The outcome is how the **scheduler** ended the turn, with one exception: when the hub ends a request itself by sending its client a terminal update, and then stops the exchange (a session-ledger failure sends `failed` with `SESSION_LIMIT_REACHED` or `SESSION_STORE_FAILED`), the record carries what the client was told. The scheduler's own verdict there can be `cancelled`, or even `completed` if the exchange had finished before the hub acted, and neither is what the client saw.

## Unsupported and missing phases

| Phase or surface | Status |
| --- | --- |
| `sign_in_probe` on `send` | **Codex and Claude only.** Gemini and Grok run no sign-in probe on the send path (their turns fail with an authentication error instead), so the phase is absent for them, as it is for any send that does not ask for `check_sign_in`. |
| `sign_in_probe` on `status` | Every provider: the status check is the probe. `probes` counts a status request as one check even when the provider was not found and nothing was spawned, so it counts readiness checks, not processes. |
| `launched` and `provider_init` | Gemini and Grok start their process while the exchange is built, so `launched` is observed right after `built` and the whole synchronous start is inside `provider_init`. Codex and Claude launch after any probe. |
| `first_text` granularity | Codex reports each agent message whole, so its first text is the first complete message. Claude reports text as it streams; Gemini and Grok report it as their adapters do. |
| `cleanup` | Only the time to drop the exchange. Per-turn file cleanup an adapter does *before* its terminal update, such as Gemini deleting its transcripts, is inside `completion`, not `cleanup` (slice F-02 moves it). Grok removes its per-turn workspace on a background thread, which is in neither. |
| `queue_wait` | Starts when the hub dequeues the request. The socket read, the hop into the hub's inbox and any time the hub's thread was busy before it dequeued (a ledger write, an exchange being built) are not in it: the client-observed time in the benchmark harness bounds that gap. |
| The handshake | Broker side of local IPC only. The client's own connect time, handshakes over the hosted web transport and refused handshakes are not recorded. |
| Refused before a request exists | A request for a provider the app's grant does not allow is answered at once and leaves no record. |
| The in-process service (`seatline-service`) | Not instrumented: it has no queue and no handshake. A host can still time its turns with `start_timed`. |
| Reused provider processes | There are none yet, so there is no phase for one (tracker E-02). |

## Record format

One JSON object per line. `schema` is `1`; adding a field does not change it, removing or redefining one does. A reader must ignore fields it does not know. Absent marks and phases are left out, not written as `null` or `0`.

```json
{"kind":"broker","schema":1,"version":"0.1.0-dev","protocol":1,"os":"linux","arch":"x86_64",
 "limits":{"max_running":8,"max_app_running":2,"max_provider_running":2,"max_queue":64,"...":0}}

{"kind":"connection","schema":1,"connection":3,"app":"app-a","handshake_us":290}

{"kind":"request","schema":1,"connection":3,"request":"req-17","app":"app-a","provider":"codex",
 "method":"send","outcome":"completed","text":true,"probes":1,"launches":1,
 "marks_us":{"admitted":13,"built":140,"probe_started":120,"probe_ended":5200,"launched":5300,
             "started":9100,"first_text":9300,"terminal":9600,"released":9650},
 "phases_us":{"queue_wait":13,"sign_in_probe":5080,"provider_init":3807,"first_text":200,
              "completion":300,"cleanup":50},
 "total_us":9650}

{"kind":"dropped","schema":1,"count":4}
```

`phases_us` can be recomputed from `marks_us` and `method` alone; the library does exactly that (`Marks::phases`). `probes` is the number of readiness checks the request ran and `launches` the number of provider processes it started for its turn; together they say how many processes a request cost without parsing provider logs.

### Correlation

A request is identified by `(connection, request)`: the broker's own number for the connection, which restarts with the broker, and the request ID the application chose. A `connection` record carries the same `connection` number, so a request's handshake can be joined to it. The benchmark harness matches its own samples to records by request ID.

## What is never recorded

Prompts, answer text, tokens and credentials, account names, file system paths, provider output and error text, and native session handles never enter a timeline or a record. A record holds only:

- the **app name** and **request ID**, which the local administrator and the application chose: an application that turns telemetry on should not put private data in request IDs;
- the static **provider** and **method** names (a method the broker does not serve is recorded as `unknown`, never as what the client sent), and **failure reasons** from the fixed list in `companion/src/wire.rs`;
- numbers: durations, counts and the broker's limits.

Three tests pin this: a hub test sends a request whose prompt, answer and native session handle are all recognizable and asserts that none appears in the record; another sends a made-up method and asserts it is not echoed; and an IPC test asserts the app's credential is not in the file.

## Cost

**Disabled** (the default): a turn carries `timeline: None`. The scheduler's per-update cost is one `Option` check; no clock is read and nothing is allocated. In the hub, each telemetry call starts with `if !enabled() { return }` and builds no lookup key. The one thing that is not conditional: Codex and Claude read the clock twice around a sign-in probe they run, because the adapter cannot know whether anyone is listening; that is two reads against a process spawn.

**Enabled:** an update that sets a mark that is not yet set reads the clock (`Instant::now`) once; every other update only asks the timeline whether it wants it. Each request allocates its record's strings once, and records leave through the bounded queue.

Measured on the scheduler alone (scripted turns, no process, socket or provider), so the figure is the scheduler's own:

| Scheduler, per update, release build | p50 |
| --- | --- |
| Phase timing **off** (the default) | 105–133 ns |
| Phase timing **on** | 117–148 ns: **+11 to +15 ns** per update |
| `main`'s scheduler before this slice, same benchmark | 116–139 ns |

The first two rows are three consecutive `seatline-bench overhead` runs ([raw](performance-baselines/2026-10-03-scheduler-overhead.json)): 200 scripted turns of 200 updates, alternating off and on, the first round discarded. The machine's speed drifted between invocations (hence the ranges); within each invocation timing on is 11–15 ns above timing off. The third row is the same burst benchmark built against `main`'s scheduler (commit `93c58ab`) and against this one, five interleaved runs each, with timing off in both: `main` 116–139 ns, this branch 103–130 ns. **With timing off the scheduler is indistinguishable from `main`** within that spread. That comparison was a one-off built in a scratch crate, not part of the harness: copy `overhead::run`'s untimed variant against a checkout of `main` to repeat it.

Reading the clock only for an update that would set a mark, rather than for every update, is what keeps timing on at +15 ns instead of the +41 ns the first version cost: a flood of progress, or every delta after the first, reads no clock.

Re-run it with `seatline-bench overhead` (release build). It measures the scheduler's hot loop, not the broker; the hub's own part (a map insert and a few string copies per request) is not separately measured, and is small next to the process spawns a request costs.

## Verification

- `seatline-core`: marks are set once; phases tile every shape of request, checked on injected instants with no clock; the record has exactly the documented fields.
- `seatline-scheduler`: boundaries arrive in order for completion, cancel, timeout, adapter panic and scheduler panic; a turn started without a timeline keeps nothing; an adapter whose `probe_span` panics still ends its turn normally, and untaken timelines are bounded.
- `seatline-tests/tests/probe_span.rs`: Codex and Claude report their probe, ordered before `launched`; a signed-out probe ends the span and the request before any launch; cancelling mid-probe ends the span; Gemini and Grok report none. Counts are checked against the fake CLI's own invocation log.
- `seatline-companion`: one record per request across send, status, cleanup, refusal, queue cancel, connection close and a hub-ended ledger failure (recorded as the client saw it); telemetry off keeps nothing; an existing world-readable telemetry file is made private; the real broker writes its configuration, a handshake and a request record, and writes nothing unless asked.

No test asserts a wall-clock threshold: orderings are compared, durations are compared only with each other, and exact values come from injected instants.
