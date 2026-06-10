# acp-cli

Command-line front-end for the [acp-core][repo] libraries.

[repo]: https://github.com/ky2renzzz/acp-core

## Installation

```sh
cargo install acp-cli
```

This installs two binaries:
- `acp` — the main CLI
- `acp-echo-agent` — a minimal ACP-compliant agent for testing

## Commands

### Record

Proxy stdio between client and agent, writing a trace:

```sh
acp record --trace ./session -- ./my-agent
```

Options:
- `--capture-env KEY` — include this env var in manifest (repeatable)
- `--no-recording-env` — skip host metadata entirely
- `--clock {system|epoch|nanos:N}` — timestamp source (default: system)

### Replay

Act as the agent, replaying recorded responses:

```sh
acp replay ./session < client-stream.jsonl > replayed.jsonl
```

Options:
- `--remap-ids` — tolerate different JSON-RPC id numbering

### Emit

Write all recorded A2C frames to stdout:

```sh
acp emit ./session > output.jsonl
```

### Inspect

Print a human-readable summary:

```sh
acp inspect ./session
acp inspect ./session --full  # show all events
```

### Diff

Compare two traces:

```sh
acp diff ./session-a ./session-b
acp diff ./session-a ./session-b --dot | dot -Tsvg > diff.svg
```

Exit code 1 if traces differ, 0 if identical.

## Example

```sh
# Record a session
acp record --trace ./demo -- ./target/release/acp-echo-agent < input.jsonl > recorded.jsonl

# Replay it
acp replay ./demo < input.jsonl > replayed.jsonl

# Verify byte-exact replay
cmp recorded.jsonl replayed.jsonl && echo "Identical!"
```

## License

Apache-2.0
