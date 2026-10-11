# I-10: Claude's own system prompt, 2026-10-11

Every Claude turn starts with the system prompt Claude Code puts in front of it. Tracker item I-10 asked whether that prompt costs time, whether `--exclude-dynamic-system-prompt-sections` or `--system-prompt` would help, and what it would do to the answers. This is what was measured, and why **nothing in Seatline's Claude launch changed**. It is **Claude only**, from one cloud sandbox: CLI 2.1.296, default model `claude-opus-5-5`, an OAuth sign-in on a plan of unknown size, the adapter's own command line and environment, and a scratch workspace that is not a git repository and holds no `CLAUDE.md`. [`scripts/compare-claude-system-prompts.py`](../../scripts/compare-claude-system-prompts.py) ran everything below; the raw rows are beside this file, and `--from-raw FILE` prints any of these tables again without a Claude. This is the second version of the evidence: the first took the variants in the same order every round, and a review asked for it to be rotated and everything measured again (see [what changed](#what-changed-when-the-order-was-rotated)).

## What a plain turn sends

A turn that gives Claude no tools sends about 2.7 thousand input tokens for a one-line question. About 2.1 thousand of them are Claude Code's own system prompt. The application's own `system` text is not part of that prompt: Seatline puts it at the start of the user message (`seatline_core::prompt`), because a system prompt on the command line would show in the process list. With the prompt cache warm, all but a few of the tokens are cache reads (2,709 of 2,712) and nothing is written. In a workspace the prompt has not seen (its text names the working directory), the first request writes about 2,200 tokens and reads about 535.

Four variants, taken in every round, **in a different order each time**: the four orders of a balanced Latin square (every variant takes each position, and follows each other variant, equally often), dealt out in shuffled blocks (seed 1) so that a variant's place in the round does not stand in for the variant and no prompt always meets the same order. The counted rounds fill the blocks, so every variant was taken exactly equally often in every position (8 times each in the 32 warm rounds). Round 0 only warms the cache and is not counted. Times are milliseconds from the launch of the process, and each comparison is **paired within a round** with a two-sided sign test. The prompts repeat every fourth round, so a prompt's own text is a cache read from its second turn on, which an application's changing text is not (some tens of tokens).

- **`base`**: as the adapter runs it.
- **`dynamic`**: plus `--exclude-dynamic-system-prompt-sections`, which moves the per-machine sections (working directory, environment, memory paths, git status) of the default prompt into the first user message.
- **`short`**: plus `--system-prompt-file` with the five-word text `Answer the user's message directly.`, in place of Claude's prompt.
- **`padded`**: the same text as `short` followed by 75 lines of filler that say nothing, about as long as Claude's own prompt. It tells the prompt's length from its words.

## A plain turn, prompt cache warm

32 rounds over four proofreading prompts (one per round, in turn). "Slower" counts the rounds in which the variant took longer than `base` in the same round.

| Variant | Input tokens | Cache write / read | First text (median; p25–p75; p95) | Result (median) | First text against `base` | Result against `base` |
| --- | --- | --- | --- | --- | --- | --- |
| `base` | 2,712 | 0 / 2,709 | 2,167 (2,037–2,406; 3,213) | 2,356 | | |
| `dynamic` | 2,522 | 0 / 2,520 | 2,093 (2,003–2,283; 3,253) | 2,334 | −30 ms; slower in 15 of 32; p 0.86 | −23 ms; 13 of 32; p 0.38 |
| `short` | 604 | 0 / 602 | 1,300 (1,211–1,718; 2,505) | 2,042 | **−763 ms; slower in 2 of 32; p < 0.01** | **−220 ms; 4 of 32; p < 0.01** |
| `padded` | 2,704 | 0 / 2,702 | 2,090 (2,005–2,492; 3,080) | 2,345 | −8 ms; slower in 16 of 32; p 1.00 | −25 ms; 15 of 32; p 0.86 |

Where the run was, medians in milliseconds from launch:

| Variant | Claude ready (`system`/`init`) | Answer begun (`message_start`) | First text | Result |
| --- | --- | --- | --- | --- |
| `base` | 674 | 2,073 | 2,167 | 2,356 |
| `dynamic` | 679 | 2,045 | 2,093 | 2,334 |
| `short` | 660 | 1,238 | 1,300 | 2,042 |
| `padded` | 660 | 1,974 | 2,090 | 2,345 |

The saving is all between Claude being ready and the model starting to answer: not in the CLI's start, which is within 19 ms across the variants, and not in the cache, which serves every token in all four. The first text falls by more than the result does because the answer then takes longer to arrive: from the first text to the result the median run took 726 ms with `short` and 200 ms with `base`, for the same 30-token answer (why was not looked into). The answer starts 0.76 s sooner and ends 0.22 s sooner (9 %). It is there for each of the four prompts (first text −651 to −917 ms, the median of each prompt's rounds), and the place in the round does not explain it: `short`'s median first text was 1,357, 1,282, 1,274 and 1,317 ms when it was taken first, second, third and fourth, and `base`'s 2,210, 2,133, 2,100 and 2,089 ms.

## A plain turn in a workspace the prompt has not seen

20 rounds, each run in a directory of its own (`--fresh-cwd`), as a second application's first request would be.

| Variant | Input tokens | Cache write / read | First text (median; p25–p75; p95) | Result (median) | First text against `base` | Result against `base` |
| --- | --- | --- | --- | --- | --- | --- |
| `base` | 2,733 | 2,196 / 535 | 2,085 (1,965–2,404; 2,605) | 2,287 | | |
| `dynamic` | 2,544 | 1,325 / 1,217 | 2,077 (1,995–2,459; 2,897) | 2,292 | +38 ms; slower in 11 of 20; p 0.82 | +40 ms; 11 of 20; p 0.82 |
| `short` | 615 | 613 / 0 | 1,626 (1,227–2,010; 2,464) | 2,147 | **−759 ms; slower in 4 of 20; p 0.01** | −216 ms; 8 of 20; p 0.50 |
| `padded` | 2,715 | 353 / 2,360 | 2,244 (1,982–2,593; 2,753) | 2,378 | +13 ms; slower in 10 of 20; p 1.00 | +13 ms; 12 of 20; p 0.50 |

`dynamic` does what it says for the cache (a new workspace writes 1,325 tokens, not 2,196, because the part that names the directory no longer sits in the prompt) and changes no time. Seatline runs an application's Claude turns in one workspace of its own (`claude-workspace` in the application's cache directory), so that saving would be had at most on the first turn of each application. With `short`, the first text is again about 0.76 s sooner; the gain at the end of the answer (−216 ms) is the size of the warm run's but, with 20 rounds, not established.

