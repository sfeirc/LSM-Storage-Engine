//! The top-level LSM-Tree engine: coordinates the memtable, the WAL, the set
//! of on-disk SSTables, and compaction into a single `get`/`put`/`delete`
//! key-value store.
//!
//! ## Write path
//! `put`/`delete` → append to WAL (durable) → apply to memtable → if the
//! memtable has grown past `memtable_max_bytes`, flush it to a new SSTable
//! (and truncate the WAL, since its contents are now durably captured in
//! that SSTable) → if the number of SSTables has reached
//! `compaction_trigger`, run compaction.
//!
//! ## Read path
//! `get` checks the memtable first (newest data), then every SSTable from
//! newest to oldest, stopping at the first table that has *any* record for
//! the key — a `Some` (present) or a tombstone (deleted) both stop the
//! search; only "this table doesn't mention the key at all" continues to
//! the next, older table.

use crate::compaction;
use crate::memtable::Memtable;
use crate::sstable::SsTable;
use crate::wal::{Wal, WalRecord};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// (sstable id, key, value-or-tombstone) — see `debug_all_sstable_entries`.
pub type SstableEntryDebug = (u64, Vec<u8>, Option<Vec<u8>>);

#[derive(Clone, Debug)]
pub struct LsmOptions {
    /// Flush the memtable to a new SSTable once its approximate byte size
    /// (sum of key+value lengths of live entries) reaches this threshold.
    pub memtable_max_bytes: usize,
    /// Record one (key, offset) sparse-index entry every N entries in a
    /// newly built SSTable.
    pub sparse_index_interval: usize,
    /// Bits of Bloom filter storage per key per SSTable. ~10 bits/key gives
    /// roughly a 1% false-positive rate (see `bloom.rs` and the README
    /// benchmark section for the measured trade-off).
    pub bloom_bits_per_key: usize,
    /// Build (and consult) a Bloom filter for each SSTable at all. Set to
    /// `false` only to reproduce the "no bloom filter" side of the
    /// benchmark comparison — real usage should leave this `true`.
    pub bloom_enabled: bool,
    /// Run a full compaction once the number of on-disk SSTables reaches
    /// this count.
    pub compaction_trigger: usize,
    /// `fsync` the WAL file after every append. Durable and the honest
    /// default; disabling this trades durability (a crash can lose the last
    /// un-synced writes) for throughput — see the write-throughput
    /// benchmark for the measured cost of leaving this on.
    pub wal_sync: bool,
}

impl Default for LsmOptions {
    fn default() -> Self {
        LsmOptions {
            memtable_max_bytes: 4 * 1024 * 1024,
            sparse_index_interval: 32,
            bloom_bits_per_key: 10,
            bloom_enabled: true,
            compaction_trigger: 4,
            wal_sync: true,
        }
    }
}

pub struct LsmTree {
    dir: PathBuf,
    memtable: Memtable,
    wal: Wal,
    /// Ascending by id: `sstables[0]` is the oldest, `sstables.last()` the
    /// newest. Reads walk this in reverse.
    sstables: Vec<SsTable>,
    next_sstable_id: u64,
    opts: LsmOptions,
}

fn sstable_file_name(id: u64) -> String {
    format!("{id:06}.sst")
}

fn discover_sstable_files(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".sst") {
            if let Ok(id) = stem.parse::<u64>() {
                found.push((id, path));
                continue;
            }
        }
        if name.ends_with(".sst.tmp") {
            // Leftover from a flush/compaction interrupted mid-write: the
            // real file is only ever visible under its final name after an
            // atomic rename, so a `.tmp` file here is always garbage from
            // an incomplete write and is safe to discard.
            let _ = fs::remove_file(&path);
        }
    }
    found.sort_by_key(|(id, _)| *id);
    Ok(found)
}
