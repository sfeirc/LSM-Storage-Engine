//! A from-scratch Bloom filter.
//!
//! Used by each SSTable to answer "is this key *definitely absent*?" without
//! touching disk. A Bloom filter never produces a false negative (if it says
//! "absent", the key is truly absent) but can produce false positives (it may
//! say "maybe present" for a key that isn't there) — the false-positive rate
//! is a function of `bits_per_key` and is tuned below via the standard
//! formulas from Bloom (1970) / Mitzenmacher & Upfal.
//!
//! No external hashing crate is used: two independent 64-bit FNV-1a hashes
//! (with different seeds) are combined via Kirsch/Mitzenmacher double
//! hashing (`h_i = h1 + i*h2`) to derive the `k` bit positions per key, which
//! is the standard, well-analyzed way to avoid needing `k` fully independent
//! hash functions.

/// FNV-1a, 64-bit, with an explicit seed (offset basis). Deterministic and
/// dependency-free; not cryptographic, which is fine for a Bloom filter.
fn fnv1a_64(seed: u64, data: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut hash = seed;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

const SEED_1: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
const SEED_2: u64 = 0x1234_5678_9abc_def1; // arbitrary distinct seed