## A search turn

12 rounds of one question that needs a web search (about 11 s). Nothing is established: the differences are smaller than the spread, no sign test comes near 0.05, and the model thinks before it answers in 47 of 48 runs, so the time is the search and the reasoning.

| Variant | Input tokens | First text (median) | Result (median) | First text against `base` | Result against `base` |
| --- | --- | --- | --- | --- | --- |
| `base` | 8,314 | 11,607 | 11,634 | | |
| `dynamic` | 7,948 | 11,834 | 11,860 | +280 ms; slower in 8 of 12; p 0.39 | +282 ms; 8 of 12; p 0.39 |
| `short` | 4,139 | 10,923 | 10,951 | −711 ms; 4 of 12; p 0.39 | −721 ms; 4 of 12; p 0.39 |
| `padded` | 8,302 | 10,989 | 11,014 | −90 ms; 6 of 12; p 1.00 | −101 ms; 6 of 12; p 1.00 |

## What the words of a short prompt do

The saving is not a property of "short". The same `short` text, padded to the length of Claude's prompt, saves nothing (above). And a short prompt with other words saves nothing either: 32 more rounds with `short` set to `You are a careful proofreader. Correct spelling, grammar and punctuation. Return only the corrected text.` (628 input tokens, about the length of the first) against `base`:

