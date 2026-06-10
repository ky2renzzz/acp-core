//! Transparent recording proxy for ACP agents.
//!
//! The proxy spawns an ACP agent as a subprocess and sits between it and the
//! parent process (the real ACP client). Every newline-delimited frame
//! crossing the boundary is parsed, written to a [`TraceWriter`], and
//! forwarded with its original bytes intact. The agent's stderr is forwarded
//! to the proxy's stderr untouched.
//!
//! There is no async runtime here. Three OS threads (`stdin → child.stdin`,
//! `child.stdout → stdout`, `child.stderr → stderr`) is the minimum to
//! safely move bytes in both directions without deadlock, and it keeps the
//! crate dependency-free of `tokio`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, missing_debug_implementations)]

use std::ffi::OsString;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use thiserror::Error;

use acp_trace::{Direction, Manifest, TraceWriter};
use acp_wire::{FrameReader, WireError};

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("wire: {0}")]
    Wire(#[from] WireError),
    #[error("trace: {0}")]
    Trace(#[from] acp_trace::TraceError),
    #[error("agent argv is empty")]
    EmptyArgv,
    #[error("agent exited with status {0}")]
    AgentExit(i32),
}

/// Configuration for a single recording.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Agent executable followed by its arguments. Use `OsString` so that
    /// Windows-only code units and non-UTF-8 paths survive untouched into
    /// the child process. The manifest records lossy UTF-8 strings.
    pub agent_argv: Vec<OsString>,
    pub trace_dir: std::path::PathBuf,
    pub recorder_version: String,
}

/// Run the proxy until the agent exits or the parent stdin closes.
///
/// `parent_stdin`  — bytes coming from the real ACP client (this binary's stdin).
/// `parent_stdout` — bytes going to the real ACP client (this binary's stdout).
/// `parent_stderr` — receives the agent's stderr.
///
/// The function returns the agent's exit code.
pub fn run<R, W, E>(
    cfg: ProxyConfig,
    parent_stdin: R,
    parent_stdout: W,
    parent_stderr: E,
) -> Result<i32, ProxyError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    E: Write + Send + 'static,
{
    if cfg.agent_argv.is_empty() {
        return Err(ProxyError::EmptyArgv);
    }

    let manifest_argv: Vec<String> = cfg
        .agent_argv
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let manifest = Manifest::new(&cfg.recorder_version, manifest_argv);
    let writer = TraceWriter::create(&cfg.trace_dir, manifest)?;
    let writer = Arc::new(Mutex::new(Some(writer)));

    let mut cmd = Command::new(&cfg.agent_argv[0]);
    cmd.args(&cfg.agent_argv[1..]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child: Child = cmd.spawn()?;

    let child_stdin = child.stdin.take().expect("piped");
    let child_stdout = child.stdout.take().expect("piped");
    let child_stderr = child.stderr.take().expect("piped");

    // C2A: parent stdin → child stdin, recording every frame.
    let w_c2a = Arc::clone(&writer);
    let c2a = thread::spawn(move || -> Result<(), ProxyError> {
        let mut reader = FrameReader::new(BufReader::new(parent_stdin));
        let mut sink = BufWriter::new(child_stdin);
        while let Some(frame) = reader.read_frame()? {
            acp_wire::write_frame(&mut sink, &frame)?;
            if let Some(w) = w_c2a.lock().unwrap().as_mut() {
                w.record(Direction::C2a, &frame)?;
            }
        }
        // Closing child stdin signals EOF to the agent.
        drop(sink);
        Ok(())
    });

    // A2C: child stdout → parent stdout, recording every frame.
    let w_a2c = Arc::clone(&writer);
    let a2c = thread::spawn(move || -> Result<(), ProxyError> {
        let mut reader = FrameReader::new(BufReader::new(child_stdout));
        let mut sink = BufWriter::new(parent_stdout);
        while let Some(frame) = reader.read_frame()? {
            acp_wire::write_frame(&mut sink, &frame)?;
            if let Some(w) = w_a2c.lock().unwrap().as_mut() {
                w.record(Direction::A2c, &frame)?;
            }
        }
        Ok(())
    });

    // Stderr forwarder. Plain bytes, no parsing — the spec only mandates
    // that stderr is UTF-8 if used, and clients MAY ignore it.
    let stderr = thread::spawn(move || -> io::Result<()> {
        let mut src = BufReader::new(child_stderr);
        let mut sink = parent_stderr;
        let mut buf = [0u8; 4096];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sink.write_all(&buf[..n])?;
            sink.flush()?;
        }
        Ok(())
    });

    let status = child.wait()?;

    // Join threads. The A2C reader naturally ends when child stdout closes,
    // which the wait() above implies. The C2A reader may still be blocked
    // on parent stdin if the client hasn't closed it — that's expected
    // (the proxy lifetime is bounded by the child) so we don't join it.
    let _ = a2c.join();
    let _ = stderr.join();
    // c2a thread keeps a clone of the writer Arc; we must release it.
    // Dropping the writer slot here (before joining c2a) is fine because
    // c2a's `lock().unwrap().as_mut()` will simply see None.
    {
        let mut slot = writer.lock().unwrap();
        if let Some(w) = slot.take() {
            w.finalize()?;
        }
    }
    // Best-effort wait for c2a to notice EOF or be unblocked.
    let _ = c2a.join();

    Ok(status.code().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity test for ProxyError display formatting.
    #[test]
    fn empty_argv_rejected() {
        let cfg = ProxyConfig {
            agent_argv: vec![],
            trace_dir: std::env::temp_dir().join("acp-proxy-empty"),
            recorder_version: "0.1.0".into(),
        };
        let r = run(cfg, io::empty(), io::sink(), io::sink());
        assert!(matches!(r, Err(ProxyError::EmptyArgv)));
    }
}
