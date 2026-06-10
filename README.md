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
  `ReplayError::Divergence { expected_hash, got_hash, … }` and the
  process exits with a non-zero code.

The bundled `acp-echo-agent` is itself deterministic (no time, no
randomness, sessionIds derived from request count) so the E2E test
suite asserts byte-equality of recorded and replayed outputs directly.

---

## Measured performance

Numbers from a real run on this workspace
(`ACP_RUN_BENCH=1 cargo test --release -p acp-cli --test bench`):

| Workload | Value |
|---|---:|
| Session size                        | 1000 prompts (≈ 3004 ACP frames) |
| Client input                        | 107 000 bytes |
| Agent output (a2c)                  | 222 970 bytes |
| `acp record` end-to-end wall time   | 4.49 s |
| `acp replay` end-to-end wall time   | 694 ms |
| Recorded vs. replayed output        | **byte-identical** |
| Replay throughput                   | ≈ 4 300 frames/s through the CLI |

Both numbers include subprocess spawn, JSON parse/serialise, blake3
hashing, disk I/O for events.jsonl + blob CAS, and stdio pipe overhead.

---

## Test coverage

```
$ cargo test --workspace
test result: ok. 14 passed; 0 failed; …   (acp-wire)
test result: ok.  2 passed; 0 failed; …   (acp-trace)
test result: ok.  1 passed; 0 failed; …   (acp-proxy)
test result: ok.  4 passed; 0 failed; …   (acp-replay)
test result: ok.  2 passed; 0 failed; …   (acp-diverge)
test result: ok.  5 passed; 0 failed; …   (acp-cli e2e: real subprocess)
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
