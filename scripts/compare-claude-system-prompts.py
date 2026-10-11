#!/usr/bin/env python3
"""Does Claude's own system prompt cost time? (tracker I-10)

    SEATLINE_BENCH_LIVE=1 python3 scripts/compare-claude-system-prompts.py ROUNDS [options]
    python3 scripts/compare-claude-system-prompts.py --from-raw FILE [--prompts FILE] [--search]

Runs the command line and environment Seatline gives Claude for a plain turn
that keeps no session, in rounds that take every variant in turn (so drift in
the network or the machine reaches all alike):

  base     as the adapter runs it
  dynamic  plus --exclude-dynamic-system-prompt-sections: the per-machine
           sections (working directory, environment, memory paths, git status)
           of Claude's default system prompt move into the first user message,
           so the rest of the prompt can be shared across workspaces in the
           prompt cache
  short    plus --system-prompt-file with a short text, in place of Claude's
           default system prompt
  padded   the same short text followed by filler that says nothing, about as
           long as Claude's default system prompt. If `short` is faster than
           `base` and `padded` is not, it is the shorter prompt that saves the
           time, not the absence of Claude's words

and prints, per variant, the input tokens (and how many were read from or
written to the prompt cache) and the time to the first answer text and to the
result, with the paired difference from `base` and a sign test over the
rounds, then where in the run the time went (Claude ready, its answer begun,
the first text, the result), and last how often each variant gave the answer
`base` usually gives and in how many runs the model thought before it
answered (a prompt that makes it think adds time). Each round uses the next
prompt; round 0 only warms the prompt cache and is not counted.

Options:
  --prompts FILE       one prompt per line, a backslash and n standing for a
                       newline (default: four short proofreading prompts)
  --fresh-cwd          run every turn in a directory of its own. The default
                       prompt names the working directory, so every run misses
                       the cache for that part, as a second application's first
                       request does
  --search             give the turns WebSearch, as a search turn has
  --short-prompt TEXT  the text of the `short` variant
                       (default: "Answer the user's message directly.")
  --variants LIST      comma separated, from base,dynamic,short,padded
                       (default: base,dynamic,short)
  --answers FILE       write every answer, one JSON object per line, for
                       comparing them by eye or with a judge. Answer quality is
                       not measured here: that needs your own corpus.
  --raw FILE           write every run (timings, usage, answer) as JSON lines
  --from-raw FILE      print the report for rows saved with --raw, running
                       nothing and using no quota. Give the same --prompts or
                       --search as the run that saved them

Each run sends one real prompt, so it uses a little of the account's quota and
needs the explicit SEATLINE_BENCH_LIVE=1, like `seatline-bench run --live`.
`CLAUDE_BIN` names the executable if `claude` is not on PATH. Needs Python 3.11
or later (`process_group`).
"""
import collections
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import uuid

ARGS = [
    "-p", "--output-format", "stream-json", "--input-format", "stream-json",
    "--verbose", "--include-partial-messages", "--permission-mode", "default",
    "--strict-mcp-config", "--disallowedTools", "mcp__*", "--no-session-persistence",
]

# What Seatline passes on to a provider from its own environment
# (platform/src/environment.rs), and the variable the Claude adapter adds. A
# shell inside another Claude session carries many more, which change what the
# CLI does (it can wait for a summary of the turn before it prints its result).
INHERITED = [
    "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
    "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR",
    "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy",
    "NO_PROXY", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
    "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_GIT_BASH_PATH",
]

