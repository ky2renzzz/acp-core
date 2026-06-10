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

/// Host / process context captured at recording time.
///
/// Persisted under `Manifest::recording_env`. Everything is optional so a
/// recorder MAY omit it (e.g. for redacted public traces), and so traces
/// produced by older builds keep deserializing cleanly. Environment
/// variables are only included by an explicit caller-supplied whitelist:
/// raw `std::env::vars()` would routinely leak `*_API_KEY`, `*_TOKEN`,
/// `AWS_SECRET_*` and similar credentials into a file the user may share.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordingEnv {
    /// Working directory of the agent process at spawn time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// `std::env::consts::OS` of the recording host (e.g. `"linux"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_os: Option<String>,
    /// `std::env::consts::ARCH` of the recording host (e.g. `"x86_64"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_arch: Option<String>,
    /// OS process id of the recorder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorder_pid: Option<u32>,
    /// Whitelisted environment variables passed to the agent. Keys not
    /// supplied in the whitelist are NOT captured — secrets stay out.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

impl RecordingEnv {
    /// Capture host metadata and the listed env vars from the current process.
    /// Missing vars are silently skipped. Pass an empty slice to capture no
    /// env at all (host metadata still populated).
    pub fn capture(env_whitelist: &[&str]) -> Self {
        let mut env = std::collections::BTreeMap::new();
        for key in env_whitelist {
            if let Ok(v) = std::env::var(key) {
                env.insert((*key).to_string(), v);
            }
        }
        RecordingEnv {
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
            host_os: Some(std::env::consts::OS.to_string()),
            host_arch: Some(std::env::consts::ARCH.to_string()),
            recorder_pid: Some(std::process::id()),
            env,
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Number of events at finalize time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_count: Option<u64>,
    /// blake3 of `events.jsonl` after finalization, hex with `blake3:` prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_hash: Option<String>,
    /// Host & process context at record time. See [`RecordingEnv`].
    /// `None` for older traces; produced by recorders that called
    /// [`Manifest::with_recording_env`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_env: Option<RecordingEnv>,
}

impl Manifest {
    /// Create a new manifest using the system clock for `started_at`.
    pub fn new(recorder_version: &str, agent_argv: Vec<String>) -> Self {
        Self::new_with_clock(recorder_version, agent_argv, &SystemClock)
    }

    /// Create a new manifest, sourcing the start timestamp from `clock`.
    /// Use a [`FixedClock`] (or any other [`Clock`] impl) when you need
    /// bit-identical `events.jsonl` across runs, e.g. for golden tests.
    pub fn new_with_clock(
        recorder_version: &str,
        agent_argv: Vec<String>,
        clock: &dyn Clock,
    ) -> Self {
        Manifest {
            format_version: FORMAT_VERSION,
            recorder: "acp-core".into(),
            recorder_version: recorder_version.into(),
            agent_argv,
            started_at: clock.rfc3339_now(),
            ended_at: None,
            event_count: None,
            events_hash: None,
            recording_env: None,
        }
    }

    /// Attach a captured environment snapshot to this manifest.
    pub fn with_recording_env(mut self, env: RecordingEnv) -> Self {
        self.recording_env = Some(env);
        self
    }
}

/// Source of timestamps used while recording.
///
/// `acp-trace` calls a `Clock` exactly twice per event (`unix_nanos` for
/// the event's `t_wall_ns`) plus once for `Manifest::started_at` /
/// `Manifest::ended_at`. Replacing the system clock with a
/// [`FixedClock`] makes the produced `manifest.json` and `events.jsonl`
/// bit-identical across runs — which is exactly what you want when
/// asserting golden files in tests.
pub trait Clock: std::fmt::Debug + Send + Sync {
    /// Wall-clock nanoseconds since the Unix epoch.
    fn unix_nanos(&self) -> u128;
    /// RFC 3339 string matching [`Self::unix_nanos`].
    fn rfc3339_now(&self) -> String;
}

/// Real-time clock backed by [`std::time::SystemTime`]. This is the
/// default; you only need to think about clocks when you want
/// determinism.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_nanos(&self) -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }
    fn rfc3339_now(&self) -> String {
        use time::OffsetDateTime;
        OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
    }
}

/// Clock that always returns the same instant. Useful for golden tests
/// where you want `manifest.json` and `events.jsonl` to be bit-identical
/// across runs. Construct with [`FixedClock::epoch`] for the Unix epoch
/// or [`FixedClock::at_nanos`] for a custom point.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock {
    nanos: u128,
}