| Variant | Input tokens | First text (median; p25–p75; p95) | Result (median) | First text against `base` | Result against `base` | Thought before answering |
| --- | --- | --- | --- | --- | --- | --- |
| `base` | 2,710 | 2,103 (1,959–2,529; 3,074) | 2,373 | | | 9 of 32 runs |
| `short` | 628 | 2,216 (1,961–2,586; 3,023) | 2,513 | −74 ms; slower in 14 of 32; p 0.60 | +2 ms; 17 of 32; p 0.86 | 16 of 32 runs |

Claude reports the tokens it spent thinking before an answer (`usage.output_tokens_details.thinking_tokens`); the CLI does not show them, and a run that thinks takes longer to reach its first text. A prompt that sets a task and a manner made the model think first in 16 of 32 runs, against 9 of 32 with Claude's own prompt. Any prompt of Seatline's own seems to do that more than Claude's: the five-word prompt in the warm run above made it think in 11 of 32 runs and the padded one in 12, against 7 for `base` and 6 for `dynamic`. **Which words go in the prompt decides whether there is any saving**, and they decide how the model behaves.

## What it does to the answers

- **Corrections were the same.** Of the 32 answers each to the four proofreading prompts, none from `dynamic`, `short` or `padded` was one that `base` had not also given to that prompt; the same holds for the proofreader wording (none of 32).
- **The application's instructions were followed.** Four prompts rendered as Seatline renders an application's system text (`Follow these instructions from the application for the whole conversation:`, then the instruction, then the question: answer in French; reply in capital letters only; end with the word DONE; answer with a JSON object with the single key `answer`), 24 rounds, six to a prompt. All 24 answers followed the instruction under `base`, `dynamic` and `short`. Times for these: `base` 2,009 ms to the first text and 2,158 to the result; `short` 1,344 and 1,990 (−628 ms, slower in 4 of 24, p < 0.01; −118 ms, slower in 8 of 24, p 0.15); `dynamic` −58 and −76 ms (p 0.31 and 0.54).
- **The style changed.** Of those 24 answers, `short` gave 8 that `base` never gave to the prompt: the Markdown bold around `Rome` was gone in 3 of the 6 French answers, and 5 of the 6 capital-letter answers had a longer greeting (`HELLO! IT'S GREAT TO MEET YOU. HOW CAN I HELP YOU TODAY?`). `dynamic` gave none, and `base` varied only in how it wrote 2 + 2. In the first version of this evidence the differences were other ones (bold gone in 4 of 5, a sentence for a word in 2 of 5 JSON answers): the particular changes are not stable from run to run, but each run found `short` giving answers that `base` did not. That is what "it changes answers" means here: not wrong, but not the same, and a corpus decides whether the difference matters.

## Claude's own minimal mode

`claude --bare` ("Minimal mode", which sets `CLAUDE_CODE_SIMPLE=1`) would give a small prompt without Seatline writing one. Both the flag and the variable make a subscription (OAuth) sign-in unusable: the run answers `Not logged in · Please run /login`, in 0.7 s and with no tokens. Its help says Anthropic authentication is then "strictly `ANTHROPIC_API_KEY` or `apiKeyHelper`". It is not an option for the sign-in the owners use.

## What was decided

Seatline's Claude launch is **unchanged**.

