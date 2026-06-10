//! Deterministic replay of recorded ACP sessions.
//!
//! Two modes:
//!
//! * [`replay_offline`] — write every `A2c` event from the trace to a writer,
//!   in original order, byte-exact. Useful for golden-output regression
//!   tests where the client side has been recorded as well.
//!
//! * [`replay_interactive`] — act as an ACP agent against a live client.
//!   The replay engine reads frames from `client_in`, expects each one to
//!   match the next recorded `C2a` event by canonical hash, and then emits
//!   every following `A2c` event up to (but not including) the next `C2a`
//!   event. Mismatches surface as [`ReplayError::Divergence`].
//!
//! "Byte-exact" here means: every byte written to the output corresponds
//! to a byte that was originally observed on the wire during recording.
//! The trailing `\n` framing is reproduced by [`acp_wire::write_frame`].

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, missing_debug_implementations)]

use std::io::{BufRead, Write};

use thiserror::Error;

use acp_trace::{Direction, EventRecord, TraceReader};
use acp_wire::{Frame, FrameReader};

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire: {0}")]
    Wire(#[from] acp_wire::WireError),
    #[error("trace: {0}")]
    Trace(#[from] acp_trace::TraceError),
    #[error("trace exhausted but client sent another frame (method={method:?})")]
    TraceExhausted { method: Option<String> },
    #[error("expected next c2a event seq={expected_seq} with hash={expected_hash}, got hash={got_hash} (method={got_method:?})")]
    Divergence {
        expected_seq: u64,
        expected_hash: String,
        got_hash: String,
        got_method: Option<String>,
    },
}

/// Offline replay: write every A2c event to `out` in trace order.
///
/// Returns the number of frames written.
pub fn replay_offline<W: Write>(trace: &TraceReader, out: &mut W) -> Result<u64, ReplayError> {
    let mut n = 0;
    for rec in &trace.events {
        if rec.dir != Direction::A2c {
            continue;
        }
        let bytes = trace.load_blob(rec)?;
        acp_wire::write_line(out, &bytes)?;
        n += 1;
    }
    Ok(n)
}

/// Interactive replay: pretend to be the agent against a live client.
///
/// `client_in` is the bytes the client writes (what the original agent
/// received). `client_out` receives what the original agent emitted.
///
/// Returns the number of frames emitted to the client.
pub fn replay_interactive<R, W>(
    trace: &TraceReader,
    client_in: R,
    mut client_out: W,
) -> Result<u64, ReplayError>
where
    R: BufRead,
    W: Write,
{
    let mut cursor = 0usize;
    let mut emitted = 0u64;

    // Emit any leading A2c events the trace begins with (rare but legal —
    // e.g. agents that send a banner notification before any client input).
    cursor = emit_until_next_c2a(trace, cursor, &mut client_out, &mut emitted)?;

    let mut reader = FrameReader::new(client_in);
    while let Some(incoming) = reader.read_frame()? {
        let expected = find_next_c2a(trace, cursor)?;
        let expected_hash = expected.1.hash.clone();
        let got_hash = incoming.payload_hash_str();
        if got_hash != expected_hash {
            return Err(ReplayError::Divergence {
                expected_seq: expected.1.seq,
                expected_hash,
                got_hash,
                got_method: incoming.method.clone(),
            });
        }
        // Advance past the matched C2a event.
        cursor = expected.0 + 1;
        // Emit all A2c events up to the next C2a event.
        cursor = emit_until_next_c2a(trace, cursor, &mut client_out, &mut emitted)?;
    }

    Ok(emitted)
}

fn find_next_c2a(trace: &TraceReader, from: usize) -> Result<(usize, &EventRecord), ReplayError> {
    for (idx, rec) in trace.events.iter().enumerate().skip(from) {
        if rec.dir == Direction::C2a {
            return Ok((idx, rec));
        }
    }
    Err(ReplayError::TraceExhausted { method: None })
}

fn emit_until_next_c2a<W: Write>(
    trace: &TraceReader,
    from: usize,
    out: &mut W,
    emitted: &mut u64,
) -> Result<usize, ReplayError> {
    let mut idx = from;
    while idx < trace.events.len() {
        let rec = &trace.events[idx];
        if rec.dir == Direction::C2a {
            break;
        }
        let bytes = trace.load_blob(rec)?;
        acp_wire::write_line(out, &bytes)?;
        *emitted += 1;
        idx += 1;
    }
    Ok(idx)
}

/// Compare two ACP frames the same way the interactive replayer does:
/// by canonical hash of their payload. Returns `true` iff equivalent.
pub fn frames_equivalent(a: &Frame, b: &Frame) -> bool {
    a.payload_hash() == b.payload_hash()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_trace::{Manifest, TraceWriter};
    use std::io::Cursor;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        d.push(format!("acp-replay-test-{name}-{nanos}"));
        d
    }

    fn build_trace(dir: &std::path::Path, frames: &[(Direction, &[u8])]) {
        let mut w = TraceWriter::create(dir, Manifest::new("0.1.0", vec!["x".into()])).unwrap();
        for (d, raw) in frames {
            let f = Frame::parse(raw).unwrap();
            w.record(*d, &f).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn offline_emits_only_a2c() {
        let dir = tmpdir("offline");
        build_trace(
            &dir,
            &[
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                ),
                (
                    Direction::A2c,
                    br#"{"jsonrpc":"2.0","id":1,"result":{"v":1}}"#,
                ),
                (
                    Direction::A2c,
                    br#"{"jsonrpc":"2.0","method":"session/update","params":{"x":1}}"#,
                ),
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#,
                ),
            ],
        );
        let r = TraceReader::open(&dir).unwrap();
        let mut out = Vec::new();
        let n = replay_offline(&r, &mut out).unwrap();
        assert_eq!(n, 2);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interactive_matches_recorded_client() {
        let dir = tmpdir("interactive-ok");
        build_trace(
            &dir,
            &[
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"a":1,"b":2}}"#,
                ),
                (
                    Direction::A2c,
                    br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
                ),
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#,
                ),
                (
                    Direction::A2c,
                    br#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#,
                ),
            ],
        );
        let r = TraceReader::open(&dir).unwrap();
        // Client sends the same logical frames, but with different key order:
        // canonical hashing must still accept them.
        let client_stream = b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1,\"params\":{\"b\":2,\"a\":1}}\n\
                              {\"jsonrpc\":\"2.0\",\"method\":\"session/new\",\"id\":2,\"params\":{}}\n";
        let mut out = Vec::new();
        let n = replay_interactive(&r, Cursor::new(&client_stream[..]), &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interactive_detects_divergence() {
        let dir = tmpdir("interactive-bad");
        build_trace(
            &dir,
            &[
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                ),
                (Direction::A2c, br#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            ],
        );
        let r = TraceReader::open(&dir).unwrap();
        let client_stream =
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"different":true}}
"#;
        let mut out = Vec::new();
        let err = replay_interactive(&r, Cursor::new(&client_stream[..]), &mut out).unwrap_err();
        assert!(matches!(err, ReplayError::Divergence { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interactive_emits_leading_a2c_before_first_client_frame() {
        let dir = tmpdir("leading-a2c");
        build_trace(
            &dir,
            &[
                (
                    Direction::A2c,
                    br#"{"jsonrpc":"2.0","method":"agent/banner","params":{"v":"1"}}"#,
                ),
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                ),
                (Direction::A2c, br#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            ],
        );
        let r = TraceReader::open(&dir).unwrap();
        let stream = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
"#;
        let mut out = Vec::new();
        let n = replay_interactive(&r, Cursor::new(&stream[..]), &mut out).unwrap();
        assert_eq!(n, 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
