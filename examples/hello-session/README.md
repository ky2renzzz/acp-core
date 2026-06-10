# `hello-session` — minimal record/replay example

A five-frame ACP session against the bundled `acp-echo-agent`. The
echo agent is fully deterministic (no clock, no randomness, sessionIds
derived from request counts), so the agent-to-client output is stable
byte-for-byte across runs and across machines.

Files in this directory:

| File | What it is |
|---|---|
| `client-stream.jsonl` | Five client-to-agent frames: `initialize` → `session/new` → two `session/prompt`s → `shutdown`. |
| `expected-output.jsonl` | Exactly what the echo agent emits in response: seven a2c frames (two streaming `session/update` notifications interleaved with `session/prompt` responses, plus the `initialize` / `session/new` / `shutdown` results). |

The recorded trace itself is **not committed** to the repository: it
contains volatile fields (`started_at`/`ended_at`, `recorder_pid`,
host paths in `recording_env`) that would change on every run and
could leak local-machine details. Reproduce it in two commands.

## Reproduce

From the workspace root, after `cargo build --release`:

```sh
# Record the session.
./target/release/acp record --trace examples/hello-session/trace \
    -- ./target/release/acp-echo-agent \
    < examples/hello-session/client-stream.jsonl \
    > examples/hello-session/got-recorded.jsonl

# Verify the agent output matches the committed expected output.
cmp examples/hello-session/got-recorded.jsonl \
    examples/hello-session/expected-output.jsonl
echo "recorded output matches expected: $?"
```

On Windows / PowerShell:

```powershell
cmd /c ".\target\release\acp.exe record --trace examples\hello-session\trace `
    -- .\target\release\acp-echo-agent.exe `
    < examples\hello-session\client-stream.jsonl `
    > examples\hello-session\got-recorded.jsonl"
fc /b examples\hello-session\got-recorded.jsonl examples\hello-session\expected-output.jsonl
```

Both should report no differences.

## Replay against the same client stream

```sh
./target/release/acp replay examples/hello-session/trace \
    < examples/hello-session/client-stream.jsonl \
    > examples/hello-session/got-replayed.jsonl

cmp examples/hello-session/got-replayed.jsonl \
    examples/hello-session/expected-output.jsonl
```

This is the **byte-exact replay** property: the bytes you got from the
real recording session and the bytes you get from replaying the trace
are identical, even though `acp replay` never spawns the agent.

## Try permuted client keys

ACP frames are JSON, and JSON object key order is unspecified. The
replay engine matches frames by RFC 8785 canonical hash, so a client
that emits the same logical frames with shuffled keys is still
accepted:

```sh
cat <<'EOF' | ./target/release/acp replay examples/hello-session/trace
{"method":"initialize","id":1,"jsonrpc":"2.0","params":{"protocolVersion":1}}
{"params":{"cwd":"/work"},"method":"session/new","id":2,"jsonrpc":"2.0"}
{"method":"session/prompt","jsonrpc":"2.0","id":3,"params":{"prompt":"hello","sessionId":"sess-1"}}
{"id":4,"jsonrpc":"2.0","method":"session/prompt","params":{"sessionId":"sess-1","prompt":"acp-core"}}
{"jsonrpc":"2.0","params":{},"method":"shutdown","id":5}
EOF
```

## Trigger a divergence

Change one byte in the prompt of any frame, replay, and the engine
exits non-zero with a `Divergence` error on stderr pinpointing the
seq number and the offending hash:

```sh
sed 's/hello/HELLO/' examples/hello-session/client-stream.jsonl |
    ./target/release/acp replay examples/hello-session/trace
echo "replay exit code: $?"   # non-zero
```

## Inspect

```sh
./target/release/acp inspect examples/hello-session/trace
```

Sample output (your timestamps, paths, pid will differ):

```
trace_dir       : examples/hello-session/trace
format_version  : 1
recorder        : acp-core 0.1.0
agent_argv      : ./target/release/acp-echo-agent
started_at      : 2026-06-10T08:31:11Z
ended_at        : 2026-06-10T08:31:11Z
event_count     : 12
events_hash     : blake3:69acf3c0ba9a06fc…
host            : linux / x86_64
cwd             : /path/to/acp-core
recorder_pid    : 19104
c2a frames      : 5
a2c frames      : 7
total wire bytes: 869
unique payloads : 11 (dedup ratio 1.09x)
```
