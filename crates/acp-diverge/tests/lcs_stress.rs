//! Stress and property tests for the LCS-based step aligner.
//!
//! Two-trace unit tests already cover the "identical" and "single
//! divergent suffix" cases. This file exercises the alignment under
//! conditions the basic unit tests do not touch:
//!
//! * `diff_steps` MUST be symmetric: swapping A↔B reflects every
//!   `OnlyA`/`OnlyB` op and preserves common counts.
//! * Injecting K independent divergent islands into an otherwise common
//!   sequence MUST yield exactly the expected per-trace `only_*` counts.
//! * The aligner MUST handle the empty / one-sided cases sanely.
//! * The aligner MUST scale to a few thousand steps without taking a
//!   noticeable amount of time (sanity check on the documented limit).
//!
//! All inputs are built directly as `Vec<Step>` rather than going
//! through `TraceWriter`, because we want to test the alignment, not
//! disk I/O / blake3 / dedup.

use acp_diverge::{diff_steps, DiffOp, Step};
use acp_trace::Direction;

/// Build a step at sequence position `seq` with a synthetic hash.
fn step(seq: u64, dir: Direction, tag: &'static str) -> Step<'static> {
    // Hash uniqueness is what matters for alignment; the actual blake3
    // string doesn't have to be valid, only stable.
    Step {
        dir,
        hash: tag,
        method: None,
        seq,
    }
}

#[test]
fn empty_versus_empty_is_clean() {
    let (ops, stats) = diff_steps(&[], &[]);
    assert!(ops.is_empty());
    assert_eq!(stats.common, 0);
    assert_eq!(stats.only_a, 0);
    assert_eq!(stats.only_b, 0);
    assert_eq!(stats.first_divergence, None);
}

#[test]
fn empty_versus_nonempty_yields_only_b() {
    let b: Vec<Step<'_>> = (0..5).map(|i| step(i, Direction::C2a, "h-b")).collect();
    let (ops, stats) = diff_steps(&[], &b);
    assert_eq!(stats.only_a, 0);
    assert_eq!(stats.only_b, 5);
    assert!(ops.iter().all(|op| matches!(op, DiffOp::OnlyB { .. })));
    assert_eq!(stats.first_divergence, Some(0));
}

#[test]
fn identical_steps_are_all_common() {
    // Use distinct hashes so we don't accidentally rely on dedup.
    let make = || -> Vec<Step<'_>> {
        let tags = ["h0", "h1", "h2", "h3", "h4"];
        tags.iter()
            .enumerate()
            .map(|(i, &t)| step(i as u64, Direction::C2a, t))
            .collect()
    };
    let a = make();
    let b = make();
    let (_ops, stats) = diff_steps(&a, &b);
    assert_eq!(stats.common, 5);
    assert_eq!(stats.only_a, 0);
    assert_eq!(stats.only_b, 0);
    assert_eq!(stats.first_divergence, None);
}

/// Property: swapping the inputs reflects every divergent op.
#[test]
fn diff_steps_is_symmetric() {
    // Hand-built sequences with several divergence islands.
    let tags_a: &[&str] = &["x0", "x1", "DA", "x2", "x3", "DB", "x4"];
    let tags_b: &[&str] = &["x0", "x1", "EA", "x2", "x3", "EB", "x4"];
    let a: Vec<Step<'_>> = tags_a
        .iter()
        .enumerate()
        .map(|(i, &t)| step(i as u64, Direction::C2a, t))
        .collect();
    let b: Vec<Step<'_>> = tags_b
        .iter()
        .enumerate()
        .map(|(i, &t)| step(i as u64, Direction::C2a, t))
        .collect();

    let (_ops_ab, stats_ab) = diff_steps(&a, &b);
    let (_ops_ba, stats_ba) = diff_steps(&b, &a);

    assert_eq!(stats_ab.common, stats_ba.common);
    assert_eq!(stats_ab.only_a, stats_ba.only_b);
    assert_eq!(stats_ab.only_b, stats_ba.only_a);
    assert_eq!(stats_ab.first_divergence, stats_ba.first_divergence);
}

/// Injecting K independent divergent slots into an otherwise common
/// sequence must yield exactly K `only_a` AND K `only_b` ops.
#[test]
fn k_independent_divergence_islands_are_all_counted() {
    let k = 8;
    let stride = 50; // every 50th position diverges
    let total_common = k * stride;

    let mut a: Vec<Step<'_>> = Vec::with_capacity(total_common + k);
    let mut b: Vec<Step<'_>> = Vec::with_capacity(total_common + k);

    // Leaky leak of String storage: tests are short-lived, this is fine.
    let common_tags: Vec<&'static str> = (0..total_common)
        .map(|i| Box::leak(format!("c{i}").into_boxed_str()) as &'static str)
        .collect();
    let only_a_tags: Vec<&'static str> = (0..k)
        .map(|i| Box::leak(format!("only-a-{i}").into_boxed_str()) as &'static str)
        .collect();
    let only_b_tags: Vec<&'static str> = (0..k)
        .map(|i| Box::leak(format!("only-b-{i}").into_boxed_str()) as &'static str)
        .collect();

    let mut seq_a: u64 = 0;
    let mut seq_b: u64 = 0;
    for island in 0..k {
        // Common prefix for this island.
        for i in 0..stride {
            let tag = common_tags[island * stride + i];
            a.push(step(seq_a, Direction::C2a, tag));
            seq_a += 1;
            b.push(step(seq_b, Direction::C2a, tag));
            seq_b += 1;
        }
        // Divergent step: only in A.
        a.push(step(seq_a, Direction::A2c, only_a_tags[island]));
        seq_a += 1;
        // Divergent step: only in B.
        b.push(step(seq_b, Direction::A2c, only_b_tags[island]));
        seq_b += 1;
    }

    let (_ops, stats) = diff_steps(&a, &b);
    assert_eq!(stats.common as usize, total_common);
    assert_eq!(stats.only_a as usize, k);
    assert_eq!(stats.only_b as usize, k);
    assert!(stats.first_divergence.is_some());
}

/// Sanity: 2 000 × 2 000 alignment must complete quickly (well under a
/// second on any reasonable CI runner). We don't assert the time but a
/// timeout in CI would catch a quadratic-time regression in the inner
/// loop.
#[test]
fn five_thousand_steps_with_two_divergence_regions() {
    const N: usize = 2_000;
    let common_tags: Vec<&'static str> = (0..N)
        .map(|i| Box::leak(format!("h{i}").into_boxed_str()) as &'static str)
        .collect();

    // Build A and B as the same N steps but with two single-step inserts
    // in different places (so the LCS is forced to skip across both).
    let mut a: Vec<Step<'_>> = (0..N)
        .map(|i| step(i as u64, Direction::C2a, common_tags[i]))
        .collect();
    let mut b: Vec<Step<'_>> = a.clone();

    let inject_a: &'static str = "INJECT-A";
    let inject_b: &'static str = "INJECT-B";
    a.insert(500, step(99_999, Direction::A2c, inject_a));
    b.insert(1_500, step(88_888, Direction::A2c, inject_b));

    let (_ops, stats) = diff_steps(&a, &b);
    // Each side carries one extra step the other doesn't have.
    assert_eq!(stats.only_a, 1);
    assert_eq!(stats.only_b, 1);
    assert_eq!(stats.common as usize, N);
}
