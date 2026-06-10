//! Divergence analysis for ACP traces.
//!
//! Each recorded session is reduced to a sequence of "steps", where a step
//! is the tuple `(direction, canonical_hash)`. Two sessions can then be
//! aligned by running Longest-Common-Subsequence on these step sequences:
//!
//! * Steps in the LCS are *common* — both subagents observed the same
//!   message at the same logical point.
//! * Steps not in the LCS are *divergent* — they appear in one trace but
//!   not the other.
//!
//! This is exactly what you want when investigating "8 worktree subagents
//! were given the same task; where did they part ways?".
//!
//! The implementation is `O(n*m)` time and `O(n*m)` memory in the lengths
//! of the two step sequences. For session sizes ACP agents realistically
//! produce (thousands of frames at most), this is well below a second.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, missing_debug_implementations)]

use acp_trace::{Direction, EventRecord, TraceReader};

/// One aligned step in the comparison output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    /// Both traces had this step at the aligned position.
    Common {
        a_seq: u64,
        b_seq: u64,
        dir: Direction,
        hash: String,
        method: Option<String>,
    },
    /// Step present only in trace A.
    OnlyA {
        a_seq: u64,
        dir: Direction,
        hash: String,
        method: Option<String>,
    },
    /// Step present only in trace B.
    OnlyB {
        b_seq: u64,
        dir: Direction,
        hash: String,
        method: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct DiffReport {
    pub label_a: String,
    pub label_b: String,
    pub ops: Vec<DiffOp>,
    pub stats: DiffStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub common: u64,
    pub only_a: u64,
    pub only_b: u64,
    /// Position (0-based) of the first divergent op, if any.
    pub first_divergence: Option<usize>,
}

#[derive(Clone, PartialEq, Eq)]
struct Step<'a> {
    dir: Direction,
    hash: &'a str,
    method: Option<&'a str>,
    seq: u64,
}

fn steps(trace: &TraceReader) -> Vec<Step<'_>> {
    trace
        .events
        .iter()
        .map(|e: &EventRecord| Step {
            dir: e.dir,
            hash: e.hash.as_str(),
            method: e.method.as_deref(),
            seq: e.seq,
        })
        .collect()
}

/// Compare two traces. The labels are used in the resulting [`DiffReport`].
pub fn diff_two(a: &TraceReader, label_a: &str, b: &TraceReader, label_b: &str) -> DiffReport {
    let sa = steps(a);
    let sb = steps(b);
    let ops = lcs_diff(&sa, &sb);

    let mut stats = DiffStats::default();
    for (i, op) in ops.iter().enumerate() {
        match op {
            DiffOp::Common { .. } => stats.common += 1,
            DiffOp::OnlyA { .. } => {
                if stats.first_divergence.is_none() {
                    stats.first_divergence = Some(i);
                }
                stats.only_a += 1;
            }
            DiffOp::OnlyB { .. } => {
                if stats.first_divergence.is_none() {
                    stats.first_divergence = Some(i);
                }
                stats.only_b += 1;
            }
        }
    }

    DiffReport {
        label_a: label_a.into(),
        label_b: label_b.into(),
        ops,
        stats,
    }
}

fn step_eq(a: &Step<'_>, b: &Step<'_>) -> bool {
    a.dir == b.dir && a.hash == b.hash
}

fn lcs_diff(a: &[Step<'_>], b: &[Step<'_>]) -> Vec<DiffOp> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = LCS length of a[..i], b[..j].
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in 0..n {
        for j in 0..m {
            let v = if step_eq(&a[i], &b[j]) {
                dp[idx(i, j)] + 1
            } else {
                dp[idx(i, j + 1)].max(dp[idx(i + 1, j)])
            };
            dp[idx(i + 1, j + 1)] = v;
        }
    }

    // Backtrack.
    let mut ops = Vec::with_capacity(n + m);
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if step_eq(&a[i - 1], &b[j - 1]) {
            let s = &a[i - 1];
            ops.push(DiffOp::Common {
                a_seq: a[i - 1].seq,
                b_seq: b[j - 1].seq,
                dir: s.dir,
                hash: s.hash.to_string(),
                method: s.method.map(String::from),
            });
            i -= 1;
            j -= 1;
        } else if dp[idx(i - 1, j)] >= dp[idx(i, j - 1)] {
            let s = &a[i - 1];
            ops.push(DiffOp::OnlyA {
                a_seq: s.seq,
                dir: s.dir,
                hash: s.hash.to_string(),
                method: s.method.map(String::from),
            });
            i -= 1;
        } else {
            let s = &b[j - 1];
            ops.push(DiffOp::OnlyB {
                b_seq: s.seq,
                dir: s.dir,
                hash: s.hash.to_string(),
                method: s.method.map(String::from),
            });
            j -= 1;
        }
    }
    while i > 0 {
        let s = &a[i - 1];
        ops.push(DiffOp::OnlyA {
            a_seq: s.seq,
            dir: s.dir,
            hash: s.hash.to_string(),
            method: s.method.map(String::from),
        });
        i -= 1;
    }
    while j > 0 {
        let s = &b[j - 1];
        ops.push(DiffOp::OnlyB {
            b_seq: s.seq,
            dir: s.dir,
            hash: s.hash.to_string(),
            method: s.method.map(String::from),
        });
        j -= 1;
    }
    ops.reverse();
    ops
}

