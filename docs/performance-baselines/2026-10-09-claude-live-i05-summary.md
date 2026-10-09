# I-05: ending a finished Claude turn at its result, 2026-10-09

The [first live baseline](2026-10-09-claude-live-summary.md) found that a Claude turn waited about half a second, a fifth of a one-word answer, for Claude's process to exit after its answer was complete. This is what that wait is, what was tried, what was done about it, and what it bought. It is **Claude only**, from the same cloud sandbox as the baseline, and its limits are that baseline's: an account of unknown plan, one machine, a one-word prompt, ten to twelve samples per scenario. Antigravity and Grok wait as before: their wait is unmeasured.

## What Claude does after its final result

The measurements ran the adapter's own command line (a plain turn, no tools, no session kept) with the environment Seatline gives a provider and in a process group of its own, as Seatline starts it. [`scripts/measure-claude-exit.py`](../../scripts/measure-claude-exit.py) does the same for your CLI; it has no kill mode, because of what a kill leaves. Milliseconds from the `result` line on Claude's stdout to its exit:

| What was done at the result | Runs | Result → exit | Exit | Left behind |
| --- | --- | --- | --- | --- |
| Nothing | 5 | 450, 505, 552, 626, 649 (median 552) | 0 | Nothing |
| SIGTERM to the group | 5 | 396, 434, 437, 516, 528 (median 437) | 0 | Nothing |
| SIGKILL to the group | 5 | 10, 25, 26, 28, 32 | killed | In all five: `~/.claude/sessions/<pid>.json`, its `.key` file and `/tmp/cc-socks/<pid>.sock` |
| Nothing, with `DISABLE_TELEMETRY=1` | 3 | 19, 20, 21 | 0 | Nothing |
| Nothing, with `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | 3 | 20, 22, 24 | 0 | Nothing |

A traced run (`strace` slows the CLI, so its times are longer) shows the order of the wrap-up: after the result line Claude removes and rewrites its own session file and removes its `.key` and its messaging socket, then opens **a new TLS connection through the proxy and sends one event-logging POST**, removes its session file again, and exits. With its debug log on, that step is the CLI's telemetry plugin posting a batch of its usage events (0.11 s of it was the POST, and the rest of the wait left no line). With telemetry switched off by either variable the CLI leaves in about 20 ms, which fits its skipping that step (it was timed, not traced).

So there are three ways to stop waiting, and two of them are wrong:

- **A signal does not help.** Claude answers SIGTERM with the same wrap-up and exits 0 in about the same time. The tracker's plan, a short SIGTERM grace, would have saved nothing.
- **A kill is fast and leaves a mess.** It skips the wrap-up and leaves the CLI's session bookkeeping in the user's own `~/.claude` and `/tmp`, where Claude may take it for sessions that are still running. (The five killed runs of this experiment left such files, which were removed by hand.)
- **Not waiting is what is left,** while letting the process finish. That is what I-05 does. Switching the upload off (the last two rows) would also remove the half second, but it turns off that reporting for every turn Seatline runs, which is the owner's decision and not an adapter's: it is tracker item I-11.

## What was done

A Claude turn that succeeded and keeps no session now completes when Claude prints its final `result`. Its answer and usage are in hand, and there is no transcript for the next turn to resume. The process is handed to a bounded `Reaper` (`seatline_core::process`): a helper thread waits for it to exit on its own for up to the adapter's `finish` grace (5 s), asks it to stop if it has not, and kills it if that is not enough, exactly as the turn would have done had it waited. At most 8 are waited for at once, across all applications; a turn past that waits for its own process, as before. Turns that keep a session wait as before, because Claude is still writing the transcript the next turn resumes, and so do failed turns. The wire is unchanged.

## Before and after, live

**Alternating runs.** The previous commit's binaries and this change's were run in turn (old, new, old, new) on the same scenarios, `warm-send` and `prepared-send`, 12 measured requests each and 1 warm-up, so that the sandbox's drift reaches both alike. Medians in milliseconds over the 48 requests each binary served; the last two columns are the gaps between the two runs of the same binary, which is the noise to compare the difference with. Raw reports: [old 1](2026-10-09-claude-live-i05-old-1.json), [new 1](2026-10-09-claude-live-i05-new-1.json), [old 2](2026-10-09-claude-live-i05-old-2.json), [new 2](2026-10-09-claude-live-i05-new-2.json).

| | Old 1 | Old 2 | New 1 | New 2 | Old | New | New − old | Old 2 − old 1 | New 2 − new 1 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `submit→done` | 2347 | 2310 | 1968 | 1927 | 2330 | 1948 | **−382** | −37 | −41 |
| `completion` | 464 | 477 | 27 | 26 | 467 | 27 | −441 | +13 | −1 |
| `tail` | 439 | 445 | 0.1 | 0.1 | 439 | 0.1 | −439 | +7 | 0 |
| `init` | 657 | 676 | 665 | 669 | 669 | 667 | −3 | +19 | +4 |
| `text wait` | 1185 | 1146 | 1227 | 1224 | 1171 | 1225 | +54 | −40 | −4 |
| `submit→text` | 1880 | 1856 | 1943 | 1900 | 1868 | 1918 | +50 | −25 | −43 |

- **A one-word answer that keeps no session reaches its end 382 ms sooner** (16 %), against differences of at most 41 ms between runs of one binary. It is the whole of the wait after the answer: `tail` falls from 0.44 s to 0.1 ms, and what remains of `completion` (27 ms) is the broker's own polling.
- **The next launch is not slowed by the previous process still leaving:** `init` is the same (−3 ms) although a new Claude starts while the last one is finishing its upload.
- **First text may arrive about 50 ms later,** which is as large as the noise between runs (25 to 43 ms) and so not established. If real, it would be the previous process's upload and the next request sharing the sandbox's one proxy and four CPUs. It is a cost of the overlap in a workload that makes requests back to back; an application that asks once and then waits for the user does not overlap.

**The whole scenario set, once.** The same six scenarios as the baseline, 10 measured requests each, after the change. Medians in milliseconds; "before" is run 1 / run 2 of the baseline. The baseline's runs and this one were not interleaved, and this one's `init` was 40 to 140 ms slower in **every** scenario, `resumed-context` too (which does not leave a process behind), so its end-to-end numbers carry that drift; the `completion` and `tail` rows are the mechanism and are not affected by it. Raw report: [after](2026-10-09-claude-live-after-i05.json); the [comparison](2026-10-09-claude-live-i05-comparison.md) is `seatline-bench compare` of baseline run 1 against it.

| Scenario | `tail` before → after | `completion` before → after | `submit→done` before → after |
| --- | --- | --- | --- |
| `cold-broker` | 476 / 485 → 0.1 | 502 / 517 → 31 | 2387 / 2538 → 2161 |
| `warm-send` | 449 / 495 → 0.1 | 476 / 516 → 32 | 2382 / 2402 → 2182 |
| `warm-send-probe` | 523 / 510 → 0.1 | 545 / 534 → 32 | 2644 / 2757 → 2397 |
| `prepared-send` | 499 / 495 → 0.1 | 530 / 531 → 29 | 2501 / 2458 → 1997 |
| `resumed-context` (keeps a session) | 469 / 523 → **476** | 501 / 550 → **534** | 2459 / 2511 → 2341 |

All 40 requests of the four scenarios that keep no session have a `tail` of 0.1 ms; the one that keeps a session is unchanged, as designed. All 60 measured requests completed.

**Processes.** While that run went, a sampler counted `claude -p` processes every 50 ms: in 3,211 samples there were none alive in 547, one in 2,440 and two in 224, **never three**, and no zombie was seen. When it ended no Claude process of the run remained, and `~/.claude/sessions` and `/tmp/cc-socks` held only the entries of the Claude session that was already running: every process had left on its own and removed its own files.

## What it may and may not claim

- **May:** in this sandbox, a one-word Claude answer that keeps no session ends about 0.38 s sooner, and the half second Claude spends on its way out no longer comes out of the answer's time.
- **May not:** carry over to a turn that keeps a session or fails (unchanged), to Codex (already stopped at its result, H-02), to Antigravity or Grok (unmeasured, unchanged), or to another Claude version: the wrap-up was observed for 2.1.295, and what the design needs of it is only that the process leaves by itself once its input has ended.
- **May not:** speak for a machine under load or for several requests at once. One request ran at a time here, so the largest number of processes ever alive was two; under a burst the bound of 8 waiting processes is what limits the overlap, and what the overlap costs the next request was only seen to be small, not measured under load.
- **No p95:** these are ten to twelve requests a scenario.

## Not measured

Antigravity and Grok; Claude under concurrent load; other CLI versions; what the wrap-up is for (it was seen to upload events, not asked why). Run `scripts/measure-claude-exit.py` on your own CLI to see its wrap-up, and the [runbook](../performance-measurement.md#recording-a-live-baseline) for the rest.
