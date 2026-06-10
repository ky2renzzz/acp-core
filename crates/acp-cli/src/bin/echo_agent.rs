//! Minimal real ACP agent used as a test fixture.
//!
//! Implements just enough of ACP to exercise the record/replay pipeline:
//!
//! * `initialize` — returns a fixed protocolVersion and empty capabilities.
//! * `session/new` — returns a deterministic sessionId derived from request count.
//! * `session/prompt` — emits a `session/update` notification, then responds
//!   with `{"stopReason":"end_turn"}`.
//! * any other method — returns JSON-RPC error -32601 (method not found).
//!
//! Everything about the responses is deterministic (no time, no randomness),
//! so a recorded trace can be byte-compared against a fresh run.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

use std::io::{self, BufReader, Write};

use serde_json::{json, Value};

use acp_wire::{FrameReader, Id};

fn main() {
    if let Err(e) = run() {
        eprintln!("acp-echo-agent: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = FrameReader::new(BufReader::new(stdin.lock()));
    let mut out = stdout.lock();
    let mut prompt_count: u64 = 0;
    let mut session_count: u64 = 0;

    while let Some(frame) = reader.read_frame()? {
        let id = match frame.id.as_ref() {
            Some(id) => id.clone(),
            None => {
                // Notifications: we accept and ignore.
                continue;
            }
        };
        let method = frame.method.as_deref().unwrap_or("");
        match method {
            "initialize" => {
                send_result(
                    &mut out,
                    &id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {}
                    }),
                )?;
            }
            "session/new" => {
                session_count += 1;
                send_result(
                    &mut out,
                    &id,
                    json!({ "sessionId": format!("sess-{session_count}") }),
                )?;
            }
            "session/prompt" => {
                prompt_count += 1;
                let session_id = frame
                    .value
                    .get("params")
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("sess-?")
                    .to_string();
                // Streaming update notification.
                send_notification(
                    &mut out,
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "kind": "agent_message_chunk",
                            "content": { "type": "text", "text": format!("echo #{prompt_count}") }
                        }
                    }),
                )?;
                // Final response.
                send_result(&mut out, &id, json!({ "stopReason": "end_turn" }))?;
            }
            "shutdown" => {
                send_result(&mut out, &id, json!(null))?;
                break;
            }
            _ => {
                send_error(
                    &mut out,
                    &id,
                    -32601,
                    &format!("method not found: {method}"),
                )?;
            }
        }
    }
    Ok(())
}

fn send_result<W: Write>(out: &mut W, id: &Id, result: Value) -> io::Result<()> {
    let env = json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "result": result
    });
    write_envelope(out, &env)
}

fn send_error<W: Write>(out: &mut W, id: &Id, code: i32, message: &str) -> io::Result<()> {
    let env = json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "error": { "code": code, "message": message }
    });
    write_envelope(out, &env)
}

fn send_notification<W: Write>(out: &mut W, method: &str, params: Value) -> io::Result<()> {
    let env = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params
    });
    write_envelope(out, &env)
}

fn write_envelope<W: Write>(out: &mut W, env: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(env).expect("serialize");
    // Defence in depth: drop the (impossible here) embedded newline.
    debug_assert!(!bytes.contains(&b'\n'));
    out.write_all(&bytes)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