PROOFREADING = [
    "Proofread this and return only the corrected text: Their going to the libary tomorow to study for there exams, and they dont no weather it will be open.",
    "Proofread this and return only the corrected text: The commitee have decided to postpone the meeting untill further notise, which effect everyones schedule.",
    "Proofread this and return only the corrected text: Me and my freind went too the store, but it was closed, so we goes home and watched a movie.",
    "Proofread this and return only the corrected text: Its important to remember that practise makes perfect, and that mistakes is how we learn.",
]
SEARCH_PROMPT = (
    "Search the web before you answer the question below, and base your answer on what you find. "
    "Answer directly in two sentences.\n\nWhat is the latest stable version of the Rust programming language?"
)
VARIANTS = ("base", "dynamic", "short", "padded")
DEFAULT_VARIANTS = ("base", "dynamic", "short")
PADDING = "\n".join(
    f"Reference note {number}: nothing in this note applies to the task, and it can be ignored."
    for number in range(1, 76)
)


def provider_environment(executable):
    env = {name: os.environ[name] for name in INHERITED if name in os.environ}
    env["DISABLE_AUTOUPDATER"] = "1"
    env["PATH"] = os.path.dirname(executable) + os.pathsep + os.environ.get("PATH", "")
    return env


def variant_args(variant, scratch, short_text):
    if variant == "base":
        return []
    if variant == "dynamic":
        return ["--exclude-dynamic-system-prompt-sections"]
    path = os.path.join(scratch, f"system-{uuid.uuid4().hex[:8]}.txt")
    with open(path, "w", opener=lambda p, f: os.open(p, f, 0o600)) as file:
        file.write(short_text if variant == "short" else f"{short_text}\n\n{PADDING}")
    return ["--system-prompt-file", path]


def run_once(executable, variant, prompt, cwd, scratch, search, short_text):
    tools = ["--tools", "WebSearch", "--allowedTools", "WebSearch"] if search else ["--tools", ""]
    message = json.dumps({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
    }) + "\n"
    started = time.monotonic()
    process = subprocess.Popen(
        [executable, *ARGS, *tools, *variant_args(variant, scratch, short_text)],
        cwd=cwd, env=provider_environment(executable),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        process_group=0,
    )
    process.stdin.write(message.encode())
    process.stdin.close()
    ready = answer_begun = first_text = result = None
    usage = None
    answer = ""
    error = None
    buffer = b""
    while True:
        chunk = os.read(process.stdout.fileno(), 65536)
        now = (time.monotonic() - started) * 1000
        if not chunk:
            break
        buffer += chunk
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") == "system" and event.get("subtype") == "init":
                ready = now if ready is None else ready
            elif event.get("type") == "stream_event":
                inner = event.get("event", {})
                delta = inner.get("delta", {})
                if inner.get("type") == "message_start" and answer_begun is None:
                    answer_begun = now
                if inner.get("type") == "content_block_delta" and delta.get("type") == "text_delta":
                    if first_text is None:
                        first_text = now
                    answer += delta.get("text", "")
            elif event.get("type") == "result":
                result = now
                usage = event.get("usage")
                if event.get("is_error"):
                    error = str(event.get("result"))[:200]
    process.stderr.read()
    process.wait()
    return {"variant": variant, "ready_ms": ready, "answer_begun_ms": answer_begun,
            "first_text_ms": first_text, "result_ms": result,
            "usage": usage, "answer": answer, "error": error}


def percentile(values, fraction):
    values = sorted(values)
    position = (len(values) - 1) * fraction
    low, high = math.floor(position), math.ceil(position)
    return values[low] + (values[high] - values[low]) * (position - low)


def sign_test(differences):
    """Two-sided p for 'neither is usually slower', from the signs alone."""
    slower = sum(d > 0 for d in differences)
    faster = sum(d < 0 for d in differences)
    total = slower + faster
    if total == 0:
        return 1.0
    tail = sum(math.comb(total, i) for i in range(min(slower, faster) + 1))
    return min(1.0, 2 * tail / 2 ** total)


def tokens(row):
    usage = row["usage"]
    return (usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0)
            + usage.get("cache_read_input_tokens", 0),
            usage.get("cache_creation_input_tokens", 0), usage.get("cache_read_input_tokens", 0))


def alike(text):
    return " ".join(text.split())


