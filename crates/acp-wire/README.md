# acp-wire

JSON-RPC 2.0 wire format for the [Agent Client Protocol][acp] (ACP).

[acp]: https://agentclientprotocol.com/

## Features

- **Newline-delimited framing** — parse and emit `\n`-terminated JSON-RPC messages
- **Frame classification** — `Request`, `Response`, `Notification`
- **RFC 8785 JSON Canonicalization (JCS)** — deterministic JSON for content-addressing
- **blake3 payload hashing** — stable hash even when object keys are reordered

## Usage

```rust
use acp_wire::{Frame, FrameReader, payload_hash};
use std::io::Cursor;

// Parse a single frame
let frame = Frame::parse(
    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
).unwrap();
assert_eq!(frame.method.as_deref(), Some("initialize"));

// Streaming reader
let input = br#"{"jsonrpc":"2.0","id":1,"method":"x","params":{}}
"#;
let mut reader = FrameReader::new(Cursor::new(&input[..]));
while let Some(frame) = reader.read_frame().unwrap() {
    println!("{}: {}", frame.kind.as_str(), frame.payload_hash_str());
}
```

## Key invariant

Two JSON objects that differ only in key order produce **identical** canonical hashes:

```rust
use acp_wire::payload_hash;
use serde_json::json;

let a = payload_hash(&json!({"a":1,"b":2}));
let b = payload_hash(&json!({"b":2,"a":1}));
assert_eq!(a, b);
```

This is the foundation of the entire acp-core project: recorded sessions tolerate client-side key reordering while replay remains byte-exact.

## License

Apache-2.0
