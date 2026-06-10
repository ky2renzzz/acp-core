//! Sanity benchmark — not a microbenchmark framework, just measured wall time
//! through the public CLI on a realistic session size, gated behind
//! `ACP_RUN_BENCH=1` so it does not slow down ordinary `cargo test`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

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
    d.push(format!("acp-bench-{name}-{nanos}"));
    d
}

fn build_input(prompt_count: usize) -> Vec<u8> {
    let mut s = String::new();
    s.push_str("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n");
    s.push_str("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":\"/work\"}}\n");
    for i in 0..prompt_count {
        let id = i + 3;
        s.push_str(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":\"hello #{i}\"}}}}\n"
        ));
    }
    s.push_str(&format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"shutdown\",\"params\":{{}}}}\n",
        prompt_count + 3
    ));
    s.into_bytes()
}

#[test]
fn bench_record_replay() {
    if std::env::var("ACP_RUN_BENCH").ok().as_deref() != Some("1") {
        eprintln!("(skip — set ACP_RUN_BENCH=1 to run)");
        return;
    }
    let prompts = 1000usize;
    let input = build_input(prompts);
    let trace = tmpdir("rec");

    // RECORD
    let t0 = Instant::now();
    let mut child = Command::new(cli_bin())
        .arg("record")
        .arg("--trace")
        .arg(&trace)
        .arg("--")
        .arg(agent_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(&input).unwrap();
    drop(child.stdin.take());
    let mut rec_out = Vec::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut rec_out)
        .unwrap();
    child.wait().unwrap();
    let rec = t0.elapsed();

    // REPLAY
    let t1 = Instant::now();
    let mut child = Command::new(cli_bin())
        .arg("replay")
        .arg(&trace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(&input).unwrap();
    drop(child.stdin.take());
    let mut rep_out = Vec::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut rep_out)
        .unwrap();
    child.wait().unwrap();
    let rep = t1.elapsed();

    assert_eq!(rec_out, rep_out, "byte-exact requirement");

    let total_frames = 2 + prompts + 1 + 2 + 2 * prompts + 1; // c2a (2+prompts+1=4) + a2c (2+2*prompts+1) — for stats only
    eprintln!("== acp benchmark ==");
    eprintln!("prompts in session : {prompts}");
    eprintln!("input bytes        : {}", input.len());
    eprintln!("output bytes       : {}", rec_out.len());
    eprintln!("record wall-time   : {:?}", rec);
    eprintln!("replay wall-time   : {:?}", rep);
    eprintln!("trace dir size:");
    let mut total = 0u64;
    for entry in walk(&trace) {
        let len = std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
        total += len;
        eprintln!(
            "  {:>10} bytes  {}",
            len,
            entry.strip_prefix(&trace).unwrap_or(&entry).display()
        );
    }
    eprintln!("trace total        : {total} bytes");
    eprintln!("frames recorded ~  : {total_frames}");
    eprintln!(
        "record throughput  : {:.1} frames/s",
        total_frames as f64 / rec.as_secs_f64()
    );
    eprintln!(
        "replay throughput  : {:.1} frames/s",
        total_frames as f64 / rep.as_secs_f64()
    );

    std::fs::remove_dir_all(&trace).ok();
}

fn walk(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.is_file() {
        out.push(root.to_path_buf());
        return out;
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
