# Fuzz testing for acp-core

This directory contains fuzz targets for the acp-wire crate.

## Prerequisites

```sh
cargo install cargo-fuzz
```

Note: cargo-fuzz requires nightly Rust and Linux (or WSL on Windows).

## Running

```sh
# Fuzz Frame::parse
cargo +nightly fuzz run fuzz_frame_parse

# Fuzz canonicalize
cargo +nightly fuzz run fuzz_canonicalize

# Run for a specific duration
cargo +nightly fuzz run fuzz_frame_parse -- -max_total_time=60
```

## Targets

### `fuzz_frame_parse`

Feeds arbitrary bytes to `Frame::parse`. Checks that the function never panics — it may return `Ok` or `Err`, but must gracefully handle any input.

### `fuzz_canonicalize`

Parses arbitrary bytes as JSON, then canonicalizes. Checks:
1. `canonicalize` never panics on valid `serde_json::Value`
2. The output is valid JSON
3. Canonicalization is idempotent (canonicalize twice = same result)

## Corpus

The fuzzer will create a `corpus/` directory with interesting inputs. Commit these to improve future fuzzing runs.
