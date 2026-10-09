# I-04: Claude live baseline, 2026-10-09

The first live-provider baseline for Seatline. It covers **Claude only**: it is the one provider CLI installed and signed in where it was run. **Codex, Gemini and Grok have no live baseline yet**; they need a machine and an account of the owner's, and [the runbook](../performance-measurement.md#recording-a-live-baseline) gives the commands.

It answers where the time goes between a request and a one-word answer on this machine, and whether Claude launch isolation (I-06) changes it. These are the numbers **before** tracker item I-05, which later ended the half second Claude spends exiting for turns that keep no session: see the [I-05 summary](2026-10-09-claude-live-i05-summary.md). It does not say how long Claude answers on anyone's machine: see [what it may claim](#what-it-may-and-may-not-claim) at the end.

## What was measured

| | |
| --- | --- |
| When | 2026-10-09, 12:53 to 13:02 UTC. Three runs back to back, in this order: **run 1** (launch isolation off), **isolated** (on), **run 2** (off again). The second run without it is the noise. |
| Code | Release builds of the harness and the companion at `ad026ce` plus the slice's uncommitted working tree (I-04 and I-06), which the commit that adds this file records. Rust 1.97.0. |
| Machine | A 4-CPU Linux x86-64 container in a cloud sandbox. Nothing else was built or tested while the runs went. |
| CLI | Claude Code 2.1.295, signed in with an OAuth token the sandbox supplies (`authMethod: oauth_token`, first-party API). **The plan is not known**: the CLI does not report one. |
| Model | None is named by the adapter, so the CLI's default answered: `claude-opus-5-5`, as the `init` message of an adapter-launched turn says. Effort: the CLI's default. |
| Home | The container's own. `settings.json` is empty, there is no MCP server and no managed settings file. An adapter-launched turn's `init` lists 54 slash commands, 5 agents, 20 skills, 3 plugins and 0 tools; with `--safe-mode` it lists 53, 4, 19, 3 and 0. Safe mode dropped one skill, one agent and one command: **almost nothing was there to drop**. |
| Prompt | "Reply with the single word: ok", tools off. The answer is `ok`, 4 output tokens. |
| Sample | 10 measured requests and 1 warm-up per scenario and run, 56 prompts a run (168 in all). With ten, the 95th percentile **is** the slowest request, so this document quotes p50 and the slowest, never a p95. |

```sh
export SEATLINE_BENCH_LIVE=1
S="--scenario cold-broker --scenario warm-send --scenario warm-send-probe --scenario prepared-send --scenario warm-status --scenario resumed-context"
target/release/seatline-bench run --live claude --samples 10 --warmup 1 $S \
    --label "claude live, launch isolation off (run 1)" --output claude-live-run1.json
echo '{"claude_isolation": true}' > isolated.json
target/release/seatline-bench run --live claude --samples 10 --warmup 1 $S --policy isolated.json \
    --label "claude live, launch isolation on" --output claude-live-isolated.json
target/release/seatline-bench run --live claude --samples 10 --warmup 1 $S \
    --label "claude live, launch isolation off (run 2)" --output claude-live-run2.json
```