impl FixedClock {
    /// A clock pinned to the Unix epoch (`1970-01-01T00:00:00Z`).
    pub const fn epoch() -> Self {
        FixedClock { nanos: 0 }
    }

    /// A clock pinned to `nanos` nanoseconds since the Unix epoch.
    pub const fn at_nanos(nanos: u128) -> Self {
        FixedClock { nanos }
    }
}

impl Clock for FixedClock {
    fn unix_nanos(&self) -> u128 {
        self.nanos
    }
    fn rfc3339_now(&self) -> String {
        use time::OffsetDateTime;
        // u128 nanos → seconds + nanos. We saturate at i64::MAX seconds
        // (well past year 2200) which is far beyond any realistic test.
        let secs = (self.nanos / 1_000_000_000).min(i64::MAX as u128) as i64;
        let nanos = (self.nanos % 1_000_000_000) as u32;
        OffsetDateTime::from_unix_timestamp(secs)
            .and_then(|t| t.replace_nanosecond(nanos))
            .ok()
            .and_then(|t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
    }
}

/// Append-only trace writer. Frames are persisted in two places per call to
/// [`TraceWriter::record`]:
///
/// 1. An [`EventRecord`] is appended to `events.jsonl`.
/// 2. The frame's raw bytes are written to `blobs/<aa>/<rest>` keyed by
///    canonical hash. Duplicate hashes are written once (file is created
///    only if absent), which dedups identical prompts across subagents.
pub struct TraceWriter {
    root: PathBuf,
    blobs: PathBuf,
    events: BufWriter<File>,
    manifest: Manifest,
    next_seq: u64,
    clock: Box<dyn Clock>,
}

impl std::fmt::Debug for TraceWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceWriter")
            .field("root", &self.root)
            .field("next_seq", &self.next_seq)
            .finish_non_exhaustive()
    }
}

impl TraceWriter {
    /// Create a new trace writer using the system clock.
    pub fn create(root: impl Into<PathBuf>, manifest: Manifest) -> Result<Self, TraceError> {
        Self::create_with_clock(root, manifest, Box::new(SystemClock))
    }

    /// Create a new trace writer with an injected clock. Pass a
    /// [`FixedClock`] to make timestamps reproducible.
    pub fn create_with_clock(
        root: impl Into<PathBuf>,
        manifest: Manifest,
        clock: Box<dyn Clock>,
    ) -> Result<Self, TraceError> {
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
            clock,
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
            t_wall_ns: self.clock.unix_nanos(),
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

        self.manifest.ended_at = Some(self.clock.rfc3339_now());
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

    #[test]
    fn manifest_without_recording_env_is_readable() {
        // Old recorders never wrote `recording_env`. New readers must accept
        // a manifest where the field is missing, defaulting it to `None`.
        let legacy = r#"{
            "format_version": 1,
            "recorder": "acp-core",
            "recorder_version": "0.0.1",
            "agent_argv": ["legacy-agent"],
            "started_at": "2026-01-01T00:00:00Z"
        }"#;
        let m: Manifest = serde_json::from_str(legacy).expect("forward-compat parse");
        assert!(m.recording_env.is_none());
        assert!(m.ended_at.is_none());
    }

