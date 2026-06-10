//! Library-only throughput bench.
//!
//! Measures the cost of `replay_interactive` and `replay_offline` driven
//! in-process against an in-memory client stream, with no subprocess,
//! no real stdio pipes, no terminal. This is the cost an editor plugin
//! or test harness sees when embedding the engine.
//!
//! Gated behind `ACP_RUN_BENCH=1` so it does not slow down ordinary
//! `cargo test`. Run with:
//!
//!     ACP_RUN_BENCH=1 cargo test -p acp-replay --release --test library_bench -- --nocapture

use std::io::Cursor;
use std::time::Instant;

use acp_replay::{replay_interactive, replay_offline};
use acp_trace::{Direction, FixedClock, Manifest, TraceReader, TraceWriter};
use acp_wire::Frame;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let mut d = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    d.push(format!("acp-libbench-{name}-{nanos}"));
    d
}

#[test]
fn library_throughput() {
    if std::env::var("ACP_RUN_BENCH").ok().as_deref() != Some("1") {
        eprintln!("(skip — set ACP_RUN_BENCH=1 to run)");
        return;
    }

    const PROMPTS: usize = 1_000;
    let trace_dir = tmpdir("libbench");

    // ── Build a 1 000-prompt synthetic trace directly via TraceWriter,
    //    without spawning a child process. Layout mirrors what
    //    acp-echo-agent would produce.
    let clock = Box::new(FixedClock::epoch());
    let manifest = Manifest::new_with_clock("libbench", vec!["synth".into()], clock.as_ref());
    let mut w = TraceWriter::create_with_clock(&trace_dir, manifest, clock).unwrap();

    let mut client_bytes: Vec<u8> = Vec::new();
    let push = |buf: &mut Vec<u8>, line: &[u8]| {
        buf.extend_from_slice(line);
        buf.push(b'\n');
    };

    // initialize / session/new always come first.
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;
    let init_res =
        br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#;
    let snew = br#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/work"}}"#;
    let snew_res = br#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}"#;
    push(&mut client_bytes, init);
    push(&mut client_bytes, snew);
    w.record(Direction::C2a, &Frame::parse(init).unwrap())
        .unwrap();
    w.record(Direction::A2c, &Frame::parse(init_res).unwrap())
        .unwrap();
    w.record(Direction::C2a, &Frame::parse(snew).unwrap())
        .unwrap();
    w.record(Direction::A2c, &Frame::parse(snew_res).unwrap())
        .unwrap();

    // N prompt round-trips. Each is: c2a prompt, a2c notification, a2c result.
    for i in 0..PROMPTS {
        let id = (i + 3) as u64;
        let prompt = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"session/prompt","params":{{"sessionId":"sess-1","prompt":"p{i}"}}}}"#
        );
        let notif = format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sess-1","update":{{"kind":"agent_message_chunk","content":{{"type":"text","text":"echo #{i}"}}}}}}}}"#
        );
        let result =
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"stopReason":"end_turn"}}}}"#);
        push(&mut client_bytes, prompt.as_bytes());
        w.record(Direction::C2a, &Frame::parse(prompt.as_bytes()).unwrap())
            .unwrap();
        w.record(Direction::A2c, &Frame::parse(notif.as_bytes()).unwrap())
            .unwrap();
        w.record(Direction::A2c, &Frame::parse(result.as_bytes()).unwrap())
            .unwrap();
    }

    // shutdown.
    let shut = br#"{"jsonrpc":"2.0","id":99999,"method":"shutdown","params":{}}"#;
    let shut_res = br#"{"jsonrpc":"2.0","id":99999,"result":null}"#;
    push(&mut client_bytes, shut);
    w.record(Direction::C2a, &Frame::parse(shut).unwrap())
        .unwrap();
    w.record(Direction::A2c, &Frame::parse(shut_res).unwrap())
        .unwrap();
    w.finalize().unwrap();

    let reader = TraceReader::open(&trace_dir).unwrap();
    let total_frames = reader.events.len();
    let a2c_frames = reader
        .events
        .iter()
        .filter(|e| e.dir == Direction::A2c)
        .count();

    // ── replay_interactive against the same client stream, in-memory.
    let mut out = Vec::with_capacity(client_bytes.len() * 3);
    let t0 = Instant::now();
    let emitted = replay_interactive(&reader, Cursor::new(&client_bytes[..]), &mut out).unwrap();
    let t_interactive = t0.elapsed();
    assert_eq!(emitted as usize, a2c_frames);

    // ── replay_offline writes every a2c blob in order, no client side.
    let mut offline_out = Vec::with_capacity(out.len());
    let t1 = Instant::now();
    let off_emitted = replay_offline(&reader, &mut offline_out).unwrap();
    let t_offline = t1.elapsed();
    assert_eq!(off_emitted as usize, a2c_frames);

    eprintln!("== acp-replay library bench (no subprocess, no pipes) ==");
    eprintln!("trace frames        : {total_frames}");
    eprintln!("a2c frames          : {a2c_frames}");
    eprintln!("client input bytes  : {}", client_bytes.len());
    eprintln!("recorded a2c bytes  : {}", out.len());
    eprintln!(
        "replay_interactive  : {:>8.2} ms  ({:>9.0} frames/s)",
        t_interactive.as_secs_f64() * 1e3,
        total_frames as f64 / t_interactive.as_secs_f64()
    );
    eprintln!(
        "replay_offline      : {:>8.2} ms  ({:>9.0} frames/s)",
        t_offline.as_secs_f64() * 1e3,
        a2c_frames as f64 / t_offline.as_secs_f64()
    );

    std::fs::remove_dir_all(&trace_dir).ok();
}
