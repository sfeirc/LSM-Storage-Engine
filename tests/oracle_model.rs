//! Model-based test: a long randomized sequence of put/delete/get operations
//! is run against both the real `LsmTree` and a plain `HashMap` used as a
//! reference oracle, comparing every `get` result against the oracle's
//! notion of the current state. Flushes and compactions are triggered
//! periodically *within* the same sequence, so the comparison exercises the
//! memtable, sstable, and post-compaction read paths, not just one of them.
//!
//! ## On "in parallel" against the oracle
//!
//! The task this engine was built against asks for this comparison to run
//! "in parallel" against a `HashMap`. Taken literally as *concurrent
//! multi-threaded* execution, that isn't meaningful here: `LsmTree` exposes
//! no interior mutability or locking and every mutation takes `&mut self`,
//! so it does not support concurrent writers at all (see the crate
//! README's "Honest scope" section) — bolting on fake concurrency just to
//! claim the word would test nothing real and would misrepresent what the
//! engine can do. What *is* implemented, and is the standard reading of
//! "test against a reference oracle" in model-based testing, is this:
//! both structures are advanced by the exact same sequential operation
//! stream and compared at every step (a sequential linearizability-against-
//! an-oracle check). A true concurrent linearizability checker (e.g.
//! generating concurrent histories and searching for a linearization) is
//! listed as unimplemented future work, not silently skipped.

use lsm_storage_engine::{LsmOptions, LsmTree};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;
use tempfile::tempdir;

#[test]
fn long_random_sequence_matches_hashmap_oracle_through_flushes_and_compactions() {
    let dir = tempdir().unwrap();
    let opts = LsmOptions {
        memtable_max_bytes: 300, // small: forces many flushes during the run
        compaction_trigger: 4,
        ..LsmOptions::default()
    };
    let mut tree = LsmTree::open(dir.path(), opts).unwrap();
    let mut oracle: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    let mut rng = StdRng::seed_from_u64(0xC0FFEE_u64);
    let key_space: u32 = 64; // small key space -> heavy overwrite/delete churn on the same keys
    let num_ops: u32 = 8000;

    for step in 0..num_ops {
        let key = format!("key{}", rng.gen_range(0..key_space)).into_bytes();
        let op: u8 = rng.gen_range(0..3);
        match op {
            0 => {
                let value = format!("v{step}").into_bytes();
                tree.put(&key, &value).unwrap();
                oracle.insert(key, value);
            }
            1 => {
                tree.delete(&key).unwrap();
                oracle.remove(&key);
            }
            _ => {
                let expected = oracle.get(&key).cloned();
                let actual = tree.get(&key).unwrap();
                assert_eq!(actual, expected, "mismatch at step {step} for key {key:?}");
            }
        }

        if step % 500 == 499 {
            tree.flush().unwrap();
        }
        if step % 1300 == 1299 {
            tree.compact().unwrap();
        }
    }

    // Final full sweep over the whole key space must match exactly too,
    // whatever storage state (memtable / sstables / post-compaction) the
    // run happened to end in.
    for k in 0..key_space {
        let key = format!("key{k}").into_bytes();
        assert_eq!(
            tree.get(&key).unwrap(),
            oracle.get(&key).cloned(),
            "final sweep mismatch for key {key:?}"
        );
    }
}

#[test]
fn shorter_sequence_with_bloom_filter_disabled_also_matches_oracle() {
    // Same idea, but with bloom_enabled=false, so this also incidentally
    // exercises correctness of the "no bloom filter" read path (used
    // separately for the benchmark A/B comparison) against the oracle, not
    // just the default bloom-enabled path.
    let dir = tempdir().unwrap();
    let opts = LsmOptions {
        memtable_max_bytes: 200,
        compaction_trigger: 3,
        bloom_enabled: false,
        ..LsmOptions::default()
    };
    let mut tree = LsmTree::open(dir.path(), opts).unwrap();
    let mut oracle: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    let mut rng = StdRng::seed_from_u64(42);
    let key_space: u32 = 32;
    let num_ops: u32 = 3000;

    for step in 0..num_ops {
        let key = format!("k{}", rng.gen_range(0..key_space)).into_bytes();
        let op: u8 = rng.gen_range(0..3);
        match op {
            0 => {
                let value = format!("v{step}").into_bytes();
                tree.put(&key, &value).unwrap();
                oracle.insert(key, value);
            }
            1 => {
                tree.delete(&key).unwrap();
                oracle.remove(&key);
            }
            _ => {
                assert_eq!(
                    tree.get(&key).unwrap(),
                    oracle.get(&key).cloned(),
                    "mismatch at step {step}"
                );
            }
        }
        if step % 400 == 399 {
            tree.flush().unwrap();
        }
    }
}
