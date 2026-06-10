# acp-core

[![CI](https://github.com/ky2renzzz/acp-core/actions/workflows/ci.yml/badge.svg)](https://github.com/ky2renzzz/acp-core/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust 1.82+](https://img.shields.io/badge/rust-1.82%2B-orange.svg)](Cargo.toml)

Minimal-dependency Rust implementation of the [Agent Client Protocol][acp]
(ACP) wire format plus a **byte-exact deterministic replay engine** for
recorded agent sessions.

[acp]: https://agentclientprotocol.com/

ACP is the JSON-RPC 2.0 / stdio protocol used by the Zed editor's
agent integration, Gemini CLI's ACP mode, Claude Code, the xAI Grok
Build CLI (beta since May 2026, up to 8 parallel worktree subagents),
and any other ACP-compatible coding agent. This project gives the Rust
ecosystem a layer that the official SDK does not ship:

* **Record** a live session by transparently proxying stdio between any
  ACP client and any ACP agent — the agent does not need to be modified
  in any way, and the bytes are forwarded untouched.
* **Replay** that session against a real client and reproduce the
  observed `a2c` stream **byte-for-byte**, even if the client permutes
  the order of object keys in its requests.
* **Diff** two recordings to localise where two parallel subagents
  (Grok Build runs up to eight in isolated worktrees) parted ways.

The implementation has **7 direct dependencies** (`serde`, `serde_json`,
`blake3`, `thiserror`, `time`, `clap`, `anyhow`) — most of the
transitive bulk is `clap`'s help-rendering machinery. There is no
async runtime and **no `unsafe` code** anywhere in the workspace
(`#![forbid(unsafe_code)]` on every library crate).

---

## Crate layout

| Crate | Purpose |
|---|---|
| `acp-wire` | Newline-delimited JSON-RPC 2.0 framing, frame classification (request / response / notification), **RFC 8785 JSON canonicalization (JCS)**, blake3 payload hashing. |
| `acp-trace` | On-disk recording format: `manifest.json`, append-only `events.jsonl`, content-addressed `blobs/` keyed by canonical hash. Raw frame bytes are stored alongside the hash so replay is byte-exact. |
| `acp-proxy` | Spawns an ACP agent as a child process and tees the stdio traffic to a `TraceWriter`. Pure OS threads, no async runtime. |
| `acp-replay` | Offline emit of recorded a2c frames, and an interactive replay that pretends to be the agent against a live client, matching incoming requests by canonical hash. |
| `acp-diverge` | LCS alignment of two trace step sequences (`(direction, hash)` tuples), unified-diff and Graphviz DOT renderers. |
| `acp-cli` | One binary, `acp`, with `record / replay / emit / inspect / diff` subcommands. A second binary, `acp-echo-agent`, is a tiny but fully ACP-compliant agent used as a test fixture. |

The five library crates (`acp-wire`, `acp-trace`, `acp-proxy`,
`acp-replay`, `acp-diverge`) are all `pub`-shaped and meant to be
embedded — the CLI is just one consumer. Editor plugins or test
harnesses can pull in `acp-trace` + `acp-replay` directly and drive
replay in-process without spawning the `acp` binary.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for a full walkthrough
with diagrams of the record and replay data flows.

---

## Quick start

```sh
cargo build --release
```

Record any ACP agent (here, the bundled echo agent):

```sh
./target/release/acp record --trace ./session \
    -- ./target/release/acp-echo-agent < client-stream.jsonl > recorded.jsonl
```

Optionally pin host context into the manifest — host metadata is always
recorded, env vars only when you ask for them by name:

```sh
./target/release/acp record --trace ./session \
    --capture-env PATH --capture-env LANG --capture-env RUST_LOG \
    -- ./target/release/acp-echo-agent < client-stream.jsonl > recorded.jsonl
```

Inspect what was recorded:

```sh
./target/release/acp inspect ./session
```

Replay against a live client (the engine acts as the agent on stdio):

```sh
./target/release/acp replay ./session < client-stream.jsonl > replayed.jsonl
cmp recorded.jsonl replayed.jsonl   # byte-identical
```

Diff two recordings:

```sh
./target/release/acp diff ./session-a ./session-b
./target/release/acp diff ./session-a ./session-b --dot | dot -Tsvg > div.svg
```

A runnable five-frame example with a committed `expected-output.jsonl`
lives in [`examples/hello-session/`](examples/hello-session/README.md).
It reproduces the byte-exact record/replay property, the
key-permutation property, and the divergence-detection property in
three shell commands.

---

## Embedding as a library

The CLI is convenience scaffolding. The actual work lives in five
`pub`-shaped crates and they are meant to be used directly from editor
plugins, test harnesses, and CI tooling. Common embedding shapes:

```rust,no_run
// Replay a recorded session in-process against an in-memory client
// stream. No subprocess, no pipes — just function calls.
use std::io::Cursor;
use acp_trace::TraceReader;
use acp_replay::{replay_interactive_with, ReplayOptions};

let trace = TraceReader::open("./recorded-session")?;
let client_stream: &[u8] = b"…the JSONL the agent originally received…";
let mut out = Vec::new();

replay_interactive_with(
    &trace,
    Cursor::new(client_stream),
    &mut out,
    &ReplayOptions { remap_ids: true },   // tolerate id renumbering
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

```rust,no_run
// Record a session from your own test harness without going through
// the `acp` binary. Useful in `#[test]` functions that drive a real
// ACP agent and want a trace as a side effect.
use std::path::PathBuf;
use acp_proxy::{run, ProxyConfig};

let cfg = ProxyConfig {
    agent_argv: vec!["./my-agent".into()],
    trace_dir: PathBuf::from("./trace-test-001"),
    recorder_version: env!("CARGO_PKG_VERSION").into(),
    env_whitelist: vec!["PATH".into(), "LANG".into()],
    skip_recording_env: false,
    clock: None, // None = system clock; use FixedClock for goldens
};
run(cfg, std::io::stdin(), std::io::stdout(), std::io::stderr())?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

```rust,no_run
// Compare two traces purely as data, without spawning `acp diff`.
use acp_trace::TraceReader;
use acp_diverge::diff_two;

let a = TraceReader::open("./run-a")?;
let b = TraceReader::open("./run-b")?;
let report = diff_two(&a, "run-a", &b, "run-b");
if let Some(idx) = report.stats.first_divergence {
    eprintln!("first divergence at op #{idx}: {:?}", report.ops[idx]);
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Every public API used here has either a doc-comment (see `cargo doc
--workspace --open`) or a runnable doctest. The `acp-replay` crate's
crate-level doc includes a full end-to-end example: record a synthetic
trace with `FixedClock`, replay it, assert byte equality — that
doctest runs as part of `cargo test --workspace`.

---

## Trace format

A trace directory contains three things:

```
session/
  manifest.json       version, recorder, agent argv, start/end time,
                      event count, blake3 of events.jsonl
  events.jsonl        one EventRecord per line, append-only
  blobs/<aa>/<rest>   the original frame bytes for each unique
                      canonical hash (CAS, dedup across the session)
```

`EventRecord` fields:

```jsonc
{
  "seq": 7,                                  // monotonic
  "t_wall_ns": 1781077007326554000,          // wall-clock ns since epoch
  "dir": "c2a" | "a2c",
  "kind": "request" | "response" | "notification",
  "method": "session/update",                // present for requests/notifications
  "id": { "num": 3 },                        // present for requests/responses
  "hash": "blake3:d2ce79a6ae33b05c…",        // canonical-form payload hash
  "bytes": 158                               // size of the original frame
}
```

The split between the **canonical hash** (used for identity, dedup,
divergence analysis and replay matching) and the **raw bytes in the
blob** (used to reconstruct the wire stream verbatim) is what makes
replay both robust against client-side key reordering *and* byte-exact
on the agent-to-client side.

---

## Determinism

Replay is byte-exact when the recorded process itself behaves
deterministically across runs:

* Same input bytes from the client → same canonical-hash sequence on the
  c2a side → same a2c bytes emitted (since they are loaded from the
  blob store, not regenerated).
* Client object-key permutations are absorbed: two frames whose JSON
  values are equal in JCS canonical form match the same trace event.
* Mismatched client requests are reported as
  `ReplayError::Divergence` carrying a `DivergenceDetail` with both
  sides of the comparison (recorded `seq` + `method` + hash, offending
  client `method` + `id` + hash, and how many client frames had been
  consumed before the failure) and the process exits with a non-zero
  code.

The bundled `acp-echo-agent` is itself deterministic (no time, no
randomness, sessionIds derived from request count) so the E2E test
suite asserts byte-equality of recorded and replayed outputs directly.

## Limitations

This is a recorder, not a sandbox — replay fidelity inherits everything
the agent depends on. Be aware of the following:

* **The agent must be deterministic.** If the agent embeds wall-clock
  time, randomness, model temperature > 0, or `pid`/`HOSTNAME` in any
  response, two recordings of the "same" session will not match and
  replay against a re-run client will diverge. For LLM-backed agents,
  pin model version and set `temperature=0`.
* **JSON-RPC id matching is strict by default.** The replay engine
  matches client frames by canonical hash of the *whole* envelope,
  which includes `"id"`. If a recorded session used ids `1,2,3,…` and
  your live client re-numbers them `1000,1001,…`, default replay will
  report a divergence on the first frame. Pass `acp replay --remap-ids`
  to opt in to id rewriting: the engine then matches by canonical hash
  modulo `id` and rewrites outbound responses so the client sees its
  own ids back.
* **Environment is not implicitly captured.** Out of the box the
  manifest stores only `agent_argv`, plus host metadata (os/arch/cwd/
  pid) and any env vars you explicitly opt in via
  `acp record --capture-env KEY ...`. This is deliberate: a blanket
  `std::env::vars()` snapshot would routinely leak `*_API_KEY`,
  `*_TOKEN` and similar secrets into a file users may share.
* **Wall-clock timestamps in `events.jsonl`.** Each event records
  `t_wall_ns` for human inspection. By default two recordings of the
  same session produce non-identical `events.jsonl` files. This does
  **not** affect byte-exact replay (a2c bytes come from the blob
  store, not the event log). If you _do_ need bit-identical recordings
  — e.g. for golden-file tests — pass `--clock epoch` (or
  `--clock nanos:<N>`) to pin every timestamp, plus
  `--no-recording-env` to suppress the host/pid snapshot. Two such
  recordings of the same session are then `cmp`-equal on both
  `manifest.json` and `events.jsonl`.
* **Stderr is forwarded but not recorded.** Per the ACP spec stderr
  carries no protocol traffic; the proxy tees it to the parent unchanged
  but does not persist it. If you need stderr for debugging, redirect
  it yourself.
* **CRLF tolerance is one-way.** The reader accepts CRLF for
  Windows-clipboard-pasted inputs, but the writer always emits LF. ACP
  itself mandates LF; CRLF in recorded blobs would break replay byte
  equality.

---

## Measured performance

Numbers from real runs on this workspace; two profiles are reported
because they answer different questions.

### CLI throughput

`ACP_RUN_BENCH=1 cargo test --release -p acp-cli --test bench` —
includes subprocess spawn, JSON parse/serialise, blake3 hashing, disk
I/O for `events.jsonl` + blob CAS, AND stdio pipe overhead. This is
what a shell user sees.

| Workload | Value |
|---|---:|
| Session size                        | 1000 prompts (≈ 3004 ACP frames) |
| Client input                        | 107 000 bytes |
| Agent output (a2c)                  | 222 970 bytes |
| `acp record` end-to-end wall time   | 4.49 s |
| `acp replay` end-to-end wall time   | 694 ms |
| Recorded vs. replayed output        | **byte-identical** |
| Replay throughput                   | ≈ 4 300 frames/s |

### Library throughput (no subprocess, no pipes)

`ACP_RUN_BENCH=1 cargo test --release -p acp-replay --test library_bench`
— drives the engine in-process from an in-memory client stream into an
in-memory output buffer. This is what an editor plugin or test harness
sees when it embeds the engine.

| Mode | Throughput |
|---|---:|
| `replay_interactive` (3006 frames, hash-matched against synthetic client) | ≈ 5 400 frames/s |
| `replay_offline` (2003 a2c frames, sequential blob load) | ≈ 4 000 frames/s |

The library numbers aren't dramatically higher than the CLI because
the bottleneck is disk I/O for the per-frame blob loads, not subprocess
overhead. If you only need the hash sequence (e.g. for divergence
analysis without re-emitting bytes), iterate `TraceReader::events`
directly — that is a `Vec` traversal at memory speed.

---

## Test coverage

```
$ cargo test --workspace
test result: ok. 14 passed; 0 failed; …   (acp-wire unit tests)
test result: ok.  4 passed; 0 failed; …   (acp-wire JCS property tests, 200 trials each)
test result: ok.  7 passed; 0 failed; …   (acp-trace, incl. FixedClock golden tests)
test result: ok.  1 passed; 0 failed; …   (acp-proxy)
test result: ok.  7 passed; 0 failed; …   (acp-replay, incl. --remap-ids tests)
test result: ok.  2 passed; 0 failed; …   (acp-diverge)
test result: ok.  5 passed; 0 failed; …   (acp-cli e2e: real subprocess)
        Doc-tests — 2 passed                  (acp-wire, acp-replay)
```

The E2E tests spawn the actual `acp` and `acp-echo-agent` binaries,
exchange a full ACP session (`initialize` → `session/new` →
`session/prompt` ×N → `shutdown`) over real OS pipes, and assert:

* recorded ↔ replayed bytes are equal,
* a perturbed client stream is rejected with a `Divergence` error,
* key-permuted client streams are still accepted,
* `inspect` reports the right c2a / a2c counts,
* `diff` returns exit code 1 and surfaces only-A / only-B steps.

`cargo clippy --workspace --all-targets -- -D warnings` passes clean.

### Fuzz testing

The `fuzz/` directory contains [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets for `Frame::parse` and `canonicalize`:

```sh
cargo +nightly fuzz run fuzz_frame_parse
cargo +nightly fuzz run fuzz_canonicalize
```

---

## Why this exists

The official `agent-client-protocol` Rust SDK (Apache-2.0) gives you
wire types and a tokio-based runtime for clients/agents/proxies. It
does **not** give you a record/replay layer, a subagent divergence
analyser, or a content-addressed trace format. Proprietary multi-agent
coding CLIs (Grok Build runs up to eight isolated worktree subagents in
parallel; Claude Code and Codex have similar fan-out modes) offer no
external way to observe or replay their interleaving. `acp-core` fills
that gap, in pure synchronous Rust, with byte-exact reproducibility.

## License

Apache-2.0.
