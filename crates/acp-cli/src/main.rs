//! `acp` — command-line front-end for the acp-core libraries.
//!
//! Subcommands:
//!
//! * `acp record <trace-dir> -- <agent argv…>` — proxy stdio between this
//!   process and the agent, writing a trace.
//! * `acp replay <trace-dir>` — act as the agent against a live client on
//!   stdio, using the recorded session as a stub.
//! * `acp emit <trace-dir>` — write every recorded a2c frame to stdout.
//! * `acp inspect <trace-dir>` — print a human summary of the trace.
//! * `acp diff <trace-a> <trace-b> [--dot]` — divergence report between two traces.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

use std::ffi::OsString;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use acp_diverge::{diff_two, render_dot, render_unified};
use acp_proxy::ProxyConfig;
use acp_replay::{replay_interactive, replay_offline};
use acp_trace::{Direction, TraceReader};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(name = "acp", version = VERSION, about = "ACP recorder, replayer and divergence analyzer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Record a session by proxying stdio to a child ACP agent.
    Record {
        /// Where to write the trace.
        #[arg(long)]
        trace: PathBuf,
        /// Agent argv (executable followed by args). Use `--` to separate.
        #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
        agent: Vec<OsString>,
    },
    /// Replay a recorded session against a live ACP client on stdio.
    Replay {
        /// Path to the trace directory.
        trace: PathBuf,
    },
    /// Emit the recorded a2c frames to stdout, in order.
    Emit {
        /// Path to the trace directory.
        trace: PathBuf,
    },
    /// Print a human summary of a trace.
    Inspect {
        /// Path to the trace directory.
        trace: PathBuf,
        /// Show every event (default: first/last 10).
        #[arg(long)]
        full: bool,
    },
    /// Compare two traces (LCS by canonical hash).
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Output Graphviz DOT instead of text.
        #[arg(long)]
        dot: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("acp: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Record { trace, agent } => cmd_record(trace, agent),
        Cmd::Replay { trace } => cmd_replay(trace),
        Cmd::Emit { trace } => cmd_emit(trace),
        Cmd::Inspect { trace, full } => cmd_inspect(trace, full),
        Cmd::Diff { a, b, dot } => cmd_diff(a, b, dot),
    }
}

fn cmd_record(trace_dir: PathBuf, agent: Vec<OsString>) -> Result<ExitCode> {
    if agent.is_empty() {
        bail!("agent argv is empty");
    }
    let cfg = ProxyConfig {
        agent_argv: agent,
        trace_dir: trace_dir.clone(),
        recorder_version: VERSION.into(),
    };
    let code = acp_proxy::run(cfg, io::stdin(), io::stdout(), io::stderr())
        .with_context(|| format!("recording to {}", trace_dir.display()))?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

fn cmd_replay(trace_dir: PathBuf) -> Result<ExitCode> {
    let reader = TraceReader::open(&trace_dir)
        .with_context(|| format!("opening trace {}", trace_dir.display()))?;
    let stdin = BufReader::new(io::stdin());
    let mut stdout = io::stdout();
    let emitted = replay_interactive(&reader, stdin, &mut stdout).context("interactive replay")?;
    eprintln!("acp: replay emitted {emitted} a2c frames");
    Ok(ExitCode::SUCCESS)
}

fn cmd_emit(trace_dir: PathBuf) -> Result<ExitCode> {
    let reader = TraceReader::open(&trace_dir)
        .with_context(|| format!("opening trace {}", trace_dir.display()))?;
    let mut stdout = io::stdout();
    let n = replay_offline(&reader, &mut stdout)?;
    eprintln!("acp: emitted {n} a2c frames");
    Ok(ExitCode::SUCCESS)
}

fn cmd_inspect(trace_dir: PathBuf, full: bool) -> Result<ExitCode> {
    let reader = TraceReader::open(&trace_dir)
        .with_context(|| format!("opening trace {}", trace_dir.display()))?;
    let m = &reader.manifest;
    println!("trace_dir       : {}", trace_dir.display());
    println!("format_version  : {}", m.format_version);
    println!("recorder        : {} {}", m.recorder, m.recorder_version);
    println!("agent_argv      : {}", shell_join(&m.agent_argv));
    println!("started_at      : {}", m.started_at);
    println!(
        "ended_at        : {}",
        m.ended_at.clone().unwrap_or_else(|| "<unfinished>".into())
    );
    println!(
        "event_count     : {}",
        m.event_count.unwrap_or(reader.events.len() as u64)
    );
    println!(
        "events_hash     : {}",
        m.events_hash
            .clone()
            .unwrap_or_else(|| "<unfinished>".into())
    );

    let (c2a, a2c) = count_dirs(&reader);
    println!("c2a frames      : {c2a}");
    println!("a2c frames      : {a2c}");
    let bytes_total: u64 = reader.events.iter().map(|e| e.bytes).sum();
    println!("total wire bytes: {bytes_total}");
    let unique: std::collections::HashSet<&String> =
        reader.events.iter().map(|e| &e.hash).collect();
    println!(
        "unique payloads : {} (dedup ratio {:.2}x)",
        unique.len(),
        if unique.is_empty() {
            0.0
        } else {
            reader.events.len() as f64 / unique.len() as f64
        }
    );

    println!();
    println!("seq  dir  kind          method                                   bytes  hash");
    let events = &reader.events;
    let shown: Vec<&acp_trace::EventRecord> = if full || events.len() <= 20 {
        events.iter().collect()
    } else {
        let mut v: Vec<&acp_trace::EventRecord> = events.iter().take(10).collect();
        v.extend(events.iter().rev().take(10).rev());
        v
    };
    for e in shown {
        println!(
            "{:>4} {:<4} {:<13} {:<40} {:>6}  {}",
            e.seq,
            e.dir.as_str(),
            kind_str(e.kind),
            e.method.clone().unwrap_or_default(),
            e.bytes,
            short_hash(&e.hash),
        );
    }
    if !full && events.len() > 20 {
        println!("... ({} events not shown; use --full)", events.len() - 20);
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_diff(a: PathBuf, b: PathBuf, dot: bool) -> Result<ExitCode> {
    let ra = TraceReader::open(&a).with_context(|| format!("opening {}", a.display()))?;
    let rb = TraceReader::open(&b).with_context(|| format!("opening {}", b.display()))?;
    let report = diff_two(&ra, &a.display().to_string(), &rb, &b.display().to_string());
    let text = if dot {
        render_dot(&report)
    } else {
        render_unified(&report)
    };
    print!("{text}");
    let exit = if report.stats.only_a == 0 && report.stats.only_b == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };
    Ok(exit)
}

fn count_dirs(r: &TraceReader) -> (u64, u64) {
    let mut c = 0u64;
    let mut a = 0u64;
    for e in &r.events {
        match e.dir {
            Direction::C2a => c += 1,
            Direction::A2c => a += 1,
        }
    }
    (c, a)
}

fn kind_str(k: acp_trace::RecordKind) -> &'static str {
    match k {
        acp_trace::RecordKind::Request => "request",
        acp_trace::RecordKind::Response => "response",
        acp_trace::RecordKind::Notification => "notification",
    }
}

fn short_hash(h: &str) -> String {
    let stripped = h.strip_prefix("blake3:").unwrap_or(h);
    let n = stripped.len().min(12);
    format!("blake3:{}…", &stripped[..n])
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.contains(' ') {
                format!("\"{p}\"")
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
