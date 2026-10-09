#!/usr/bin/env python3
"""How long does the Claude CLI take to exit after it prints its final result?

Seatline ends a Claude turn that keeps no session when Claude prints its
`result`, and leaves the process to finish exiting on its own (tracker I-05).
What the CLI does between its result and its exit is the CLI's, and differs by
version, so this measures it on yours: it runs the adapter's own command line
(a plain turn, no tools, no session kept), with the environment Seatline gives
a provider and the process group it starts one in, and times result -> exit.

    SEATLINE_BENCH_LIVE=1 python3 scripts/measure-claude-exit.py MODE RUNS [--env NAME=VALUE ...]

MODE is `natural` (do nothing: the exit as it is) or `term` (send SIGTERM to
the group at the result: does a signal shorten it?). Each run sends one real
one-word prompt, so it uses a little of the account's quota and needs the
explicit SEATLINE_BENCH_LIVE=1, like `seatline-bench run --live`.

There is no `kill` mode on purpose. Killing the CLI after its result exits in
milliseconds, but skips its wrap-up and leaves its session bookkeeping behind
(`~/.claude/sessions/<pid>.json` and its `.key`, `/tmp/cc-socks/<pid>.sock`).

`--env` adds a variable to the child's environment, for example
`DISABLE_TELEMETRY=1` to see whether the wrap-up is the CLI's usage upload.
`CLAUDE_BIN` names the executable if `claude` is not on PATH.

Needs Python 3.11 or later (`process_group`).
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time

ARGS = [
    "-p", "--output-format", "stream-json", "--input-format", "stream-json",
    "--verbose", "--include-partial-messages", "--permission-mode", "default",
    "--tools", "", "--strict-mcp-config", "--disallowedTools", "mcp__*",
    "--no-session-persistence",
]

# What Seatline passes on to a provider from its own environment
# (platform/src/environment.rs), and the two variables the Claude adapter adds.
INHERITED = [
    "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
    "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR",
    "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy",
    "NO_PROXY", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
    "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_GIT_BASH_PATH",
]


def provider_environment(executable, extra):
    env = {name: os.environ[name] for name in INHERITED if name in os.environ}
    env["DISABLE_AUTOUPDATER"] = "1"
    env["PATH"] = os.path.dirname(executable) + os.pathsep + os.environ.get("PATH", "")
    env.update(extra)
    return env


def run_once(executable, mode, cwd, extra):
    message = json.dumps({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": "Reply with the single word: ok"}]},
    }) + "\n"
    process = subprocess.Popen(
        [executable, *ARGS], cwd=cwd, env=provider_environment(executable, extra),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        bufsize=0, process_group=0,
    )

    def drain_stderr():
        while os.read(process.stderr.fileno(), 65536):
            pass

    threading.Thread(target=drain_stderr, daemon=True).start()
    process.stdin.write(message.encode())
    process.stdin.close()
    result_at = None
    buffer = b""
    while True:
        chunk = os.read(process.stdout.fileno(), 65536)
        now = time.monotonic()
        if not chunk:
            break
        buffer += chunk
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") == "result" and result_at is None:
                result_at = now
                if mode == "term":
                    os.killpg(process.pid, signal.SIGTERM)
    code = process.wait()
    exited_at = time.monotonic()
    if result_at is None:
        raise SystemExit("the CLI exited without a result: is it signed in?")
    return (exited_at - result_at) * 1000, code


def main(argv):
    if os.environ.get("SEATLINE_BENCH_LIVE") != "1":
        raise SystemExit("this sends real prompts and uses a little of the account's quota; set SEATLINE_BENCH_LIVE=1 to confirm")
    if sys.version_info < (3, 11):
        raise SystemExit("needs Python 3.11 or later")
    args = argv[1:]
    extra = {}
    while "--env" in args:
        index = args.index("--env")
        name, _, value = args[index + 1].partition("=")
        extra[name] = value
        del args[index:index + 2]
    if len(args) != 2 or args[0] not in ("natural", "term") or not args[1].isdigit():
        raise SystemExit(__doc__)
    mode, runs = args[0], int(args[1])
    executable = os.environ.get("CLAUDE_BIN") or shutil.which("claude")
    if not executable:
        raise SystemExit("no `claude` on PATH; set CLAUDE_BIN")
    executable = os.path.realpath(executable) if os.path.islink(executable) else executable
    cwd = os.path.join(os.path.expanduser("~"), ".cache", "seatline-claude-exit")
    os.makedirs(cwd, exist_ok=True)
    times = []
    for index in range(runs):
        milliseconds, code = run_once(executable, mode, cwd, extra)
        times.append(milliseconds)
        print(f"{mode} run {index}: result -> exit {milliseconds:.0f} ms (exit code {code})")
    times.sort()
    print(f"{mode}: n={len(times)} min {times[0]:.0f} median {times[len(times) // 2]:.0f} max {times[-1]:.0f} ms")


main(sys.argv)
