//! Property tests for the RFC 8785 JSON Canonicalization Scheme implementation.
//!
//! We don't pull in `proptest` (would inflate the workspace's dependency
//! footprint), so we ship a tiny deterministic `xorshift64*` PRNG and a
//! recursive `serde_json::Value` generator. Two-hundred random inputs per
//! property, with a fixed seed, give us reproducible coverage with no
//! flake risk in CI.
//!
//! Properties checked:
//!
//! 1. **Idempotence.** `canonicalize(v) == canonicalize(parse(canonicalize(v)))`
//!    — i.e. round-tripping a canonical form is a no-op.
//! 2. **Order-invariance.** For every randomly key-permuted copy of `v`,
//!    `canonicalize` and `payload_hash` produce identical bytes / digest.
//! 3. **Reparse fidelity.** The canonical bytes are valid JSON and parse
//!    back into a value semantically equal to the input.
//! 4. **Sorted keys.** In the canonical output, every object's keys are
//!    strictly increasing by UTF-16 code-unit order.

use serde_json::{Map, Value};

use acp_wire::{canonicalize, payload_hash};

const SEED: u64 = 0xC0FFEE_BADC0DE;
const TRIALS: usize = 200;
const MAX_DEPTH: u32 = 4;

/// xorshift64* — deterministic, branchless, no_std-friendly.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn gen_range(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n.max(1)
    }
    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 0
    }
}

fn gen_string(rng: &mut Rng) -> String {
    // Mix ASCII letters, control chars, quotes, backslashes and a couple
    // of multi-byte UTF-8 scalars so the escape paths are exercised.
    const ALPHABET: &[char] = &[
        'a', 'b', 'z', 'A', 'Z', '0', '9', ' ', '"', '\\', '\n', '\t', '\u{0001}', 'é', '漢', '🦀',
    ];
    let len = rng.gen_range(6);
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        s.push(ALPHABET[rng.gen_range(ALPHABET.len())]);
    }
    s
}

fn gen_value(rng: &mut Rng, depth: u32) -> Value {
    // Bias against deep recursion: object/array probability drops with depth.
    let branchy = depth < MAX_DEPTH;
    let pick = rng.gen_range(if branchy { 7 } else { 5 });
    match pick {
        0 => Value::Null,
        1 => Value::Bool(rng.bool()),
        2 => Value::Number(serde_json::Number::from(rng.next_u64() as i64)),
        3 => Value::Number(serde_json::Number::from(-(rng.next_u64() as i64 / 2))),
        4 => Value::String(gen_string(rng)),
        5 => {
            let n = rng.gen_range(4);
            Value::Array((0..n).map(|_| gen_value(rng, depth + 1)).collect())
        }
        _ => {
            let n = rng.gen_range(4);
            let mut m = Map::new();
            for _ in 0..n {
                // Keys must be unique within an object for permute_keys to be
                // a true permutation; uniqueness is implicit via Map::insert.
                m.insert(gen_string(rng), gen_value(rng, depth + 1));
            }
            Value::Object(m)
        }
    }
}

/// Return a clone of `v` where every object has its key insertion order
/// shuffled. Array element order is preserved (RFC 8785 mandates this).
fn permute_keys(v: &Value, rng: &mut Rng) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m
                .iter()
                .map(|(k, v)| (k.clone(), permute_keys(v, rng)))
                .collect();
            // Fisher–Yates shuffle.
            for i in (1..entries.len()).rev() {
                let j = rng.gen_range(i + 1);
                entries.swap(i, j);
            }
            let mut out = Map::new();
            for (k, v) in entries {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| permute_keys(x, rng)).collect()),
        other => other.clone(),
    }
}

fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.encode_utf16();
    let mut bi = b.encode_utf16();
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => match x.cmp(&y) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            },
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (None, None) => return std::cmp::Ordering::Equal,
        }
    }
}

fn assert_keys_sorted(v: &Value, path: &str) {
    match v {
        Value::Object(m) => {
            let mut prev: Option<&String> = None;
            for (k, child) in m {
                if let Some(p) = prev {
                    assert!(
                        utf16_cmp(p, k) == std::cmp::Ordering::Less,
                        "keys not strictly sorted at {path}: {p:?} >= {k:?}",
                    );
                }
                prev = Some(k);
                assert_keys_sorted(child, &format!("{path}/{k}"));
            }
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                assert_keys_sorted(child, &format!("{path}[{i}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn canonicalize_is_idempotent() {
    let mut rng = Rng::new(SEED);
    for trial in 0..TRIALS {
        let v = gen_value(&mut rng, 0);
        let bytes_once = canonicalize(&v);
        let reparsed: Value = serde_json::from_slice(&bytes_once)
            .unwrap_or_else(|e| panic!("trial {trial}: canonical form did not reparse: {e}"));
        let bytes_twice = canonicalize(&reparsed);
        assert_eq!(
            bytes_once, bytes_twice,
            "trial {trial}: canonicalize not idempotent for {v:?}",
        );
    }
}

#[test]
fn canonicalize_is_invariant_under_key_permutation() {
    let mut rng = Rng::new(SEED ^ 0xA5A5);
    for trial in 0..TRIALS {
        let v = gen_value(&mut rng, 0);
        let permuted = permute_keys(&v, &mut rng);
        let a = canonicalize(&v);
        let b = canonicalize(&permuted);
        assert_eq!(
            a, b,
            "trial {trial}: key permutation changed canonical bytes\n  orig:     {v:?}\n  permuted: {permuted:?}",
        );
        assert_eq!(payload_hash(&v), payload_hash(&permuted));
    }
}

#[test]
fn canonical_output_has_strictly_sorted_object_keys() {
    let mut rng = Rng::new(SEED ^ 0x5A5A);
    for _ in 0..TRIALS {
        let v = gen_value(&mut rng, 0);
        let bytes = canonicalize(&v);
        let reparsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_keys_sorted(&reparsed, "$");
    }
}

#[test]
fn canonical_output_reparses_to_equal_value() {
    let mut rng = Rng::new(SEED ^ 0xDEAD);
    for trial in 0..TRIALS {
        let v = gen_value(&mut rng, 0);
        let bytes = canonicalize(&v);
        let reparsed: Value = serde_json::from_slice(&bytes).unwrap();
        // serde_json::Value equality is order-insensitive for objects, so
        // this checks structural / scalar fidelity.
        assert_eq!(
            reparsed, v,
            "trial {trial}: canonical bytes did not roundtrip"
        );
    }
}
