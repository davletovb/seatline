# Deterministic fake provider

`seatline-fake-provider` is a test-only library: the behaviour of a fake provider executable, and the harness that installs it. The executable is used to exercise provider process supervision without coupling tests to Codex, Claude, or any other real provider. Run under the name `codex`, `claude`, `agy` or `grok`, it acts as that provider's CLI instead (see [Fake Codex](#fake-codex) and [The other personas](#the-other-personas)).

A library cannot name the binary a test package builds, so each package that runs the fake builds a binary of its own around `seatline_fake_provider::run()`: `seatline-tests` (`seatline-tests/src/main.rs`, binary `seatline-fake-provider`) for the runtime's tests, `seatline-bench` (binary `seatline-bench-fake-provider`) for the benchmark harness, and `test_provider` (binary `tabbeam-fake-provider`) for TabBeam's. Each passes the paths its own `env!` values give to `harness::Fixtures`, with the namespace of the application it stands for (which names the Gemini agents and the Grok owner file). The library depends on `seatline-core` and `seatline-providers` only.

It emits a deliberately small line-oriented JSON test stream on stdout. This is **not** the public TabBeam Native Messaging protocol and is not a provider API contract.

## Modes

```text
normal        stream three deterministic JSON lines and exit 0
slow          stream the same lines with 700 ms pauses between chunks
stderr        write a deterministic stderr message, then stream normally
exit-nonzero  write a diagnostic and exit 42
hang          emit a ready line, then remain alive
ignore-cancel emit ready, ignore SIGTERM and an input "cancel" command, then remain alive
malformed     emit a deliberately malformed JSON line and exit 0
large         emit exactly 2 MiB of deterministic stdout bytes and exit 0
crash         abort (SIGABRT on POSIX) without leaving a core file
echo          copy stdin to stdout until end of input, then exit 0
tree          start a `hang` descendant that shares stdout, emit a ready line, then remain alive
orphan        start a `hang` descendant that shares stdout, then exit 0 at once
escape        start a `detached` descendant that shares stdout, then exit 0 at once
detached      leave the process group (a new session on POSIX), emit its pid, then remain alive
partial       emit the start of a line with no line ending, then remain alive
env           print its working directory and environment as JSON, then exit 0
args          print the arguments after `--mode args` as a JSON array, then exit 0
```

`tree`, `orphan`, and `escape` leave processes running on purpose, to exercise process-tree cleanup. Run them only under a supervisor that stops the whole process group: in a shell pipeline, the descendant keeps the pipe open after the fake provider exits.

The `large` mode intentionally emits one 2 MiB byte stream **without a newline**. It exists to force later supervision/streaming code to chunk provider output by bounded byte size rather than assume one provider line maps to one protocol frame.

Only `args` takes arguments of its own. Invalid arguments print usage and exit 64.

Example:

```bash
cd native
cargo build -p seatline-tests
./target/debug/seatline-fake-provider --mode normal
```

The tests place strict timeouts around the non-terminating modes so CI never relies on manual cleanup.

## What the tests pin

`seatline-tests/tests/modes.rs` verifies both completed-output and mid-stream behavior. Slow mode is run to completion, is separately required to still be running at one second, and is killed at 0.5 seconds to prove its first line was already flushed. On POSIX, `seatline-tests/tests/signals.rs` sends SIGTERM directly: `hang` must terminate on SIGTERM, while `ignore-cancel` must survive SIGTERM until the test escalates to SIGKILL and reaps it. `ignore-cancel` blocks SIGTERM before it prints its ready line, so a supervisor that signals right after readiness always hits the ignored state.

`seatline-tests/tests/process_manager.rs` and `seatline-tests/tests/process_stress.rs` test the runtime's provider process manager (NAT-04) against this binary, including the descendant modes; see "Provider processes" in `native/README.md`. `env` and `args` show what a process received: only the environment its spec sets, in the directory it names, with each argument whole (SEC-02). `seatline-tests/tests/stream_manager.rs` tests the stream manager (NAT-05) against the same modes, and `partial` exists for it: a stream cancelled while it holds an unfinished line must drop that line.

## Fake Codex

When the binary's file name is `codex` (`codex.exe` on Windows), it answers the two commands the Codex adapter runs, with the event shapes Codex CLI 0.156 prints:

- `codex login status` exits 0 like a signed-in Codex, printing a masked key to stderr that the adapter must never read;
- `codex exec --json [...] [resume <thread id>] -` reads the question from stdin to end of file, as Codex does, and prints a JSON event per line: `thread.started`, a non-fatal warning item, `turn.started`, a command item, the answer `You asked: <question>`, and `turn.completed`. It always writes a fake secret token to stderr, which must never reach events or diagnostics.

The harness (`harness::FakeCodex`) hard-links the binary into a fresh directory as `codex`, in cargo's temporary directory under `target/`, and points the adapter at that directory. A link rather than a copy: a copy is open for writing while it is made, and a process another test thread starts at that moment would keep it busy (`ETXTBSY`) when the test runs it. A file there named `codex-scenario` chooses the behaviors, one `login=<behavior>` line and one `exec=<behavior>` line:

