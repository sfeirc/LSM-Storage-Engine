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

#[derive(Debug, Clone)]
pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: u64,
    num_hashes: u32,
}

impl BloomFilter {
    /// Build a filter sized for `expected_items` entries at roughly
    /// `bits_per_key` bits of storage per key. The classic optimum number of
    /// hash functions for a given bits-per-key ratio is `k = ln(2) *
    /// (bits_per_key)`, which is what's used here.
    pub fn new(expected_items: usize, bits_per_key: usize) -> Self {
        let expected_items = expected_items.max(1);
        let bits_per_key = bits_per_key.max(1);
        let num_bits = (expected_items * bits_per_key).max(64) as u64;
        let num_words = num_bits.div_ceil(64);
        let num_hashes =
            (((bits_per_key as f64) * std::f64::consts::LN_2).round() as u32).clamp(1, 30);
        BloomFilter {
            bits: vec![0u64; num_words as usize],
            num_bits: num_words * 64,
            num_hashes,
        }
    }

    /// An "always says maybe" filter — used when bloom filtering is disabled
    /// via `LsmOptions::bloom_enabled = false`, so the read path always falls
    /// through to the sparse index / data scan, for the A/B benchmark.
    pub fn disabled() -> Self {
        BloomFilter {
            bits: Vec::new(),
            num_bits: 0,
            num_hashes: 0,
        }
    }

    fn positions(&self, key: &[u8]) -> impl Iterator<Item = u64> + '_ {
        let h1 = fnv1a_64(SEED_1, key);
        let h2 = fnv1a_64(SEED_2, key).wrapping_mul(2).wrapping_add(1); // ensure odd -> full cycle mod power-of-two-ish sizes
        let num_bits = self.num_bits;
        (0..self.num_hashes as u64).map(move |i| {
            if num_bits == 0 {
                0
            } else {
                h1.wrapping_add(i.wrapping_mul(h2)) % num_bits
            }
        })
    }

    pub fn insert(&mut self, key: &[u8]) {
        if self.num_bits == 0 {
            return;
        }
        let positions: Vec<u64> = self.positions(key).collect();
        for pos in positions {
            let word = (pos / 64) as usize;
            let bit = pos % 64;
            self.bits[word] |= 1u64 << bit;
        }
    }

    /// `true` means "maybe present, go check"; `false` means "definitely
    /// absent, skip the disk read entirely".
    pub fn might_contain(&self, key: &[u8]) -> bool {
        if self.num_bits == 0 {
            return true; // disabled filter: never rules anything out
        }
        for pos in self.positions(key) {
            let word = (pos / 64) as usize;
            let bit = pos % 64;
            if self.bits[word] & (1u64 << bit) == 0 {
                return false;
            }
        }
        true
    }

    pub fn is_enabled(&self) -> bool {
        self.num_bits > 0
    }
