//! On-disk trace format for ACP sessions.
//!
//! A trace lives in a directory and consists of three things:
//!
//! ```text
//! <trace>/
//!   manifest.json      JSON describing the recording: agent, argv, times, counts
//!   events.jsonl       Append-only newline-delimited log, one [`EventRecord`] per line
//!   blobs/<aa>/<rest>  Content-addressed payloads. Filename = blake3 hex of canonical form.
//!                      File contents = the EXACT original frame bytes (no trailing \n),
//!                      so replay can reproduce the wire stream byte-for-byte even when
//!                      two semantically-equal frames had different key orderings.
//! ```
//!
//! The split between *canonical hash* (used for identity, dedup and divergence
//! analysis) and *raw bytes* (used for byte-exact replay) is deliberate: it
//! lets the same trace serve both regression-testing and forensic-debugging
//! workflows.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, missing_debug_implementations)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use acp_wire::{Frame, Id, Kind};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum TraceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("wire: {0}")]
    Wire(#[from] acp_wire::WireError),
    #[error("trace at {0:?} is missing required file: {1}")]
    Missing(PathBuf, &'static str),
    #[error("hash mismatch reading blob {expected}: got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("event references blob {0} which is not present in the trace")]
    DanglingBlob(String),
    #[error("trace format version mismatch: file is v{got}, this build supports v{want}")]
    Version { got: u32, want: u32 },
}

/// Direction of a recorded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Client → Agent.
    C2a,
    /// Agent → Client.
    A2c,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::C2a => "c2a",
            Direction::A2c => "a2c",
        }
    }
}

/// Wire-level frame kind, redeclared here so we can `Serialize` it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Request,
    Response,
    Notification,
}

impl From<Kind> for RecordKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Request => RecordKind::Request,
            Kind::Response => RecordKind::Response,
            Kind::Notification => RecordKind::Notification,
        }
    }
}

/// JSON-RPC id, serializable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordId {
    Num(i64),
    Str(String),
    Null,
}

impl From<&Id> for RecordId {
    fn from(id: &Id) -> Self {
        match id {
            Id::Num(n) => RecordId::Num(*n),
            Id::Str(s) => RecordId::Str(s.clone()),
            Id::Null => RecordId::Null,
        }
    }
}

/// One row in `events.jsonl`. Lightweight — payload lives in `blobs/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Monotonic position in the event log, starting at 0.
    pub seq: u64,
    /// Wall-clock nanoseconds since Unix epoch when the frame was observed.
    pub t_wall_ns: u128,
    /// Direction across the proxy.
    pub dir: Direction,
    /// Frame classification.
    pub kind: RecordKind,
    /// `method` for requests/notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// `id` for requests/responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RecordId>,
    /// `blake3:<hex>` of the canonicalized payload.
    pub hash: String,
    /// Size of the original frame in bytes (without trailing newline).
    pub bytes: u64,
}

/// Top-level recording metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub recorder: String,
    pub recorder_version: String,
    pub agent_argv: Vec<String>,
    /// RFC 3339 timestamp.
    pub started_at: String,
    /// RFC 3339 timestamp, populated by [`TraceWriter::finalize`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Number of events at finalize time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_count: Option<u64>,
    /// blake3 of `events.jsonl` after finalization, hex with `blake3:` prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_hash: Option<String>,
}

impl Manifest {
    pub fn new(recorder_version: &str, agent_argv: Vec<String>) -> Self {
        Manifest {
            format_version: FORMAT_VERSION,
            recorder: "acp-core".into(),
            recorder_version: recorder_version.into(),
            agent_argv,
            started_at: now_rfc3339(),
            ended_at: None,
            event_count: None,
            events_hash: None,
        }
    }
}

fn now_rfc3339() -> String {
    use time::OffsetDateTime;
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn now_unix_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Append-only trace writer. Frames are persisted in two places per call to
/// [`TraceWriter::record`]:
///
/// 1. An [`EventRecord`] is appended to `events.jsonl`.
/// 2. The frame's raw bytes are written to `blobs/<aa>/<rest>` keyed by
///    canonical hash. Duplicate hashes are written once (file is created
///    only if absent), which dedups identical prompts across subagents.
#[derive(Debug)]
pub struct TraceWriter {
    root: PathBuf,
    blobs: PathBuf,
    events: BufWriter<File>,
    manifest: Manifest,
    next_seq: u64,
}

impl TraceWriter {
    pub fn create(root: impl Into<PathBuf>, manifest: Manifest) -> Result<Self, TraceError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let blobs = root.join("blobs");
        fs::create_dir_all(&blobs)?;

        // Write initial manifest so a crashed recording is still inspectable.
        let manifest_path = root.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        let events_path = root.join("events.jsonl");
        let events_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&events_path)?;
        let events = BufWriter::new(events_file);

        Ok(Self {
            root,
            blobs,
            events,
            manifest,
            next_seq: 0,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persist one frame. The hash returned identifies the payload blob.
    pub fn record(&mut self, dir: Direction, frame: &Frame) -> Result<String, TraceError> {
        let hash_bytes = frame.payload_hash();
        let hash = format_blake3_hex(&hash_bytes);
        let hash_str = format!("blake3:{hash}");

        // CAS write: create-new only, ignore "already exists".
        let (subdir, file_name) = hash.split_at(2);
        let dir_path = self.blobs.join(subdir);
        fs::create_dir_all(&dir_path)?;
        let blob_path = dir_path.join(file_name);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&blob_path)
        {
            Ok(mut f) => {
                f.write_all(&frame.raw)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // dedup hit
            }
            Err(e) => return Err(TraceError::Io(e)),
        }

        let record = EventRecord {
            seq: self.next_seq,
            t_wall_ns: now_unix_nanos(),
            dir,
            kind: RecordKind::from(frame.kind),
            method: frame.method.clone(),
            id: frame.id.as_ref().map(RecordId::from),
            hash: hash_str.clone(),
            bytes: frame.raw.len() as u64,
        };
        let line = serde_json::to_vec(&record)?;
        self.events.write_all(&line)?;
        self.events.write_all(b"\n")?;
        self.events.flush()?;

        self.next_seq += 1;
        Ok(hash_str)
    }

    /// Close the trace, updating manifest with event count, end time and
    /// the blake3 hash of `events.jsonl`.
    pub fn finalize(mut self) -> Result<(), TraceError> {
        self.events.flush()?;
        drop(self.events);

        let events_path = self.root.join("events.jsonl");
        let events_bytes = fs::read(&events_path)?;
        let events_hash = format!(
            "blake3:{}",
            format_blake3_hex(blake3::hash(&events_bytes).as_bytes())
        );

        self.manifest.ended_at = Some(now_rfc3339());
        self.manifest.event_count = Some(self.next_seq);
        self.manifest.events_hash = Some(events_hash);

        let manifest_path = self.root.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&self.manifest)?)?;
        Ok(())
    }
}

