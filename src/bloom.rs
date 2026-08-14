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

    // --- serialization: [num_bits: u64][num_hashes: u32][word count: u64][words: u64 * n] ---

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20 + self.bits.len() * 8);
        buf.extend_from_slice(&self.num_bits.to_le_bytes());
        buf.extend_from_slice(&self.num_hashes.to_le_bytes());
        buf.extend_from_slice(&(self.bits.len() as u64).to_le_bytes());
        for word in &self.bits {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        buf
    }

    pub fn deserialize(buf: &[u8]) -> std::io::Result<Self> {
        use std::io::{Error, ErrorKind};
        if buf.len() < 20 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "bloom filter buffer too short",
            ));
        }
        let num_bits = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let num_hashes = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let word_count = u64::from_le_bytes(buf[12..20].try_into().unwrap()) as usize;
        let mut bits = Vec::with_capacity(word_count);
        let mut offset = 20;
        for _ in 0..word_count {
            if offset + 8 > buf.len() {
                return Err(Error::new(ErrorKind::InvalidData, "bloom filter truncated"));
            }
            bits.push(u64::from_le_bytes(
                buf[offset..offset + 8].try_into().unwrap(),
            ));
            offset += 8;
        }
        Ok(BloomFilter {
            bits,
            num_bits,
            num_hashes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives() {
        let mut bf = BloomFilter::new(1000, 10);
        let keys: Vec<Vec<u8>> = (0..1000u32).map(|i| i.to_le_bytes().to_vec()).collect();
        for k in &keys {
            bf.insert(k);
        }
        for k in &keys {
            assert!(bf.might_contain(k), "false negative for key {k:?}");
        }
    }

    #[test]
    fn false_positive_rate_is_reasonable() {
        // 10 bits/key should give a false-positive rate around 0.8-1% per the
        // standard formula; assert it's well under a loose 5% bound so this
        // doesn't flake, while still catching a broken implementation.
        let n = 5000usize;
        let mut bf = BloomFilter::new(n, 10);
        for i in 0..n as u32 {
            bf.insert(&i.to_le_bytes());
        }
        let mut false_positives = 0u32;
        let trials = 20_000u32;
        for i in 0..trials {
            let probe = (i + 10_000_000).to_le_bytes(); // guaranteed not inserted
            if bf.might_contain(&probe) {
                false_positives += 1;
            }
        }
        let rate = false_positives as f64 / trials as f64;
        assert!(rate < 0.05, "false positive rate too high: {rate}");
    }

    #[test]
    fn disabled_filter_always_says_maybe() {
        let bf = BloomFilter::disabled();
        assert!(bf.might_contain(b"anything"));
        assert!(!bf.is_enabled());
    }

    #[test]
    fn roundtrip_serialization() {
        let mut bf = BloomFilter::new(100, 8);
        for i in 0..100u32 {
            bf.insert(&i.to_le_bytes());
        }
        let bytes = bf.serialize();
        let bf2 = BloomFilter::deserialize(&bytes).unwrap();
        for i in 0..100u32 {
            assert!(bf2.might_contain(&i.to_le_bytes()));
        }
    }
}