    #[test]
    fn recording_env_captures_host_and_whitelisted_vars() {
        // SAFETY: cargo runs tests in parallel by default, but each test
        // owns a unique env-var name so there is no cross-test interference.
        std::env::set_var("ACP_TEST_SECRET", "should-not-leak");
        std::env::set_var("ACP_TEST_PUBLIC", "captured-ok");

        let env = RecordingEnv::capture(&["ACP_TEST_PUBLIC", "ACP_TEST_MISSING"]);
        assert_eq!(env.host_os.as_deref(), Some(std::env::consts::OS));
        assert_eq!(env.host_arch.as_deref(), Some(std::env::consts::ARCH));
        assert!(env.recorder_pid.is_some());
        assert!(env.cwd.is_some());

        // Whitelist works: present key captured, missing key skipped,
        // non-whitelisted secret NEVER captured even though it's in env.
        assert_eq!(
            env.env.get("ACP_TEST_PUBLIC").map(String::as_str),
            Some("captured-ok")
        );
        assert!(!env.env.contains_key("ACP_TEST_MISSING"));
        assert!(!env.env.contains_key("ACP_TEST_SECRET"));

        std::env::remove_var("ACP_TEST_SECRET");
        std::env::remove_var("ACP_TEST_PUBLIC");
    }

    #[test]
    fn manifest_with_recording_env_round_trips() {
        let dir = tmpdir("env-rt");
        let env = RecordingEnv {
            cwd: Some("/work".into()),
            host_os: Some("linux".into()),
            host_arch: Some("x86_64".into()),
            recorder_pid: Some(4242),
            env: [("PATH".to_string(), "/usr/bin".to_string())]
                .into_iter()
                .collect(),
        };
        let manifest = Manifest::new("0.1.0", vec!["agent".into()]).with_recording_env(env);
        let w = TraceWriter::create(&dir, manifest).unwrap();
        w.finalize().unwrap();

        let r = TraceReader::open(&dir).unwrap();
        let got = r.manifest.recording_env.as_ref().expect("env present");
        assert_eq!(got.cwd.as_deref(), Some("/work"));
        assert_eq!(got.recorder_pid, Some(4242));
        assert_eq!(got.env.get("PATH").map(String::as_str), Some("/usr/bin"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fixed_clock_yields_bit_identical_traces() {
        // Same input + same clock => same manifest.json and events.jsonl bytes.
        fn write_one(dir: &std::path::Path) -> (Vec<u8>, Vec<u8>) {
            let clock = Box::new(FixedClock::at_nanos(1_700_000_000_000_000_000));
            let manifest =
                Manifest::new_with_clock("0.1.0-test", vec!["agent".into()], clock.as_ref());
            let mut w = TraceWriter::create_with_clock(dir, manifest, clock).unwrap();
            let f1 = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":{}}"#).unwrap();
            let f2 = Frame::parse(br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
            w.record(Direction::C2a, &f1).unwrap();
            w.record(Direction::A2c, &f2).unwrap();
            w.finalize().unwrap();
            (
                fs::read(dir.join("manifest.json")).unwrap(),
                fs::read(dir.join("events.jsonl")).unwrap(),
            )
        }

        let a = tmpdir("fixed-a");
        let b = tmpdir("fixed-b");
        let (ma, ea) = write_one(&a);
        let (mb, eb) = write_one(&b);

        assert_eq!(ea, eb, "events.jsonl must be bit-identical with FixedClock");
        assert_eq!(
            ma, mb,
            "manifest.json must be bit-identical with FixedClock"
        );

        fs::remove_dir_all(&a).ok();
        fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn fixed_clock_rfc3339_matches_expected() {
        // 2_000_000 ns past the epoch = .002 s.
        let c = FixedClock::at_nanos(2_000_000);
        assert_eq!(c.unix_nanos(), 2_000_000);
        let s = c.rfc3339_now();
        assert!(s.starts_with("1970-01-01T00:00:00.002"), "got {s:?}");
    }
}