Raw evidence, with every request: [run 1](2026-10-09-claude-live-run1.json), [isolated](2026-10-09-claude-live-isolated.json), [run 2](2026-10-09-claude-live-run2.json). Comparisons made with `seatline-bench compare`: [run 1 against run 2](2026-10-09-claude-live-repeat-comparison.md) (the noise) and [run 1 against isolated](2026-10-09-claude-live-isolation-comparison.md). The [readiness-key check](#the-readiness-evidence-survives-a-claude-run) has its own [telemetry](2026-10-09-claude-live-readiness-key-telemetry.jsonl). All 180 measured requests (150 prompts and 30 status checks) completed, and no scenario failed.

## Where the answer time goes

Milliseconds, **p50 of run 1 / run 2** (launch isolation off). The phases are the broker's: `init` runs from launching Claude until it announces itself, `text wait` from then to the first visible text, `completion` from the first text to the terminal update, and `tail`, which lies inside `completion`, from Claude's final result to that update.

| Scenario | submit→text | submit→done | `init` | `text wait` | `completion` | of which `tail` |
| --- | --- | --- | --- | --- | --- | --- |
| `cold-broker` | 1912 / 1996 | 2387 / 2538 | 615 / 607 | 1296 / 1324 | 502 / 517 | 476 / 485 |
| `warm-send` | 1855 / 1867 | 2382 / 2402 | 574 / 618 | 1284 / 1239 | 476 / 516 | 449 / 495 |
| `warm-send-probe` | 2110 / 2208 | 2644 / 2757 | 568 / 621 | 1286 / 1311 | 545 / 534 | 523 / 510 |
| `prepared-send` | 1909 / 1915 | 2501 / 2458 | 631 / 661 | 1299 / 1260 | 530 / 531 | 499 / 495 |
| `resumed-context` | 1942 / 1913 | 2459 / 2511 | 587 / 635 | 1360 / 1284 | 501 / 550 | 469 / 523 |

The slowest of ten `submit→text` in run 1 / run 2 (the figure a p95 would be, with so few) was 3.2 / 4.7 s in `cold-broker`, 2.6 / 3.3 in `warm-send`, 3.1 / 5.8 in `warm-send-probe`, 2.1 / 8.9 in `prepared-send` and 2.0 / 3.4 in `resumed-context`. The slow requests are slow in a provider phase: in the eight slowest of the 150 prompts, six waited seconds for the first text (8.3 s at worst), one spent 2.1 s in `init`, and one 2.0 s in `tail`; seven of the eight were in run 2, when the network was evidently slower. In none of the 150 did Seatline's own phases pass 45 µs (queue wait) or 22 µs (cleanup).

**Of a warm send's roughly 2.4 s** (the share of the mean request, three runs):

| Part | Share | Whose |
| --- | --- | --- |
| `init`: Claude starting | 24 to 25 % (about 0.57 to 0.62 s) | The CLI's. A status command, which starts it and sends no prompt, takes about 0.24 s on its own. |
| `text wait`: the first text | 54 to 55 % (about 1.24 to 1.28 s) | The network and the model. |
| `completion`: first text to the end | 20 to 21 % (about 0.45 to 0.52 s) | Waiting for Claude's process to leave: `tail` alone is 19 to 20 % of the request. |
| Seatline's own | about 0.1 % | Submit to launch about 2 ms (p50 1.9 to 2.0), queue wait 11 to 12 µs, cleanup 4 to 5 µs, the broker's handshake 0.19 ms, the client's connect and handshake 0.4 ms. |

- **After its answer is complete, Claude takes about half a second to leave, and Seatline waits for it.** `tail` has a p50 of 0.43 to 0.52 s in every scenario of every run (0.50 s over all 150 prompts), and it is steady: 0.38 to 0.80 s in 149 of the 150, 2.0 s in one. That is a fifth of what an application waits for a one-word answer. It is the CLI's own exit time, not something Seatline adds: with the adapter's arguments and no Seatline in the way, five direct runs took 328, 453, 462, 510 and 538 ms from the final `result` line to the process exit (a sixth, with the CLI's debug log on, 656 ms). In that log the first 0.13 s after the turn ended was a session-end step of the CLI's own telemetry plugin, which posted a batch of its usage events (0.11 s); the rest of the wait left no line. This is the evidence tracker item I-05 was waiting on.
- **A sign-in probe costs 0.21 to 0.24 s**, because `claude auth status` is a CLI start of its own: `warm-send-probe`'s `probe` phase has a p50 of 211 / 227 ms and `warm-status` 239 / 244 ms. A checked send pays it before every launch, which is why its `submit→text` is 255 / 341 ms above a plain send's.
- **A prepared send pays none of it.** In `prepared-send` the measured sends ran 0 probes in 10 requests in each of the three runs, and their `submit→text` (1909 / 1915) is within the noise of a plain send's (1855 / 1867) and 201 / 293 ms below a checked one's. The probe moved to the `prepare` before each send, which an application makes while the user is still typing; an application that does not prepare early has only moved the cost, not removed it.
- **A new broker costs a few milliseconds.** `cold-broker`'s `prepare` (connect, starting the broker, handshake) has a p50 of 3.4 ms, against 0.4 ms warm. Its `submit→text` is 57 / 129 ms above a warm send's, of the order of the 7 to 98 ms by which the two identical runs differ within a scenario, and its phases show nothing that belongs to a cold start (`init` 615 / 607 against 574 / 618).
- **Resuming a one-turn conversation costs nothing measurable** (`resumed-context` 1942 / 1913 against `warm-send` 1855 / 1867).
- **The prompt cache is hit.** In `cold-broker`, `warm-send`, `warm-send-probe` and `prepared-send`, every measured request of run 1 and run 2 reported 2,660 input tokens with 2,658 read from the cache and none written; the prefix of a tools-off turn is small and stable, so there is little to shorten. A resumed conversation grows by about 40 tokens a turn: in run 1 each turn read 2,138 from the cache and wrote 600 to 960; in run 2 it wrote none, because run 1 had written the same conversation minutes before. A cache an earlier run warmed flatters the later one.

## Does launch isolation (I-06) help here?

**Not measurably, on this home.** The isolated run really was isolated, as these show: the broker's limits record `claude_isolation: 1` and the report records the policy; `--safe-mode` was on the command line in all 386 `ps` samples of a Claude turn taken during that run, and in none of the 337 taken during run 2; and the `init` message lists 53 commands, 4 agents and 19 skills in place of 54, 5 and 20. Its turns sent a shorter prompt: 1,976 input tokens (1,974 read from the cache) in place of 2,660. Authentication worked in safe mode: 50 of 50 measured prompts completed.

| Pooled over the five scenarios that launch Claude (p50, ms) | Off, run 1 | Off, run 2 | Off, both | On | On − off, both | Run 2 − run 1 (the noise) |
| --- | --- | --- | --- | --- | --- | --- |
| `init` | 599 | 635 | 618 | 588 | −30 | +36 |
| `submit→text` | 1944 | 1988 | 1954 | 1918 | −37 | +44 |
| `submit→done` | 2474 | 2582 | 2514 | 2434 | −80 | +108 |
| `tail` | 489 | 507 | 496 | 495 | −1 | +18 |

Each difference between the isolated run and the runs without isolation is smaller than the difference between the two runs without it, which is the yardstick the [runbook](../performance-measurement.md#recording-a-live-baseline) sets: nothing here is evidence of an effect, and at most a few tens of milliseconds of `init` may have gone. That fits what the home holds: one skill, one agent and one command dropped, and no hooks to run. **It says nothing about a machine whose home loads hooks, plugins or a large `CLAUDE.md`.** An earlier observation with two synthetic one-second hooks (5.7 s against 2.8 s, one run each) shows what can be there to save, and only a run like this one on such a machine, made with `--policy`, can price it. The setting stays off by default.

## The readiness evidence survives a Claude run

The tracker asked for Claude's readiness key to be verified against a real install. `prepared-send` makes a `prepare` before each send, so every `prepare` after the first comes after a Claude process has run. Two runs of four sends each (the [telemetry](2026-10-09-claude-live-readiness-key-telemetry.jsonl) of the second is kept): the first `prepare` ran one sign-in probe (269 and 323 ms), and **every later one ran none and took 1.5 to 1.9 ms** (six of six). During one such turn, `~/.claude.json` and files directly inside `~/.claude` were rewritten (so the directory's own modification time moved), and the evidence still held. That is what holds here, where credentials come from an OAuth token and there is no `~/.claude/.credentials.json`, one of the files the key stamps; an install that has one, and an API-key or cloud sign-in, are not covered.

## What it may and may not claim

- **May:** on this sandbox, with this account, in this hour, a one-word Claude answer took about 2.4 s from request to end through Seatline, of which about 0.6 s was Claude starting, 1.3 s the first text, 0.5 s the wait for the process after the answer, and about 2 ms Seatline. A sign-in probe added 0.2 s to a send that asked for one, and a `prepare` took it out of the send.
- **May:** that Seatline's own time is not where a Claude answer is slow, and that the wait after the answer is the largest piece of it that Seatline can act on (I-05).
- **May not:** be quoted as how fast Claude, or Opus 5.5, answers. The account and plan are unknown, the network is a sandbox's, the machine is one 4-CPU container, and the prompt is one word.
- **May not:** say how long a long answer takes. The reply is four tokens, so the time that grows with an answer's length is not in it, and the fixed costs around the answer are most of what was measured.
- **May not:** say anything of a machine with hooks, plugins or a long `CLAUDE.md`, of other models or efforts (I-03's `--effort` levels were not timed), of tools or search turns, or of several requests at once: one request ran at a time.
- **May not:** rank providers. There is one.
- **May not:** claim a cold machine. Each sample's broker and Claude process are new, but the files they read were already in the page cache.
- **No p95:** ten samples make the 95th percentile the slowest request.

## Not measured

Codex, Gemini and Grok (not installed here); the plan class; a machine with a real setup; `warm-send-adapter` and `warm-send-shared` (they add 0.1 to 0.3 ms to a plain send, which seconds bury); concurrency; macOS and Windows; other hours of the day. Rerun on the owner's machine, for every provider it has, with [the runbook](../performance-measurement.md#recording-a-live-baseline).
