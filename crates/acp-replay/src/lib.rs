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
//!
//! # Embedding the replay engine
//!
//! The crate is intended to be driven in-process from tests or editor
//! plugins, without going through the `acp` binary. The following
//! example records a tiny session and then replays it back, asserting
//! that the replay output matches what was recorded:
//!
//! ```
//! use std::io::Cursor;
//!
//! use acp_trace::{Direction, FixedClock, Manifest, TraceReader, TraceWriter};
//! use acp_wire::Frame;
//! use acp_replay::{replay_interactive, replay_offline};
//!
//! // Build a two-frame trace synthetically.
//! let dir = std::env::temp_dir().join(format!(
//!     "acp-replay-doctest-{}",
//!     std::time::SystemTime::now()
//!         .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
//! ));
//! let clock = Box::new(FixedClock::epoch());
//! let manifest = Manifest::new_with_clock("doctest", vec!["agent".into()], clock.as_ref());
//! let mut w = TraceWriter::create_with_clock(&dir, manifest, clock).unwrap();
//! let req = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#).unwrap();
//! let res = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"result":{"pong":true}}"#).unwrap();
//! w.record(Direction::C2a, &req).unwrap();
//! w.record(Direction::A2c, &res).unwrap();
//! w.finalize().unwrap();
//!
//! // Replay it interactively against the same client stream.
//! let trace = TraceReader::open(&dir).unwrap();
//! let client_stream = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
//! let mut out = Vec::new();
//! let n = replay_interactive(&trace, Cursor::new(&client_stream[..]), &mut out).unwrap();
//! assert_eq!(n, 1);
//! assert!(out.starts_with(br#"{"jsonrpc":"2.0","id":1,"result":{"pong":true}}"#));
//!
//! // Or just emit all recorded a2c bytes in order.
//! let mut offline = Vec::new();
//! replay_offline(&trace, &mut offline).unwrap();
//! assert_eq!(offline, out);
//!
//! # std::fs::remove_dir_all(&dir).ok();
//! ```

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, missing_debug_implementations)]

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::Value;
use thiserror::Error;