def report_answers(rows, variants, prompt_count):
    """How often a variant gave base's usual answer to the same prompt, and how much the model wrote.

    `base` against itself is the floor: the model does not always answer the
    same way twice. Only prompts with one short, fixed answer (proofreading)
    can be compared this way; `prompt_count` is None for the others. Output
    tokens include the ones the model spent thinking before it answered, which
    the CLI does not show; they are counted apart because a prompt that makes
    the model think more adds time to the first text.
    """
    usual = {}
    if prompt_count:
        counts = {}
        for row in rows:
            if row["round"] > 0 and row["variant"] == "base" and row["answer"]:
                counts.setdefault(row["round"] % prompt_count, collections.Counter())[alike(row["answer"])] += 1
        usual = {index: seen.most_common(1)[0][0] for index, seen in counts.items()}
    print(f'{"variant":8s} | {"same answer as base usually gives":>34s} | {"output tokens (median, largest)":>31s} | thought first')
    for variant in variants:
        answered = [r for r in rows if r["round"] > 0 and r["variant"] == variant and r["answer"] and r["usage"]]
        if not answered:
            continue
        same = (f"{sum(alike(r['answer']) == usual.get(r['round'] % prompt_count) for r in answered)} of {len(answered)}"
                if prompt_count else "-")
        out = [r["usage"].get("output_tokens", 0) for r in answered]
        thought = sum(r["usage"].get("output_tokens_details", {}).get("thinking_tokens", 0) > 0 for r in answered)
        print(f'{variant:8s} | {same:>34s} | {statistics.median(out):>22.0f}, {max(out):>7d} | '
              f'{thought} of {len(answered)} runs')


def report(rows, variants, prompt_count):
    by = {variant: {} for variant in variants}
    for row in rows:
        if row["round"] > 0 and row["usage"] and row["first_text_ms"] is not None:
            by[row["variant"]][row["round"]] = row
    print(f'{"variant":8s} {"n":>3s} | {"input":>6s} {"cache write":>11s} {"cache read":>10s} | '
          f'{"first text (ms)":>15s} {"p25-p75":>11s} {"p95":>6s} | {"to result":>9s}')
    for variant in variants:
        rs = list(by[variant].values())
        if not rs:
            print(f"{variant:8s}   0 | no completed run")
            continue
        total, write, read = (statistics.median(column) for column in zip(*(tokens(r) for r in rs)))
        first = [r["first_text_ms"] for r in rs]
        print(f'{variant:8s} {len(rs):3d} | {total:6.0f} {write:11.0f} {read:10.0f} | '
              f'{statistics.median(first):15.0f} {percentile(first, .25):5.0f}-{percentile(first, .75):<5.0f} '
              f'{percentile(first, .95):6.0f} | {statistics.median(r["result_ms"] for r in rs):9.0f}')
    print()
    print(f'{"variant":8s} | {"where the run was (median ms from launch)":>46s}')
    print(f'{"":8s} | {"ready":>9s} {"answer begun":>13s} {"first text":>11s} {"result":>9s}')
    for variant in variants:
        rs = list(by[variant].values())
        if rs:
            columns = []
            for name in ("ready_ms", "answer_begun_ms", "first_text_ms", "result_ms"):
                seen = [r[name] for r in rs if r.get(name) is not None]
                columns.append(f"{statistics.median(seen):.0f}" if seen else "-")
            print(f'{variant:8s} | {columns[0]:>9s} {columns[1]:>13s} {columns[2]:>11s} {columns[3]:>9s}')
    print()
    base = by.get("base", {})
    for variant in variants:
        if variant == "base" or not base:
            continue
        for column, what in (("first_text_ms", "time to first text"), ("result_ms", "time to the result")):
            differences = [by[variant][n][column] - base[n][column] for n in base if n in by[variant]]
            if differences:
                print(f"{variant} - base, {what}: median paired difference "
                      f"{statistics.median(differences):+.0f} ms over {len(differences)} rounds, "
                      f"slower in {sum(d > 0 for d in differences)}, sign test p = {sign_test(differences):.2f}")
    print()
    report_answers(rows, variants, prompt_count)