/// Render a unified text diff (one line per op).
pub fn render_unified(report: &DiffReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "--- {}\n+++ {}", report.label_a, report.label_b);
    let _ = writeln!(
        out,
        "# common={} only_a={} only_b={} first_div={:?}",
        report.stats.common,
        report.stats.only_a,
        report.stats.only_b,
        report.stats.first_divergence
    );
    for op in &report.ops {
        match op {
            DiffOp::Common {
                a_seq,
                b_seq,
                dir,
                method,
                hash,
            } => {
                let _ = writeln!(
                    out,
                    "  [{:>5}|{:>5}] {} {:<40} {}",
                    a_seq,
                    b_seq,
                    dir.as_str(),
                    method.clone().unwrap_or_default(),
                    short_hash(hash)
                );
            }
            DiffOp::OnlyA {
                a_seq,
                dir,
                method,
                hash,
            } => {
                let _ = writeln!(
                    out,
                    "- [{:>5}|     ] {} {:<40} {}",
                    a_seq,
                    dir.as_str(),
                    method.clone().unwrap_or_default(),
                    short_hash(hash)
                );
            }
            DiffOp::OnlyB {
                b_seq,
                dir,
                method,
                hash,
            } => {
                let _ = writeln!(
                    out,
                    "+ [     |{:>5}] {} {:<40} {}",
                    b_seq,
                    dir.as_str(),
                    method.clone().unwrap_or_default(),
                    short_hash(hash)
                );
            }
        }
    }
    out
}

fn short_hash(h: &str) -> String {
    let stripped = h.strip_prefix("blake3:").unwrap_or(h);
    let n = stripped.len().min(12);
    format!("blake3:{}…", &stripped[..n])
}

/// Render a Graphviz DOT diagram of the divergence: each op is a node,
/// arrows go from one op to the next, common ops are coloured green and
/// divergent ops red. Useful for visual inspection in `xdot` / online viewers.
pub fn render_dot(report: &DiffReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "digraph divergence {{");
    let _ = writeln!(out, "  rankdir=LR;");
    let _ = writeln!(out, "  node [shape=box, fontname=\"monospace\"];");
    let _ = writeln!(out, "  labelloc=\"t\";");
    let _ = writeln!(
        out,
        "  label=\"{} vs {}\\ncommon={} only_a={} only_b={}\";",
        report.label_a,
        report.label_b,
        report.stats.common,
        report.stats.only_a,
        report.stats.only_b
    );

    for (i, op) in report.ops.iter().enumerate() {
        match op {
            DiffOp::Common { dir, method, .. } => {
                let _ = writeln!(
                    out,
                    "  n{} [label=\"{} {}\", style=filled, fillcolor=\"#d6f5d6\"];",
                    i,
                    dir.as_str(),
                    method.clone().unwrap_or_default().replace('"', "\\\"")
                );
            }
            DiffOp::OnlyA { dir, method, .. } => {
                let _ = writeln!(
                    out,
                    "  n{} [label=\"only-A: {} {}\", style=filled, fillcolor=\"#fbd5d5\"];",
                    i,
                    dir.as_str(),
                    method.clone().unwrap_or_default().replace('"', "\\\"")
                );
            }
            DiffOp::OnlyB { dir, method, .. } => {
                let _ = writeln!(
                    out,
                    "  n{} [label=\"only-B: {} {}\", style=filled, fillcolor=\"#fde3a7\"];",
                    i,
                    dir.as_str(),
                    method.clone().unwrap_or_default().replace('"', "\\\"")
                );
            }
        }
        if i + 1 < report.ops.len() {
            let _ = writeln!(out, "  n{} -> n{};", i, i + 1);
        }
    }
    let _ = writeln!(out, "}}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_trace::{Direction, Manifest, TraceWriter};
    use acp_wire::Frame;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        d.push(format!("acp-diverge-test-{name}-{nanos}"));
        d
    }

    fn build(dir: &std::path::Path, frames: &[(Direction, &[u8])]) {
        let mut w = TraceWriter::create(dir, Manifest::new("0.1.0", vec![])).unwrap();
        for (d, raw) in frames {
            let f = Frame::parse(raw).unwrap();
            w.record(*d, &f).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn identical_traces_are_all_common() {
        let a = tmpdir("eq-a");
        let b = tmpdir("eq-b");
        let frames: &[(Direction, &[u8])] = &[
            (
                Direction::C2a,
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            ),
            (Direction::A2c, br#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
        ];
        build(&a, frames);
        build(&b, frames);
        let ra = TraceReader::open(&a).unwrap();
        let rb = TraceReader::open(&b).unwrap();
        let report = diff_two(&ra, "A", &rb, "B");
        assert_eq!(report.stats.common, 2);
        assert_eq!(report.stats.only_a, 0);
        assert_eq!(report.stats.only_b, 0);
        assert_eq!(report.stats.first_divergence, None);
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn divergence_pinpoints_first_difference() {
        let a = tmpdir("d-a");
        let b = tmpdir("d-b");
        build(
            &a,
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
                    br#"{"jsonrpc":"2.0","method":"session/update","params":{"path":"a.rs"}}"#,
                ),
            ],
        );
        build(
            &b,
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
                    br#"{"jsonrpc":"2.0","method":"session/update","params":{"path":"b.rs"}}"#,
                ),
            ],
        );
        let ra = TraceReader::open(&a).unwrap();
        let rb = TraceReader::open(&b).unwrap();
        let r = diff_two(&ra, "A", &rb, "B");
        assert_eq!(r.stats.common, 2);
        assert_eq!(r.stats.only_a, 1);
        assert_eq!(r.stats.only_b, 1);
        let first = r.stats.first_divergence.unwrap();
        // First two are common, divergence starts at index 2.
        assert!(first >= 2);
        let _ = render_unified(&r);
        let dot = render_dot(&r);
        assert!(dot.contains("digraph divergence"));
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }
}
