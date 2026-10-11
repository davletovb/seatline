# I-10: Claude's own system prompt, 2026-10-11

Every Claude turn starts with the system prompt Claude Code puts in front of it. Tracker item I-10 asked whether that prompt costs time, whether `--exclude-dynamic-system-prompt-sections` or `--system-prompt` would help, and what it would do to the answers. This is what was measured, and why **nothing in Seatline's Claude launch changed**. It is **Claude only**, from one cloud sandbox: CLI 2.1.296, default model `claude-opus-5-5`, an OAuth sign-in on a plan of unknown size, the adapter's own command line and environment, and a scratch workspace that is not a git repository and holds no `CLAUDE.md`. [`scripts/compare-claude-system-prompts.py`](../../scripts/compare-claude-system-prompts.py) ran everything below; the raw rows are beside this file, and `--from-raw FILE` prints any of these tables again without a Claude.

## What a plain turn sends

A turn that gives Claude no tools sends about 2.7 thousand input tokens for a one-line question. About 2.1 thousand of them are Claude Code's own system prompt. The application's own `system` text is not part of that prompt: Seatline puts it at the start of the user message (`seatline_core::prompt`), because a system prompt on the command line would show in the process list. With the prompt cache warm, all but a few of the tokens are cache reads (2,709 of 2,712) and nothing is written. In a workspace the prompt has not seen (its text names the working directory), the first request writes about 2,200 tokens and reads about 535.

Four variants, taken in turn in every round so that drift in the network or the machine reaches all of them alike. Round 0 only warms the cache and is not counted. Times are milliseconds from the launch of the process, and each comparison is **paired within a round** with a two-sided sign test:

- **`base`**: as the adapter runs it.
- **`dynamic`**: plus `--exclude-dynamic-system-prompt-sections`, which moves the per-machine sections (working directory, environment, memory paths, git status) of the default prompt into the first user message.
- **`short`**: plus `--system-prompt-file` with the five-word text `Answer the user's message directly.`, in place of Claude's prompt.
- **`padded`**: the same text as `short` followed by 75 lines of filler that say nothing, about as long as Claude's own prompt. It tells the prompt's length from its words.

## A plain turn, prompt cache warm

30 rounds over four proofreading prompts (one per round, in turn). "Slower" counts the rounds in which the variant took longer than `base` in the same round.

| Variant | Input tokens | Cache write / read | First text (median; p25–p75; p95) | Result (median) | First text against `base` | Result against `base` |
| --- | --- | --- | --- | --- | --- | --- |
| `base` | 2,712 | 0 / 2,709 | 1,955 (1,939–2,298; 3,140) | 2,179 | | |
| `dynamic` | 2,522 | 0 / 2,520 | 1,974 (1,901–2,322; 2,735) | 2,181 | +8 ms; slower in 16 of 30; p 0.86 | −0 ms; 15 of 30; p 1.00 |
| `short` | 604 | 0 / 602 | 1,509 (1,153–1,787; 2,289) | 2,016 | **−739 ms; slower in 4 of 30; p < 0.01** | **−250 ms; 8 of 30; p 0.02** |
| `padded` | 2,704 | 0 / 2,702 | 1,941 (1,833–2,334; 2,862) | 2,152 | −33 ms; 14 of 30; p 0.86 | −32 ms; 14 of 30; p 0.86 |

Where the run was, medians in milliseconds from launch:

| Variant | Claude ready (`system`/`init`) | Answer begun (`message_start`) | First text | Result |
| --- | --- | --- | --- | --- |
| `base` | 640 | 1,940 | 1,955 | 2,179 |
| `dynamic` | 647 | 1,920 | 1,974 | 2,181 |
| `short` | 641 | 1,206 | 1,509 | 2,016 |
| `padded` | 635 | 1,837 | 1,941 | 2,152 |

The saving is all between Claude being ready and the model starting to answer: not in the CLI's start, which is within 12 ms across the variants, and not in the cache, which serves every token in all four. The first text falls by more than the result does because the answer then takes longer to arrive: from the first text to the result the median run took 668 ms with `short` and 195 ms with `base`, for the same 30-token answer (why was not looked into). The answer ends 0.25 s sooner and starts 0.7 s sooner. The saving is there for each of the four prompts (first text −382 to −833 ms, the median of each prompt's rounds, in this run and in the next), and is smallest, in this run, on the prompt on which the model usually thinks before it answers.

## A plain turn in a workspace the prompt has not seen

