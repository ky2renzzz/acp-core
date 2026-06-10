# acp-replay

Deterministic replay engine for recorded [ACP][acp] sessions.

[acp]: https://agentclientprotocol.com/

## Two modes

### Offline (`replay_offline`)

Emit every recorded A2C frame to a writer, in order. Useful for golden-output regression tests.

```rust
use acp_trace::TraceReader;
use acp_replay::replay_offline;

let trace = TraceReader::open("./recorded-session")?;
let mut out = Vec::new();
let count = replay_offline(&trace, &mut out)?;
println!("Emitted {count} frames");
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Interactive (`replay_interactive`)

Act as the agent against a live ACP client. The engine reads incoming frames, matches them by canonical hash against the recorded C2A events, and emits the corresponding A2C responses.

```rust
use std::io::Cursor;
use acp_trace::TraceReader;
use acp_replay::replay_interactive;

let trace = TraceReader::open("./recorded-session")?;
let client_input = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
let mut out = Vec::new();

let count = replay_interactive(&trace, Cursor::new(&client_input[..]), &mut out)?;
// `out` now contains the recorded responses, byte-exact
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Key properties

- **Byte-exact**: Output bytes are loaded from the blob store, not regenerated
- **Order-tolerant**: Client may reorder JSON object keys; canonical hash still matches
- **Divergence detection**: Mismatched frames produce `ReplayError::Divergence` with full context

## ID remapping

If the live client uses different JSON-RPC IDs than the recording, enable `--remap-ids`:

```rust
use acp_replay::{replay_interactive_with, ReplayOptions};

let opts = ReplayOptions { remap_ids: true };
replay_interactive_with(&trace, client_in, &mut out, &opts)?;
// Responses will have their IDs rewritten to match the client's
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

Apache-2.0
