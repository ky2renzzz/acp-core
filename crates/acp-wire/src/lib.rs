//! ACP wire format: newline-delimited JSON-RPC 2.0 over stdio.
//!
//! Per the ACP transport spec, messages are individual JSON-RPC messages
//! delimited by `\n` and MUST NOT contain embedded newlines. All payloads
//! are UTF-8. This crate provides:
//!
//! * [`Frame`] — a parsed message classified as [`Kind::Request`],
//!   [`Kind::Response`] or [`Kind::Notification`], with the original
//!   bytes preserved verbatim.
//! * [`FrameReader`] / [`write_frame`] — newline-delimited I/O.
//! * [`canonicalize`] — RFC 8785 JSON Canonicalization Scheme.
//! * [`payload_hash`] — blake3 hash of a canonicalized payload.
//!
//! No tokio, no async — the type is pure data and the I/O is plain
//! `BufRead` / `Write`. This keeps the crate dependency-light and lets
//! callers pick their own runtime (the proxy uses raw OS threads).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, missing_debug_implementations)]

use std::io::{BufRead, Write};

use serde_json::Value;
use thiserror::Error;

pub mod canonical;
pub use canonical::canonicalize;

/// JSON-RPC message classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `method` is present and `id` is present.
    Request,
    /// `method` is present and `id` is absent.
    Notification,
    /// `method` is absent and either `result` or `error` is present.
    Response,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Request => "request",
            Kind::Notification => "notification",
            Kind::Response => "response",
        }
    }
}

/// JSON-RPC id: spec allows string, integer or null.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    Num(i64),
    Str(String),
    Null,
}

impl Id {
    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(Id::Num),
            Value::String(s) => Some(Id::Str(s.clone())),
            Value::Null => Some(Id::Null),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Id::Num(n) => Value::from(*n),
            Id::Str(s) => Value::String(s.clone()),
            Id::Null => Value::Null,
        }
    }
}

/// A parsed JSON-RPC frame with original bytes preserved.
#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: Kind,
    /// `method` for requests/notifications, `None` for responses.
    pub method: Option<String>,
    /// `id` for requests/responses, `None` for notifications.
    pub id: Option<Id>,
    /// Parsed JSON value (the entire envelope).
    pub value: Value,
    /// Original UTF-8 bytes, WITHOUT the trailing newline.
    pub raw: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum WireError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid utf-8 in frame: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a JSON-RPC 2.0 message: missing or wrong `jsonrpc` field")]
    NotJsonRpc,
    #[error("frame contains embedded newline, which ACP forbids")]
    EmbeddedNewline,
    #[error("frame is neither request, response nor notification")]
    UnclassifiableFrame,
    #[error("unexpected end of stream")]
    Eof,
}

impl Frame {
    /// Parse a single frame from one UTF-8 line (no trailing newline expected).
    pub fn parse(line: &[u8]) -> Result<Self, WireError> {
        if line.contains(&b'\n') {
            return Err(WireError::EmbeddedNewline);
        }
        let value: Value = serde_json::from_slice(line)?;
        let obj = value.as_object().ok_or(WireError::UnclassifiableFrame)?;

        // jsonrpc field per JSON-RPC 2.0
        match obj.get("jsonrpc") {
            Some(Value::String(s)) if s == "2.0" => {}
            _ => return Err(WireError::NotJsonRpc),
        }

        let method = obj.get("method").and_then(|m| m.as_str()).map(String::from);
        let id = obj.get("id").and_then(Id::from_value);
        let has_result = obj.contains_key("result");
        let has_error = obj.contains_key("error");

        let kind = match (method.is_some(), id.is_some(), has_result || has_error) {
            (true, true, false) => Kind::Request,
            (true, false, false) => Kind::Notification,
            (false, _, true) => Kind::Response,
            _ => return Err(WireError::UnclassifiableFrame),
        };

        Ok(Frame {
            kind,
            method,
            id,
            value,
            raw: line.to_vec(),
        })
    }

    /// blake3 hash of the canonicalized JSON payload (stable across key order).
    pub fn payload_hash(&self) -> [u8; 32] {
        payload_hash(&self.value)
    }

    /// blake3 hash hex-encoded with `"blake3:"` prefix.
    pub fn payload_hash_str(&self) -> String {
        hex_with_prefix(&self.payload_hash())
    }
}

/// blake3 over the RFC 8785 canonical form of `v`.
pub fn payload_hash(v: &Value) -> [u8; 32] {
    let bytes = canonicalize(v);
    *blake3::hash(&bytes).as_bytes()
}