20 rounds, each run in a directory of its own (`--fresh-cwd`), as a second application's first request would be.

| Variant | Input tokens | Cache write / read | First text (median; p25–p75; p95) | Result (median) | First text against `base` | Result against `base` |
| --- | --- | --- | --- | --- | --- | --- |
| `base` | 2,736 | 2,198 / 535 | 1,955 (1,915–2,401; 2,717) | 2,155 | | |
| `dynamic` | 2,548 | 1,330 / 1,217 | 1,979 (1,927–2,496; 3,666) | 2,177 | −2 ms; slower in 10 of 20; p 1.00 | −9 ms; 8 of 20; p 0.50 |
| `short` | 617 | 615 / 0 | 1,317 (1,193–1,754; 2,040) | 2,032 | **−705 ms; slower in 2 of 20; p < 0.01** | **−271 ms; 4 of 20; p 0.01** |
| `padded` | 2,718 | 355 / 2,360 | 2,287 (1,905–2,517; 2,625) | 2,389 | −37 ms; 9 of 20; p 0.82 | +71 ms; 12 of 20; p 0.50 |

`dynamic` does what it says for the cache (a new workspace writes 1,330 tokens, not 2,198, because the part that names the directory no longer sits in the prompt) and changes no time. Seatline runs an application's Claude turns in one workspace of its own (`claude-workspace` in the application's cache directory), so that saving would be had at most on the first turn of each application.

## A search turn

10 rounds of one question that needs a web search (about 11 s). Nothing is established: the differences are smaller than the spread, and the model thinks before it answers in every run, so the time is the search and the reasoning.

| Variant | Input tokens | First text (median) | Result (median) | First text against `base` | Result against `base` |
| --- | --- | --- | --- | --- | --- |
| `base` | 8,308 | 11,115 | 11,143 | | |
| `dynamic` | 7,920 | 11,725 | 11,761 | +628 ms; slower in 8 of 10; p 0.11 | +640 ms; 8 of 10; p 0.11 |
| `short` | 4,114 | 10,777 | 10,806 | −298 ms; 3 of 10; p 0.34 | −297 ms; 3 of 10; p 0.34 |
| `padded` | 8,358 | 12,062 | 12,093 | +1,019 ms; 8 of 10; p 0.11 | +1,018 ms; 8 of 10; p 0.11 |

## What the words of a short prompt do

The saving is not a property of "short". The same `short` text, padded to the length of Claude's prompt, saves nothing (above). And a short prompt with other words saves nothing either: 30 more rounds with `short` set to `You are a careful proofreader. Correct spelling, grammar and punctuation. Return only the corrected text.` (630 input tokens, about the length of the first) against `base`:

| Variant | Input tokens | First text (median; p25–p75; p95) | Result (median) | First text against `base` | Result against `base` | Thought before answering |
| --- | --- | --- | --- | --- | --- | --- |
| `base` | 2,712 | 2,034 (1,917–2,523; 2,941) | 2,224 | | | 7 of 30 runs |
| `short` | 630 | 1,973 (1,856–2,350; 3,399) | 2,194 | −66 ms; slower in 13 of 30; p 0.58 | −19 ms; 14 of 30; p 0.86 | 13 of 30 runs |

Claude reports the tokens it spent thinking before an answer (`usage.output_tokens_details.thinking_tokens`); the CLI does not show them, and a run that thinks takes longer to reach its first text. A prompt that sets a task and a manner made the model think first in 13 of 30 runs, against 7 of 30 with Claude's own prompt; the five-word prompt above, in 11 of 30 against 5 of 30. Those counts are too small to tell apart, but the point stands: **which words go in the prompt decides whether there is any saving**, and they decide how the model behaves.

## What it does to the answers

- **Corrections were the same.** The four proofreading prompts gave the same corrected text as `base` usually does in 30 of 30 runs with `short` (and in 30 of 30 with the proofreader wording); `dynamic` 30 of 30; `padded` 28 of 30.
- **The application's instructions were followed.** Four prompts rendered as Seatline renders an application's system text (`Follow these instructions from the application for the whole conversation:`, then the instruction, then the question: answer in French; reply in capital letters only; end with the word DONE; answer with a JSON object with the single key `answer`), 20 rounds, five to a prompt. All 20 answers followed the instruction under `base`, `dynamic` and `short`. Times for these: `base` 1,969 ms to the first text and 2,086 to the result; `short` 1,307 and 1,864 (−662 ms, slower in 3 of 20, p < 0.01; −253 ms, slower in 4 of 20, p 0.01); `dynamic` −58 and −62 ms (p 0.12 and 0.26).
- **The style changed.** Of those 20 answers, `short` differed from what `base` gave in 6: the Markdown bold around `Rome` was gone in 4 of the 5 French answers, and 2 of the 5 JSON answers held a sentence where `base` always gave one word. `dynamic` differed in 1 (`2 + 2 = 4` for `2 plus 2 is 4`). That is what "it changes answers" means here: not wrong, but not the same, and a corpus decides whether the difference matters.