/// Read-only access to a recorded trace.
#[derive(Debug)]
pub struct TraceReader {
    root: PathBuf,
    pub manifest: Manifest,
    pub events: Vec<EventRecord>,
}

impl TraceReader {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, TraceError> {
        let root = root.into();
        let manifest_path = root.join("manifest.json");
        if !manifest_path.exists() {
            return Err(TraceError::Missing(root.clone(), "manifest.json"));
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(TraceError::Version {
                got: manifest.format_version,
                want: FORMAT_VERSION,
            });
        }

        let events_path = root.join("events.jsonl");
        if !events_path.exists() {
            return Err(TraceError::Missing(root.clone(), "events.jsonl"));
        }
        let file = File::open(&events_path)?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            let rec: EventRecord = serde_json::from_str(trimmed)?;
            events.push(rec);
        }

        Ok(Self {
            root,
            manifest,
            events,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load the raw frame bytes for an event from the blob store.
    /// Verifies that the stored bytes hash to the expected canonical value
    /// — this catches corruption and accidental edits.
    pub fn load_blob(&self, rec: &EventRecord) -> Result<Vec<u8>, TraceError> {
        let hex = rec
            .hash
            .strip_prefix("blake3:")
            .ok_or_else(|| TraceError::DanglingBlob(rec.hash.clone()))?;
        if hex.len() < 2 {
            return Err(TraceError::DanglingBlob(rec.hash.clone()));
        }
        let (subdir, file_name) = hex.split_at(2);
        let path = self.root.join("blobs").join(subdir).join(file_name);
        if !path.exists() {
            return Err(TraceError::DanglingBlob(rec.hash.clone()));
        }
        let bytes = fs::read(&path)?;
        // Re-derive canonical hash to verify integrity.
        let frame = Frame::parse(&bytes)?;
        let got = format!("blake3:{}", format_blake3_hex(&frame.payload_hash()));
        if got != rec.hash {
            return Err(TraceError::HashMismatch {
                expected: rec.hash.clone(),
                got,
            });
        }
        Ok(bytes)
    }
}

fn format_blake3_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_wire::Frame;

    fn tmpdir(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        d.push(format!("acp-trace-test-{name}-{nanos}"));
        d
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tmpdir("roundtrip");
        let manifest = Manifest::new("0.1.0-test", vec!["grok".into(), "build".into()]);
        let mut w = TraceWriter::create(&dir, manifest).unwrap();

        let f1 = Frame::parse(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .unwrap();
        let f2 =
            Frame::parse(br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#).unwrap();
        let f3 = Frame::parse(br#"{"jsonrpc":"2.0","method":"session/update","params":{"x":1}}"#)
            .unwrap();
        w.record(Direction::C2a, &f1).unwrap();
        w.record(Direction::A2c, &f2).unwrap();
        w.record(Direction::A2c, &f3).unwrap();
        w.finalize().unwrap();

        let r = TraceReader::open(&dir).unwrap();
        assert_eq!(r.events.len(), 3);
        assert_eq!(r.events[0].dir, Direction::C2a);
        assert_eq!(r.events[0].method.as_deref(), Some("initialize"));
        assert_eq!(r.events[1].kind, RecordKind::Response);
        assert_eq!(r.events[2].kind, RecordKind::Notification);
        assert_eq!(r.manifest.event_count, Some(3));
        assert!(r
            .manifest
            .events_hash
            .as_ref()
            .unwrap()
            .starts_with("blake3:"));

        // Reload original bytes byte-exactly.
        let blob1 = r.load_blob(&r.events[0]).unwrap();
        assert_eq!(blob1, f1.raw);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dedup_identical_payloads() {
        let dir = tmpdir("dedup");
        let mut w = TraceWriter::create(&dir, Manifest::new("0.1.0", vec![])).unwrap();
        let f = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"a":1}}"#).unwrap();
        w.record(Direction::C2a, &f).unwrap();
        w.record(Direction::C2a, &f).unwrap();
        w.finalize().unwrap();

        let r = TraceReader::open(&dir).unwrap();
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.events[0].hash, r.events[1].hash);
        // Both events reference the same blob.
        let b1 = r.load_blob(&r.events[0]).unwrap();
        let b2 = r.load_blob(&r.events[1]).unwrap();
        assert_eq!(b1, b2);

        fs::remove_dir_all(&dir).ok();
    }
}