fn hex_with_prefix(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(7 + bytes.len() * 2);
    s.push_str("blake3:");
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Streaming reader for newline-delimited ACP frames.
#[derive(Debug)]
pub struct FrameReader<R: BufRead> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(4096),
        }
    }

    /// Read the next frame. Returns `Ok(None)` on clean EOF.
    pub fn read_frame(&mut self) -> Result<Option<Frame>, WireError> {
        self.buf.clear();
        let n = self.inner.read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }
        // Strip exactly one trailing \n (and an optional preceding \r for tolerance).
        let mut end = self.buf.len();
        if end > 0 && self.buf[end - 1] == b'\n' {
            end -= 1;
            if end > 0 && self.buf[end - 1] == b'\r' {
                end -= 1;
            }
        } else {
            // No trailing newline — last frame at EOF without delimiter is tolerated.
        }
        if end == 0 {
            // Empty line — ACP doesn't define one; skip and try the next.
            return self.read_frame();
        }
        let frame = Frame::parse(&self.buf[..end])?;
        Ok(Some(frame))
    }
}

/// Write a frame followed by `\n` and flush. The bytes written are EXACTLY
/// `frame.raw` followed by a single LF — preserving byte-exact replay.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), WireError> {
    if frame.raw.contains(&b'\n') {
        return Err(WireError::EmbeddedNewline);
    }
    w.write_all(&frame.raw)?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

/// Write a raw line. Caller guarantees no embedded newline.
pub fn write_line<W: Write>(w: &mut W, line: &[u8]) -> Result<(), WireError> {
    if line.contains(&b'\n') {
        return Err(WireError::EmbeddedNewline);
    }
    w.write_all(line)?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn req(id: i64, method: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{}}}}"#)
    }

    #[test]
    fn parse_request_response_notification() {
        let r = Frame::parse(req(1, "initialize").as_bytes()).unwrap();
        assert_eq!(r.kind, Kind::Request);
        assert_eq!(r.method.as_deref(), Some("initialize"));
        assert_eq!(r.id, Some(Id::Num(1)));

        let n = Frame::parse(br#"{"jsonrpc":"2.0","method":"session/update","params":{"x":1}}"#)
            .unwrap();
        assert_eq!(n.kind, Kind::Notification);
        assert_eq!(n.id, None);

        let resp = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(resp.kind, Kind::Response);
        assert_eq!(resp.id, Some(Id::Num(1)));

        let err =
            Frame::parse(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad"}}"#)
                .unwrap();
        assert_eq!(err.kind, Kind::Response);
    }

    #[test]
    fn reject_non_jsonrpc() {
        let err = Frame::parse(br#"{"id":1,"method":"x"}"#);
        assert!(matches!(err, Err(WireError::NotJsonRpc)));
    }

    #[test]
    fn reject_embedded_newline() {
        let err = Frame::parse(b"{\"jsonrpc\":\"2.0\"\n,\"id\":1,\"method\":\"x\"}");
        assert!(matches!(err, Err(WireError::EmbeddedNewline)));
    }

    #[test]
    fn reader_streams_multiple_frames() {
        let stream = format!("{}\n{}\n", req(1, "a"), req(2, "b"));
        let mut r = FrameReader::new(Cursor::new(stream));
        let f1 = r.read_frame().unwrap().unwrap();
        let f2 = r.read_frame().unwrap().unwrap();
        assert_eq!(f1.method.as_deref(), Some("a"));
        assert_eq!(f2.method.as_deref(), Some("b"));
        assert!(r.read_frame().unwrap().is_none());
    }

    #[test]
    fn reader_tolerates_crlf() {
        let stream = format!("{}\r\n{}\r\n", req(1, "a"), req(2, "b"));
        let mut r = FrameReader::new(Cursor::new(stream));
        assert_eq!(
            r.read_frame().unwrap().unwrap().method.as_deref(),
            Some("a")
        );
        assert_eq!(
            r.read_frame().unwrap().unwrap().method.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn payload_hash_is_order_independent() {
        let a = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"a":1,"b":2}}"#)
            .unwrap();
        let b = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"b":2,"a":1}}"#)
            .unwrap();
        assert_eq!(a.payload_hash(), b.payload_hash());
        assert_eq!(a.payload_hash_str(), b.payload_hash_str());
        assert!(a.payload_hash_str().starts_with("blake3:"));
    }

    #[test]
    fn write_then_read_roundtrip() {
        let frame = Frame::parse(req(42, "session/prompt").as_bytes()).unwrap();
        let mut out = Vec::new();
        write_frame(&mut out, &frame).unwrap();
        assert_eq!(*out.last().unwrap(), b'\n');
        let mut r = FrameReader::new(Cursor::new(out));
        let back = r.read_frame().unwrap().unwrap();
        assert_eq!(back.raw, frame.raw);
        assert_eq!(back.payload_hash(), frame.payload_hash());
    }
}