use acp_trace::{Direction, EventRecord, RecordId, TraceReader};
use acp_wire::{canonicalize, Frame, FrameReader, Id, Kind};

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire: {0}")]
    Wire(#[from] acp_wire::WireError),
    #[error("trace: {0}")]
    Trace(#[from] acp_trace::TraceError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
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

/// Behaviour knobs for [`replay_interactive_with`].
#[derive(Debug, Clone, Default)]
pub struct ReplayOptions {
    /// If true, accept client requests whose JSON-RPC `id` differs from
    /// the recorded one as long as everything else hashes to the same
    /// canonical form. Outbound responses then have their `id` rewritten
    /// back to the live client's value, so the client sees the answers
    /// for the ids it sent. Notifications (no `id`) are unaffected.
    pub remap_ids: bool,
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
    client_out: W,
) -> Result<u64, ReplayError>
where
    R: BufRead,
    W: Write,
{
    replay_interactive_with(trace, client_in, client_out, &ReplayOptions::default())
}

/// Like [`replay_interactive`] but configurable via [`ReplayOptions`].
pub fn replay_interactive_with<R, W>(
    trace: &TraceReader,
    client_in: R,
    mut client_out: W,
    opts: &ReplayOptions,
) -> Result<u64, ReplayError>
where
    R: BufRead,
    W: Write,
{
    let mut cursor = 0usize;
    let mut emitted = 0u64;
    // Maps `recorded_id` -> `client_id` for requests we've matched but
    // not yet responded to. Used only when `opts.remap_ids` is on.
    let mut id_map: HashMap<Id, Id> = HashMap::new();

    cursor = emit_a2c_block(trace, cursor, &id_map, opts, &mut client_out, &mut emitted)?;

    let mut reader = FrameReader::new(client_in);
    while let Some(incoming) = reader.read_frame()? {
        let expected = find_next_c2a(trace, cursor)?;
        let expected_hash = expected.1.hash.clone();
        let got_hash = incoming.payload_hash_str();

        if got_hash == expected_hash {
            // Fast path: hashes match outright.
            cursor = expected.0 + 1;
        } else if opts.remap_ids {
            // Try to rewrite the incoming id to whatever the trace expected
            // and see if THAT version hashes equal. If so, remember the
            // mapping so we can rewrite outbound responses on the way back.
            let (expected_id, recorded_id_record) = match (&expected.1.id, &incoming.id) {
                (Some(rec_id), Some(_)) => (record_id_to_wire(rec_id), rec_id.clone()),
                _ => {
                    return Err(ReplayError::Divergence {
                        expected_seq: expected.1.seq,
                        expected_hash,
                        got_hash,
                        got_method: incoming.method.clone(),
                    });
                }
            };
            let rewritten_hash = hash_with_id(&incoming.value, &expected_id);
            if rewritten_hash == expected_hash {
                if let Some(client_id) = incoming.id.clone() {
                    id_map.insert(record_id_to_wire(&recorded_id_record), client_id);
                }
                cursor = expected.0 + 1;
            } else {
                return Err(ReplayError::Divergence {
                    expected_seq: expected.1.seq,
                    expected_hash,
                    got_hash,
                    got_method: incoming.method.clone(),
                });
            }
        } else {
            return Err(ReplayError::Divergence {
                expected_seq: expected.1.seq,
                expected_hash,
                got_hash,
                got_method: incoming.method.clone(),
            });
        }

        cursor = emit_a2c_block(trace, cursor, &id_map, opts, &mut client_out, &mut emitted)?;
    }

    Ok(emitted)
}

fn record_id_to_wire(r: &RecordId) -> Id {
    match r {
        RecordId::Num(n) => Id::Num(*n),
        RecordId::Str(s) => Id::Str(s.clone()),
        RecordId::Null => Id::Null,
    }
}

/// Canonical hash of `value` with the top-level `"id"` field replaced.
fn hash_with_id(value: &Value, new_id: &Id) -> String {
    let mut clone = value.clone();
    if let Value::Object(map) = &mut clone {
        if map.contains_key("id") {
            map.insert("id".to_string(), new_id.to_value());
        }
    }
    let bytes = canonicalize(&clone);
    hex_with_prefix(&blake3::hash(&bytes).as_bytes()[..])
}

fn hex_with_prefix(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(7 + bytes.len() * 2);
    s.push_str("blake3:");
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Emit every A2c event from `from` up to the next C2a event, rewriting
/// response ids on the fly when `remap_ids` is enabled. Returns the
/// new cursor position.
fn emit_a2c_block<W: Write>(
    trace: &TraceReader,
    from: usize,
    id_map: &HashMap<Id, Id>,
    opts: &ReplayOptions,
    out: &mut W,
    emitted: &mut u64,
) -> Result<usize, ReplayError> {
    let mut idx = from;
    while idx < trace.events.len() {
        let rec = &trace.events[idx];
        if rec.dir == Direction::C2a {
            break;
        }
        if opts.remap_ids && rec.kind == acp_trace::RecordKind::Response {
            // Response: must remap id back to the client's value.
            let bytes = trace.load_blob(rec)?;
            let frame = Frame::parse(&bytes)?;
            let mapped = if frame.kind == Kind::Response {
                if let Some(recorded_id) = frame.id.clone() {
                    if let Some(client_id) = id_map.get(&recorded_id) {
                        rewrite_id_in_value(&frame.value, client_id)?
                    } else {
                        // Unknown id — emit verbatim.
                        bytes
                    }
                } else {
                    bytes
                }
            } else {
                bytes
            };
            acp_wire::write_line(out, &mapped)?;
        } else {
            let bytes = trace.load_blob(rec)?;
            acp_wire::write_line(out, &bytes)?;
        }
        *emitted += 1;
        idx += 1;
    }
    Ok(idx)
}

fn rewrite_id_in_value(value: &Value, new_id: &Id) -> Result<Vec<u8>, ReplayError> {
    let mut clone = value.clone();
    if let Value::Object(map) = &mut clone {
        map.insert("id".to_string(), new_id.to_value());
    }
    Ok(serde_json::to_vec(&clone)?)
}

fn find_next_c2a(trace: &TraceReader, from: usize) -> Result<(usize, &EventRecord), ReplayError> {
    for (idx, rec) in trace.events.iter().enumerate().skip(from) {
        if rec.dir == Direction::C2a {
            return Ok((idx, rec));
        }
    }
    Err(ReplayError::TraceExhausted { method: None })
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

    #[test]
    fn remap_ids_accepts_client_with_different_numbering() {
        // Trace was recorded with ids 1, 2. Live client uses 1000, 1001.
        // With remap_ids ON, replay accepts the session AND rewrites the
        // outbound responses so the client sees its own ids back.
        let dir = tmpdir("remap-ok");
        build_trace(
            &dir,
            &[
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
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
        let client_stream = b"{\"jsonrpc\":\"2.0\",\"id\":1000,\"method\":\"initialize\",\"params\":{}}\n\
                              {\"jsonrpc\":\"2.0\",\"id\":1001,\"method\":\"session/new\",\"params\":{}}\n";
        let mut out = Vec::new();
        let n = replay_interactive_with(
            &r,
            Cursor::new(&client_stream[..]),
            &mut out,
            &ReplayOptions { remap_ids: true },
        )
        .unwrap();
        assert_eq!(n, 2);
        let text = String::from_utf8(out).unwrap();
        // The client sent 1000 / 1001; responses must carry 1000 / 1001,
        // not the recorded 1 / 2.
        assert!(text.contains("\"id\":1000"), "want id 1000 in: {text}");
        assert!(text.contains("\"id\":1001"), "want id 1001 in: {text}");
        assert!(
            !text.contains("\"id\":1,"),
            "should NOT contain recorded id 1: {text}"
        );
        assert!(
            !text.contains("\"id\":2,"),
            "should NOT contain recorded id 2: {text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remap_ids_off_still_rejects_renumbered_client() {
        // Same trace as above, but remap_ids OFF: must diverge on first frame.
        let dir = tmpdir("remap-off");
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
        let bad = b"{\"jsonrpc\":\"2.0\",\"id\":999,\"method\":\"initialize\",\"params\":{}}\n";
        let mut out = Vec::new();
        let err = replay_interactive(&r, Cursor::new(&bad[..]), &mut out).unwrap_err();
        assert!(matches!(err, ReplayError::Divergence { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remap_ids_rejects_truly_different_payloads() {
        // Even with remap_ids ON, divergence on non-id fields must still fail.
        let dir = tmpdir("remap-real-div");
        build_trace(
            &dir,
            &[
                (
                    Direction::C2a,
                    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"v":1}}"#,
                ),
                (Direction::A2c, br#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            ],
        );
        let r = TraceReader::open(&dir).unwrap();
        // Different `v` in params: remap_ids cannot rescue this.
        let bad =
            b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"initialize\",\"params\":{\"v\":2}}\n";
        let mut out = Vec::new();
        let err = replay_interactive_with(
            &r,
            Cursor::new(&bad[..]),
            &mut out,
            &ReplayOptions { remap_ids: true },
        )
        .unwrap_err();
        assert!(matches!(err, ReplayError::Divergence { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }
}