## Claude's own minimal mode

`claude --bare` ("Minimal mode", which sets `CLAUDE_CODE_SIMPLE=1`) would give a small prompt without Seatline writing one. Both the flag and the variable make a subscription (OAuth) sign-in unusable: the run answers `Not logged in · Please run /login`, in 0.7 s and with no tokens. Its help says Anthropic authentication is then "strictly `ANTHROPIC_API_KEY` or `apiKeyHelper`". It is not an option for the sign-in the owners use.

## What was decided

Seatline's Claude launch is **unchanged**.

- `dynamic` is not worth taking: it saves 190 tokens (7 %), and some cache writes on an application's first turn, for no time, and it moves the same text to another place in the message.
- A short prompt is the one lever that moves time, by up to a quarter of a second on a short answer (11 %), with one wording and not with another. Taking it means Seatline choosing words for Claude's system prompt, which changes how the model answers (above), and whether the answers stay as good is a judgment on an application's own corpus that cannot be made from here. It is the owner's.
- If the comparison on a corpus holds, the change is small: an owner switch in `scheduling.json` like `claude_isolation`, default off, that passes a fixed text with `--system-prompt` (nothing secret goes in the argument; the application's own `system` text stays in the message). Until then it is not worth carrying.

To run the comparison on your own prompts and your own CLI (each run sends a real prompt):

```sh
SEATLINE_BENCH_LIVE=1 python3 scripts/compare-claude-system-prompts.py 30 \
  --prompts your-prompts.txt --short-prompt "your wording" \
  --variants base,short --answers answers.jsonl --raw rows.jsonl
```

`--prompts` takes one prompt per line (a backslash and `n` for a newline, as in [the instruction prompts used here](2026-10-11-claude-system-prompt-instructions-prompts.txt)); `--answers` writes every answer for reading side by side. The report's "same answer as base usually gives" column is only meaningful for prompts with one short, fixed answer.

## What it may and may not claim

- **May:** on this CLI and model, a plain Claude turn sends 2.7 thousand input tokens of which 2.1 thousand are Claude Code's prompt; `--exclude-dynamic-system-prompt-sections` changes the time by nothing measurable; and a prompt of a few words, in place of it, shortened a short answer by 0.25 s and its first text by 0.7 s, for one wording and not another.
- **May not:** speak for longer answers (the saving was seen on 30-token answers; whether it persists as a constant offset or shrinks as the answer grows was not measured), for tool turns (one search turn was measured, with ten rounds and no result), for another model, effort or CLI version, for a workspace with a `CLAUDE.md` or a large git repository (the default prompt is longer there, and so is the part `dynamic` moves), or for another operating system.
- **May not:** claim answer quality. The checks here were whether four corrections matched and whether four instructions were followed, in 20 to 30 runs, with a change of style in 6 of 20 answers.
- **No cause is claimed.** The saving is not the CLI's start and not the cache, and it needs both a short prompt and neutral words. Why a shorter prompt starts the answer sooner, with every token served from the cache, was not found.

## Evidence

[Warm](2026-10-11-claude-system-prompt-warm.jsonl) (30 rounds), [new workspace](2026-10-11-claude-system-prompt-new-workspace.jsonl) (20), [search](2026-10-11-claude-system-prompt-search.jsonl) (10), [instructions](2026-10-11-claude-system-prompt-instructions.jsonl) (20, with [its prompts](2026-10-11-claude-system-prompt-instructions-prompts.txt)) and [the proofreader wording](2026-10-11-claude-system-prompt-proofreader-text.jsonl) (30): one JSON object per run, with the timings, the full `usage` and the answer. A first, exploratory pass with a throwaway script (the same design, the proofreader wording) saw no first-text saving either; it is not kept. An earlier attempt at the instruction prompts ran with a file whose four prompts were one line, so the model answered all four at once and thought in every run; it was discarded.