- `dynamic` is not worth taking: it saves 190 tokens (7 %), and some cache writes on an application's first turn, for no time, and it moves the same text to another place in the message.
- A short prompt is the one lever that moves time: about 0.7 s to the first text, and about a fifth of a second (9 % in the warm run) to the end of a short answer, with one wording and not with another. Taking it means Seatline choosing words for Claude's system prompt, which changes how the model answers (above), and whether the answers stay as good is a judgment on an application's own corpus that cannot be made from here. It is the owner's.
- If the comparison on a corpus holds, the change is small: an owner switch in `scheduling.json` like `claude_isolation`, default off, that passes a fixed text with `--system-prompt` (nothing secret goes in the argument; the application's own `system` text stays in the message). Until then it is not worth carrying.

To run the comparison on your own prompts and your own CLI (each run sends a real prompt):

```sh
SEATLINE_BENCH_LIVE=1 python3 scripts/compare-claude-system-prompts.py 32 \
  --prompts your-prompts.txt --short-prompt "your wording" \
  --variants base,short --answers answers.jsonl --raw rows.jsonl
```

`--prompts` takes one prompt per line (a backslash and `n` for a newline, as in [the instruction prompts used here](2026-10-11-claude-system-prompt-instructions-prompts.txt)); `--answers` writes every answer for reading side by side. Use a number of rounds that fills the blocks of orders (a multiple of 2 for two variants, 4 for four) and the report's "answers base did not give" column only for prompts with one short, fixed answer.

## What it may and may not claim

- **May:** on this CLI and model, a plain Claude turn sends 2.7 thousand input tokens of which 2.1 thousand are Claude Code's prompt; `--exclude-dynamic-system-prompt-sections` changes the time by nothing measurable; and a prompt of five words, in place of it, started the answer about 0.7 s sooner in each of three runs (warm, new workspace, instructions) and ended a short answer 0.1 to 0.2 s sooner (established in one of the three), for one wording and not another.
- **May not:** speak for longer answers (the saving was seen on 30-token answers; whether it persists as a constant offset or shrinks as the answer grows was not measured), for tool turns (one search turn was measured, with twelve rounds and no result), for another model, effort or CLI version, for a workspace with a `CLAUDE.md` or a large git repository (the default prompt is longer there, and so is the part `dynamic` moves), or for another operating system.
- **May not:** claim answer quality. The checks here were whether four corrections matched and whether four instructions were followed, in 24 to 32 runs each, with `short` giving 8 of 24 answers that `base` never gave.
- **No cause is claimed.** The saving is not the CLI's start, not the cache and not the place in the round, and it needs both a short prompt and neutral words. Why a shorter prompt starts the answer sooner, with every token served from the cache, was not found.

## What changed when the order was rotated

The first version of this evidence took the variants in the same order in every round (`base`, `dynamic`, `short`, `padded`), so each comparison also paired a variant with a fixed place in the round, and a review of the pull request pointed out that the sign test could not tell the two apart. The script now takes them in a different order each round, and everything was measured again (all of it with the rows replaced; the first version is in the history of the branch). The first text came out the same to within the noise (warm −739 ms became −763 ms, new workspace −705 became −759, instructions −662 became −628), and `padded` and the proofreader wording still saved nothing. The search turn, where nothing was established either time, moved most (`padded` from +1,019 ms to −90 ms), which is what noise of that size does. The end of the answer shrank: −250, −271 and −253 ms became −220, −216 and −118 ms, and only the first of the three is still established. The review also found that the script read stderr only after stdout had ended, so a child writing more than the pipe holds (300 KB here) stalled it for ever; it now reads both as they come and stops a run that outlasts `--timeout`.

## Evidence

[Warm](2026-10-11-claude-system-prompt-warm.jsonl) (32 rounds), [new workspace](2026-10-11-claude-system-prompt-new-workspace.jsonl) (20), [search](2026-10-11-claude-system-prompt-search.jsonl) (12), [instructions](2026-10-11-claude-system-prompt-instructions.jsonl) (24, with [its prompts](2026-10-11-claude-system-prompt-instructions-prompts.txt)) and [the proofreader wording](2026-10-11-claude-system-prompt-proofreader-text.jsonl) (32): one JSON object per run, with the variant, its place in the round, the timings, the full `usage`, the answer and the end of stderr. A first, exploratory pass with a throwaway script (the same design in a fixed order, the proofreader wording) saw no first-text saving either; it is not kept. An earlier attempt at the instruction prompts ran with a file whose four prompts were one line, so the model answered all four at once and thought in every run; it was discarded. The first run of the search measurement in this version was cut short by the account's usage limit and was repeated.
