//! End-to-end test exercising the full pipeline against a real subprocess:
//!
//! 1. Spawn `acp record --trace <dir> -- acp-echo-agent`, feed it a real
//!    ACP session on stdin, capture stdout. Verify the trace is well-formed.
//! 2. Spawn `acp replay <dir>`, feed it the SAME client stream, capture
//!    stdout. Assert that the bytes are identical to what the proxy emitted
//!    during recording — i.e. true byte-exact deterministic replay.
//! 3. Spawn a second `acp record` with a perturbed client stream and verify
//!    `acp diff` flags the divergence.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_acp")
}

fn agent_bin() -> &'static str {
    env!("CARGO_BIN_EXE_acp-echo-agent")
}

fn tmpdir(name: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    d.push(format!("acp-e2e-{name}-{nanos}"));
    d
}

const SESSION_INPUT: &[u8] = b"\
{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":\"/work\"}}\n\
{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess-1\",\"prompt\":\"hello\"}}\n\
{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess-1\",\"prompt\":\"world\"}}\n\
{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"shutdown\",\"params\":{}}\n";

fn run_record(trace: &Path, input: &[u8]) -> (Vec<u8>, i32) {
    let mut child = Command::new(cli_bin())
        .arg("record")
        .arg("--trace")
        .arg(trace)
        .arg("--")
        .arg(agent_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn acp record");
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take()); // close to signal EOF
    let mut out = Vec::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    let status = child.wait().expect("wait");
    (out, status.code().unwrap_or(-1))
}

fn run_replay(trace: &Path, input: &[u8]) -> (Vec<u8>, Vec<u8>, i32) {
    let mut child = Command::new(cli_bin())
        .arg("replay")
        .arg(trace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn acp replay");
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take());
    let mut out = Vec::new();
    let mut err = Vec::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_end(&mut err)
        .unwrap();
    let status = child.wait().expect("wait");
    (out, err, status.code().unwrap_or(-1))
}

#[test]
fn record_then_replay_is_byte_exact() {
    let trace = tmpdir("record");
    let (recorded_out, code) = run_record(&trace, SESSION_INPUT);
    assert_eq!(code, 0, "acp record exit");
    assert!(!recorded_out.is_empty(), "proxy produced no output");

    // Trace structure: manifest.json, events.jsonl, blobs/
    assert!(trace.join("manifest.json").is_file());
    assert!(trace.join("events.jsonl").is_file());
    assert!(trace.join("blobs").is_dir());

    let (replayed_out, replay_err, code) = run_replay(&trace, SESSION_INPUT);
    assert_eq!(
        code,
        0,
        "acp replay exit: stderr={}",
        String::from_utf8_lossy(&replay_err)
    );
    assert_eq!(
        replayed_out,
        recorded_out,
        "replay output diverges from recorded output:\n  recorded: {:?}\n  replayed: {:?}",
        String::from_utf8_lossy(&recorded_out),
        String::from_utf8_lossy(&replayed_out)
    );

    std::fs::remove_dir_all(&trace).ok();
}

#[test]
fn replay_detects_divergence_on_changed_input() {
    let trace = tmpdir("diverge");
    let (_, code) = run_record(&trace, SESSION_INPUT);
    assert_eq!(code, 0);

    let perturbed = b"\
{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":\"/somewhere/else\"}}\n";

    let (_, err, code) = run_replay(&trace, perturbed);
    assert_ne!(
        code,
        0,
        "expected non-zero exit, stderr={}",
        String::from_utf8_lossy(&err)
    );
    let err_text = String::from_utf8_lossy(&err);
    assert!(
        err_text.contains("Divergence")
            || err_text.contains("divergence")
            || err_text.contains("expected next c2a"),
        "stderr should explain the divergence; got: {err_text}"
    );

    std::fs::remove_dir_all(&trace).ok();
}

#[test]
fn replay_is_order_invariant_under_key_permutation() {
    // Same logical session, but with object keys reordered in EVERY client frame.
    let trace = tmpdir("perm");
    let (_, code) = run_record(&trace, SESSION_INPUT);
    assert_eq!(code, 0);

    let permuted = b"\
{\"method\":\"initialize\",\"id\":1,\"jsonrpc\":\"2.0\",\"params\":{\"protocolVersion\":1}}\n\
{\"params\":{\"cwd\":\"/work\"},\"method\":\"session/new\",\"id\":2,\"jsonrpc\":\"2.0\"}\n\
{\"method\":\"session/prompt\",\"jsonrpc\":\"2.0\",\"id\":3,\"params\":{\"prompt\":\"hello\",\"sessionId\":\"sess-1\"}}\n\
{\"id\":4,\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess-1\",\"prompt\":\"world\"}}\n\
{\"jsonrpc\":\"2.0\",\"params\":{},\"method\":\"shutdown\",\"id\":5}\n";

    let (out, err, code) = run_replay(&trace, permuted);
    assert_eq!(
        code,
        0,
        "replay should accept permuted-key client: stderr={}",
        String::from_utf8_lossy(&err)
    );
    assert!(!out.is_empty());

    std::fs::remove_dir_all(&trace).ok();
}

#[test]
fn inspect_reports_correct_counts() {
    let trace = tmpdir("inspect");
    let (_, code) = run_record(&trace, SESSION_INPUT);
    assert_eq!(code, 0);

    let out = Command::new(cli_bin())
        .arg("inspect")
        .arg(&trace)
        .output()
        .expect("inspect");
    assert!(
        out.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // 5 client frames sent.
    assert!(
        text.contains("c2a frames      : 5"),
        "want 5 c2a frames in: {text}"
    );
    // The echo-agent emits, for the recorded prompts: 2 update notifications + responses for
    // (initialize, session/new, prompt×2, shutdown) = 2 + 5 = 7 a2c frames.
    assert!(
        text.contains("a2c frames      : 7"),
        "want 7 a2c frames in: {text}"
    );

    std::fs::remove_dir_all(&trace).ok();
}

#[test]
fn diff_identifies_divergent_traces() {
    let a = tmpdir("diff-a");
    let b = tmpdir("diff-b");
    let (_, ca) = run_record(&a, SESSION_INPUT);
    assert_eq!(ca, 0);

    let alt = b"\
{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":\"/work\"}}\n\
{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess-1\",\"prompt\":\"hello\"}}\n\
{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess-1\",\"prompt\":\"DIFFERENT\"}}\n\
{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"shutdown\",\"params\":{}}\n";
    let (_, cb) = run_record(&b, alt);
    assert_eq!(cb, 0);

    let out = Command::new(cli_bin())
        .arg("diff")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("diff");
    // diff returns exit code 1 when traces differ.
    assert_eq!(
        out.status.code(),
        Some(1),
        "diff should exit 1 when traces differ"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("only_a") || text.contains("only_b"),
        "diff text: {text}"
    );

    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}