```text
login=signed-in      exit 0 (the default)
login=signed-out     print "Not logged in" and exit 1
login=broken         exit 3
login=hangs          never answer
login=floods         write stdout and stderr without end

exec=answers         answer the question (the default)
exec=two-messages    answer in two agent messages
exec=slow            answer in two messages 300 ms apart
exec=huge            answer with 300,000 bytes, mostly multi-byte characters
exec=fails-401       fail the turn on a rejected API key, after a retry notice
exec=fails-429       fail the turn on a rate limit, after a retry notice
exec=fails-500       fail the turn on an overloaded service, after a retry notice
exec=crashes         abort after the turn starts
exec=no-result       exit 0 after the turn starts, without finishing it
exec=never-starts    announce the thread, then hang before the turn starts
exec=goes-quiet      start the turn, then hang
exec=ignores-cancel  block SIGTERM, start the turn, then hang
exec=malformed       print a line that isn't JSON, then hang
exec=oversized       print a 9 MiB agent message
exec=resume-fails    exit 1 with no JSON when asked to resume a thread
exec=lingers         finish the turn, then hang instead of exiting
exec=dribble         answer a few bytes at a time, 2 ms apart
exec=stderr-flood    write 128 MiB to stderr while answering
exec=endless-stderr  start the turn, then write stderr without end
exec=stdout-flood    report 100,000 progress events, then answer "Done flooding."
exec=endless-flood   start the turn, then report progress without end
exec=floods-and-ignores-cancel
                     block SIGTERM, start the turn, then report progress without end
exec=unknown-flood   start the turn, then print events TabBeam doesn't know, without end
exec=exits-nonzero   start the turn, then exit 3
exec=invalid-utf8    print an answer line that isn't UTF-8, then hang
exec=endless-line    print one line that never ends
exec=by-prompt       behave as the question's first word names, so one host can
                     run different behaviors side by side
```

The floods write many lines to a write, as fast as the pipe takes them. Every behavior that hangs or never ends stops after 60 seconds, so a test run that was killed leaves nothing running for long.

Each run appends what the adapter sent next to the binary, so tests can check it: its arguments and the first `PATH` entry to `codex-invocations`, its working directory and whole environment to `codex-environment` (a JSON object per line), and each `exec`'s question to `codex-prompts` (NUL-separated) and its process ID to `codex-pids`.

The hostile fake-process matrix (TST-04) runs the fake `codex` in each hostile behavior, alone and several at once, and checks the normalized outcome, the time it took, and that nothing was left behind; see "Hostile providers" in `native/README.md`. It runs twice: `seatline-tests/tests/hostile_matrix.rs` under the runtime's scheduler, and `test_provider/tests/hostile_matrix.rs` through the whole TabBeam host. `harness` holds what the tests share: installing a fake CLI (`Fixtures`, `FakeCodex`, `FakeClaude`, `FakeGemini`, `FakeGrok`) and reading back what it recorded. `resources` measures the test process's threads, file descriptors and peak memory, for the matrices. TabBeam's pacing of input frames and its host sessions are in `test_provider/tests/support/mod.rs`.

## The other personas

- **Claude** (`claude`): answers `claude auth status` and `claude -p` in stream-json mode. A `claude-scenario` file chooses `auth=<signed-in|signed-out|broken|hangs>` and `print=<behavior>`: `answers` (the default), `two-messages`, `two-deltas`, `hangs`, `ignores-cancel`, `malformed`, `fails-auth`, `fails-rate`, `no-result`, `flooding` (events the adapter doesn't know, without end), `resume-fails`, `resume-crashes`, `result-session-gone`, `forks-session`, `lingers`, `keeps-talking`, and the search variants `search-narrates`, `search-long`, `search-no-links`, `search-bad-urls` and `search-hostile`. It records `claude-invocations`, `claude-prompts` and `claude-pids` like the fake Codex does.
- **Antigravity** (`agy`, for the Gemini adapter): answers `agy models` and one-shot stream-json turns. There is no scenario file: the model a turn asks for chooses the behaviour (`gemini-test` answers; `gemini-tool-violation`, `gemini-unknown-step`, `gemini-bad-init`, `gemini-slow-init`, `gemini-cumulative-done`, `gemini-multi-search`, `gemini-auth-fail` and `gemini-hang` (started, then silent) each misbehave in one way). It writes a transcript into a fake Antigravity brain directory and a conversation database beside it (`conversations/<id>.db`, prompt included), as `agy` does, so tests can check both are removed (`FakeGemini::kept`), and records `agy-invocations` (its arguments, so the agent it ran as), `agy-prompts` (what it read on stdin, NUL-separated), `agy-agents` (the agent definition it ran, system prompt included) and `agy-pids`.
- **Grok** (`grok`): answers `grok models` and one-shot headless turns. The model chooses the behaviour here too (`grok-4.6` and the alias `grok-4` answer; the `grok-init-*` models each break one boundary of `system/init`; `grok-tool-violation`, `grok-result-auth` and `grok-hang` misbehave in one way). It records `grok-invocations`, `grok-prompts` (from the prompt file), `grok-agents` (the agent definition it ran; the system prompt is not in it, as Grok ignores an agent's body and gets it in the prompt) and `grok-pids`.
