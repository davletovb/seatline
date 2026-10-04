# F-01 through F-04: hub before/after measurements

Measured on 2026-10-04 in the same Linux x86-64 container, using release builds with Rust 1.99.0 (`b940084d7`, 2026-09-28), telemetry disabled, and the same deterministic fake Codex executable. The before build uses main revision `57737e660480a8838e84c4ef461de404dcac248d`; only the new `hub_overhead.rs` measurement driver and its `seatline-bench` command dispatch were copied into that checkout. Its production code is unchanged. The after build includes F-01 through F-04.

Raw reports: [before](2026-10-04-f-hub-before.json), [after](2026-10-04-f-hub-after.json). This is one before/after pair, not a statistical estimate across machines or runs.

## Results

The idle windows each last three seconds. CPU is summed across the benchmark child's threads using `/proc/self/task/*/schedstat`; voluntary context switches come from each thread's `status`. Initialization and connection opening are outside these windows. These are process-wide counters, including the measurement thread's sleep, rather than a count of hub timer callbacks.

| Idle state | Before CPU (ms) | After CPU (ms) | Before voluntary switches | After voluntary switches |
| --- | ---: | ---: | ---: | ---: |
| No connections | 10.270 | 0.159 | 591 | 1 |
| Three authenticated app connections | 10.858 | 0.789 | 591 | 4 |

The connected idle window uses 92.7% less CPU and 99.3% fewer voluntary switches in this run. Active polling starts at 1 ms after commands/progress and backs off to 5 ms over four quiet ticks.

| Workload | Before (ms) | After (ms) |
| --- | ---: | ---: |
| Uncontended submit to first text, p50 | 5.487 | 1.475 |
| Uncontended submit to first text, p95 | 5.669 | 1.596 |
| Uncontended submit to completion, p50 | 10.580 | 2.558 |
| Uncontended submit to completion, p95 | 10.761 | 2.687 |
| Fresh readiness completion with two long generations running | 310.894 | 3.620 |
| Interactive first text behind six queued long generations | 1240.209 | 302.211 |
| Interactive completion in that backlog | 1245.301 | 303.714 |
| All eight long generations in that backlog complete | 1245.349 | 1213.798 |

Uncontended percentiles cover 60 sequential requests rotating among three apps, after three excluded warm-up requests. Each contention row is a single deterministic workload, not a percentile. The fake long generation waits 300 ms; a readiness probe runs its own fake subprocess. The interactive request arrives after two long requests have started and six more have queued. All eight long requests still complete. Both reports count 74 generation processes and one readiness probe; there is no process reuse.

Generation priority waits for a slot already occupied by a long request; it does not preempt or exceed the provider limit. The backlog result demonstrates admission at the next available slot instead of after the queued long work. It is not evidence for the original socket harness's `short-contended` p95 queue-wait budget or a universal 35 ms bound. The original [socket scenario budgets](../performance-measurement.md#budgets) still require their own percentile comparison.

## Reproduce

On Linux, build all benchmark binaries and the companion together:

```sh
cargo build --release --locked -p seatline-bench -p seatline-companion
target/release/seatline-bench hub --idle-ms 3000 --samples 60 \
  --label "F-01 through F-04" --output hub-after.json
```

For the before build, create a separate checkout at the revision above, copy only `seatline-bench/src/hub_overhead.rs` and the driver dispatch changes in `seatline-bench/src/main.rs` into it, and build with a separate target directory using the same compiler and release profile. Run the same command from that checkout with a different label/output. The driver creates isolated grants, home/configuration and fake executable copies, removes inherited Codex configuration overrides from its child environment, and deletes its temporary data after collecting process counts. It neither connects to a real provider nor sends a real-provider prompt.

The direct command inbox and real fake-provider subprocesses exercise the production hub, admission, authorization and exchange polling paths. IPC framing, transport startup, network/model latency, live accounts, persistent execution and application integrations are excluded. This evidence supports F's idle-wait and scheduling changes; injected storage/cleanup regression tests cover F-01/F-02 separately. Full socket tests and the existing OS/MSRV/release/fuzz gates remain required.
