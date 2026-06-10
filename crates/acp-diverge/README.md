# acp-diverge

Divergence analysis for parallel [ACP][acp] subagent traces.

[acp]: https://agentclientprotocol.com/

## Use case

When multiple ACP agents process the same task in parallel (e.g., Grok Build's 8 isolated worktree subagents), you want to know: **where did they part ways?**

This crate aligns two traces using Longest-Common-Subsequence on `(direction, canonical_hash)` tuples and produces a diff report.

## Usage

```rust
use acp_trace::TraceReader;
use acp_diverge::{diff_two, render_unified, render_dot};

let a = TraceReader::open("./run-a")?;
let b = TraceReader::open("./run-b")?;

let report = diff_two(&a, "run-a", &b, "run-b");

// Text diff
println!("{}", render_unified(&report));

// Graphviz DOT (pipe to `dot -Tsvg`)
println!("{}", render_dot(&report));

// Programmatic access
if let Some(idx) = report.stats.first_divergence {
    eprintln!("First divergence at op #{idx}");
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Output

```
--- run-a
+++ run-b
# common=42 only_a=1 only_b=1 first_div=Some(42)
  [   0|   0] c2a initialize                              blake3:d2ce79a6…
  [   1|   1] a2c                                         blake3:8f3a1b2c…
  ...
- [  42|     ] a2c session/update                         blake3:1234abcd…
+ [     |  42] a2c session/update                         blake3:5678efgh…
```

## Complexity

O(n·m) time and memory. For traces up to ~10,000 frames, this completes in under a second. For larger traces, consider a streaming diff algorithm (the `DiffReport` structure is independent of how it was produced).

## License

Apache-2.0