def main(argv):
    args = argv[1:]
    options = {"--prompts": None, "--short-prompt": "Answer the user's message directly.",
               "--variants": ",".join(DEFAULT_VARIANTS), "--answers": None, "--raw": None, "--from-raw": None}
    flags = {"--fresh-cwd": False, "--search": False}
    positional = []
    while args:
        argument = args.pop(0)
        if argument in flags:
            flags[argument] = True
        elif argument in options:
            if not args:
                raise SystemExit(__doc__)
            options[argument] = args.pop(0)
        elif argument.startswith("--"):
            raise SystemExit(__doc__)
        else:
            positional.append(argument)
    variants = [v for v in options["--variants"].split(",") if v]
    if not variants or any(v not in VARIANTS for v in variants):
        raise SystemExit(__doc__)
    if options["--prompts"]:
        with open(options["--prompts"]) as file:
            prompts = [line.rstrip("\n").replace("\\n", "\n") for line in file if line.strip()]
    else:
        prompts = [SEARCH_PROMPT] if flags["--search"] else PROOFREADING
    if not prompts:
        raise SystemExit("no prompts")
    prompt_count = None if flags["--search"] else len(prompts)
    if options["--from-raw"]:
        if positional or options["--raw"] or options["--answers"] or flags["--fresh-cwd"]:
            raise SystemExit(__doc__)
        with open(options["--from-raw"]) as file:
            saved = [json.loads(line) for line in file if line.strip()]
        report(saved, list(dict.fromkeys(row["variant"] for row in saved)), prompt_count)
        return
    if len(positional) != 1 or not positional[0].isdecimal() or int(positional[0]) < 1:
        raise SystemExit(__doc__)
    rounds = int(positional[0])
    if os.environ.get("SEATLINE_BENCH_LIVE") != "1":
        raise SystemExit("this sends real prompts and uses a little of the account's quota; set SEATLINE_BENCH_LIVE=1 to confirm")
    if sys.version_info < (3, 11):
        raise SystemExit("needs Python 3.11 or later")
    executable = os.environ.get("CLAUDE_BIN") or shutil.which("claude")
    if not executable:
        raise SystemExit("no `claude` on PATH; set CLAUDE_BIN")
    executable = os.path.realpath(executable) if os.path.islink(executable) else executable

    base_dir = os.path.join(os.path.expanduser("~"), ".cache", "seatline-claude-prompts")
    os.makedirs(base_dir, exist_ok=True)
    scratch = tempfile.mkdtemp(prefix="run-", dir=base_dir)
    rows = []
    try:
        for round_number in range(rounds + 1):
            prompt = prompts[round_number % len(prompts)]
            for variant in variants:
                cwd = tempfile.mkdtemp(prefix="cwd-", dir=scratch) if flags["--fresh-cwd"] else scratch
                row = run_once(executable, variant, prompt, cwd, scratch, flags["--search"], options["--short-prompt"])
                row["round"] = round_number
                rows.append(row)
                done = row["usage"] and row["first_text_ms"] is not None
                note = "" if done else f' (no answer{": " + row["error"] if row["error"] else ""})'
                print(f'round {round_number} {variant:8s} first text {row["first_text_ms"] or 0:6.0f} ms{note}', flush=True)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print()
    report(rows, variants, prompt_count)
    if options["--raw"]:
        with open(options["--raw"], "w") as file:
            file.writelines(json.dumps(row) + "\n" for row in rows)
    if options["--answers"]:
        with open(options["--answers"], "w") as file:
            for row in rows:
                if row["round"] > 0:
                    file.write(json.dumps({"round": row["round"], "variant": row["variant"],
                                           "answer": row["answer"]}) + "\n")


if __name__ == "__main__":
    main(sys.argv)
