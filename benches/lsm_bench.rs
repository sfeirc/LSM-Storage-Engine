//! Criterion benchmarks: write throughput, read throughput (bloom filter
//! enabled vs. disabled, the required A/B comparison), and compaction
//! latency. Every number quoted in the README's benchmark section came from
//! `cargo bench --bench lsm_bench` on the machine described there.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use lsm_storage_engine::{LsmOptions, LsmTree};
use std::hint::black_box;
use tempfile::tempdir;

fn key(i: u64) -> Vec<u8> {
    format!("key-{i:010}").into_bytes()
}
fn value(i: u64) -> Vec<u8> {
    // ~100 bytes, roughly representative of a small JSON row.
    format!("{{\"id\":{i},\"payload\":\"{:080}\"}}", 0).into_bytes()
}

/// Write throughput, with the WAL fsync'd on every write (the durable
/// default) vs. not synced at all -- this isolates exactly how much of
/// put()'s cost is the fsync itself vs. everything else (encoding,
/// memtable insert, occasional flush).
fn bench_write_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_throughput");
    for &sync in &[true, false] {
        group.bench_with_input(
            BenchmarkId::new(if sync { "wal_sync_on" } else { "wal_sync_off" }, 2000),
            &2000u64,
            |b, &n| {
                b.iter_batched(
                    || {
                        let dir = tempdir().unwrap();
                        let opts = LsmOptions {
                            wal_sync: sync,
                            ..LsmOptions::default()
                        };
                        let tree = LsmTree::open(dir.path(), opts).unwrap();
                        (dir, tree)
                    },
                    |(_dir, mut tree)| {
                        for i in 0..n {
                            tree.put(&key(i), &value(i)).unwrap();
                        }
                        black_box(&tree);
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Read throughput for keys that are **absent** (the case bloom filters are
/// specifically designed to speed up), across several already-flushed
/// SSTables, with bloom filtering enabled vs. disabled -- the direct,
/// measured A/B comparison the project spec asks for.
fn bench_read_missing_keys_bloom_on_vs_off(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_missing_keys_bloom_on_vs_off");
    let n_per_table: u64 = 2000;
    let n_tables = 4;

    for &bloom_enabled in &[true, false] {
        let dir = tempdir().unwrap();
        let opts = LsmOptions {
            memtable_max_bytes: usize::MAX, // never auto-flush; we flush explicitly per table
            compaction_trigger: usize::MAX, // never auto-compact; we want n_tables distinct sstables
            bloom_enabled,
            ..LsmOptions::default()
        };
        let mut tree = LsmTree::open(dir.path(), opts).unwrap();
        for t in 0..n_tables {
            for i in 0..n_per_table {
                let k = t * n_per_table + i;
                tree.put(&key(k), &value(k)).unwrap();
            }
            tree.flush().unwrap();
        }
        assert_eq!(tree.sstable_count(), n_tables as usize);

        let label = if bloom_enabled {
            "bloom_on"
        } else {
            "bloom_off"
        };
        group.bench_function(label, |b| {
            let mut probe = n_tables * n_per_table; // guaranteed absent keys, just past the inserted range
            b.iter(|| {
                let result = tree.get(&key(probe)).unwrap();
                probe += 1;
                black_box(result)
            });
        });
    }
    group.finish();
}

/// Read throughput for keys that **are** present, same on/off comparison,
/// to show the (much smaller, ideally near-zero) overhead bloom filters add
/// to the common "key exists" case.
fn bench_read_present_keys_bloom_on_vs_off(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_present_keys_bloom_on_vs_off");
    let n_per_table: u64 = 2000;
    let n_tables = 4;

    for &bloom_enabled in &[true, false] {
        let dir = tempdir().unwrap();
        let opts = LsmOptions {
            memtable_max_bytes: usize::MAX,
            compaction_trigger: usize::MAX,
            bloom_enabled,
            ..LsmOptions::default()
        };
        let mut tree = LsmTree::open(dir.path(), opts).unwrap();
        for t in 0..n_tables {
            for i in 0..n_per_table {
                let k = t * n_per_table + i;
                tree.put(&key(k), &value(k)).unwrap();
            }
            tree.flush().unwrap();
        }

        let label = if bloom_enabled {
            "bloom_on"
        } else {
            "bloom_off"
        };
        group.bench_function(label, |b| {
            let total = n_tables * n_per_table;
            let mut i = 0u64;
            b.iter(|| {
                let k = i % total;
                i += 1;
                black_box(tree.get(&key(k)).unwrap())
            });
        });
    }
    group.finish();
}

/// Compaction latency as a function of how much data (spread across
/// several sstables) is being merged.
fn bench_compaction_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("compaction_latency");
    for &total_keys in &[2_000u64, 10_000u64, 40_000u64] {
        group.bench_with_input(
            BenchmarkId::from_parameter(total_keys),
            &total_keys,
            |b, &total_keys| {
                b.iter_batched(
                    || {
                        let dir = tempdir().unwrap();
                        let opts = LsmOptions {
                            memtable_max_bytes: usize::MAX,
                            compaction_trigger: usize::MAX,
                            ..LsmOptions::default()
                        };
                        let mut tree = LsmTree::open(dir.path(), opts).unwrap();
                        let n_tables = 4u64;
                        let per_table = total_keys / n_tables;
                        for t in 0..n_tables {
                            for i in 0..per_table {
                                let k = t * per_table + i;
                                tree.put(&key(k), &value(k)).unwrap();
                            }
                            tree.flush().unwrap();
                        }
                        (dir, tree)
                    },
                    |(_dir, mut tree)| {
                        tree.compact().unwrap();
                        black_box(&tree);
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench_write_throughput,
        bench_read_missing_keys_bloom_on_vs_off,
        bench_read_present_keys_bloom_on_vs_off,
        bench_compaction_latency
}
criterion_main!(benches);
