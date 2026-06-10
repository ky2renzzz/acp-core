# acp-trace

On-disk trace format for [ACP][acp] sessions.

[acp]: https://agentclientprotocol.com/

## Trace structure

```
session/
  manifest.json       # metadata: agent argv, timestamps, event count
  events.jsonl        # append-only log, one EventRecord per line
  blobs/<aa>/<rest>   # content-addressed payloads (blake3 hash → raw bytes)
```

## Key design

- **Canonical hash** (in `events.jsonl`) — used for matching, dedup, divergence analysis
- **Raw bytes** (in `blobs/`) — used for byte-exact replay

This split lets replay tolerate client-side JSON key reordering while reproducing the original wire stream verbatim.

## Usage

```rust
use acp_trace::{TraceWriter, TraceReader, Manifest, Direction};
use acp_wire::Frame;

// Write a trace
let manifest = Manifest::new("0.1.0", vec!["my-agent".into()]);
let mut writer = TraceWriter::create("./trace", manifest)?;

let frame = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":{}}"#)?;
writer.record(Direction::C2a, &frame)?;
writer.finalize()?;

// Read it back
let reader = TraceReader::open("./trace")?;
for event in &reader.events {
    let bytes = reader.load_blob(event)?;
    println!("seq={} dir={} bytes={}", event.seq, event.dir.as_str(), bytes.len());
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Deterministic traces

Use `FixedClock` for golden-file tests:

```rust
use acp_trace::{FixedClock, Manifest, TraceWriter};

let clock = Box::new(FixedClock::epoch());
let manifest = Manifest::new_with_clock("test", vec![], clock.as_ref());
let writer = TraceWriter::create_with_clock("./trace", manifest, clock)?;
// All timestamps will be 1970-01-01T00:00:00Z
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

Apache-2.0
