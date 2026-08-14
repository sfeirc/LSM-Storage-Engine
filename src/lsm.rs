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

impl LsmTree {
    /// Open (creating if needed) an LSM-Tree rooted at `dir`. If `dir`
    /// already contains SSTables and/or a WAL from a previous run, this is
    /// where crash recovery happens: existing SSTables are re-indexed (no
    /// data replay needed, they're immutable and were fsync'd+renamed
    /// before ever becoming visible) and the WAL is replayed into a fresh
    /// memtable to recover whatever writes hadn't been flushed yet.
    pub fn open(dir: impl AsRef<Path>, opts: LsmOptions) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        let sstable_files = discover_sstable_files(&dir)?;
        let mut sstables = Vec::with_capacity(sstable_files.len());
        let mut max_id: Option<u64> = None;
        for (id, path) in sstable_files {
            let table = SsTable::open(&path, id)?;
            max_id = Some(max_id.map_or(id, |m| m.max(id)));
            sstables.push(table);
        }

        let wal_path = dir.join("wal.log");
        let records = Wal::replay(&wal_path)?;
        let mut memtable = Memtable::new();
        for record in records {
            match record {
                WalRecord::Put(k, v) => memtable.put(k, v),
                WalRecord::Delete(k) => memtable.delete(k),
            }
        }
        let wal = Wal::open(&wal_path, opts.wal_sync)?;

        let next_sstable_id = max_id.map_or(0, |m| m + 1);

        Ok(LsmTree {
            dir,
            memtable,
            wal,
            sstables,
            next_sstable_id,
            opts,
        })
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.wal
            .append(&WalRecord::Put(key.to_vec(), value.to_vec()))?;
        self.memtable.put(key.to_vec(), value.to_vec());
        self.maybe_flush()
    }

    pub fn delete(&mut self, key: &[u8]) -> io::Result<()> {
        self.wal.append(&WalRecord::Delete(key.to_vec()))?;
        self.memtable.delete(key.to_vec());
        self.maybe_flush()
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if let Some(value) = self.memtable.get(key) {
            return Ok(value.clone());
        }
        for table in self.sstables.iter().rev() {
            if let Some(value) = table.get(key)? {
                return Ok(value);
            }
        }
        Ok(None)
    }

    fn maybe_flush(&mut self) -> io::Result<()> {
        if self.memtable.approx_size_bytes() >= self.opts.memtable_max_bytes {
            self.flush()?;
        }
        Ok(())
    }

    /// Force the current memtable to disk as a new SSTable, even if it
    /// hasn't hit the size threshold yet. No-op if the memtable is empty.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        let entries: Vec<(Vec<u8>, Option<Vec<u8>>)> = self
            .memtable
            .iter_sorted()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let id = self.next_sstable_id;
        self.next_sstable_id += 1;
        let path = self.dir.join(sstable_file_name(id));
        let table = SsTable::build(
            &path,
            id,
            entries,
            self.opts.sparse_index_interval,
            self.opts.bloom_bits_per_key,
            self.opts.bloom_enabled,
        )?;
        self.sstables.push(table);
        self.memtable.clear();
        // The WAL's contents are now durably captured in the SSTable that
        // was just fsync'd; truncating avoids unbounded WAL growth and
        // means a future crash only ever needs to replay writes since this
        // point, not the whole history.
        self.wal.truncate()?;

        self.maybe_compact()
    }

    fn maybe_compact(&mut self) -> io::Result<()> {
        if self.sstables.len() >= self.opts.compaction_trigger {
            self.compact()?;
        }
        Ok(())
    }

    /// Force a full compaction: merge every current SSTable into one new
    /// SSTable, resolving duplicate keys in favor of the newest version and
    /// permanently dropping any key whose newest version is a tombstone.
    /// No-op if there are no SSTables at all.
    pub fn compact(&mut self) -> io::Result<()> {
        if self.sstables.is_empty() {
            return Ok(());
        }
        let newest_first: Vec<&SsTable> = self.sstables.iter().rev().collect();
        let merged = compaction::merge_tables_dropping_tombstones(&newest_first)?;

        let new_id = self.next_sstable_id;
        self.next_sstable_id += 1;
        let new_path = self.dir.join(sstable_file_name(new_id));
        let new_table = SsTable::build(
            &new_path,
            new_id,
            merged.into_iter().map(|(k, v)| (k, Some(v))),
            self.opts.sparse_index_interval,
            self.opts.bloom_bits_per_key,
            self.opts.bloom_enabled,
        )?;

        // Delete the superseded files oldest-first (ascending id — which is
        // exactly the order `self.sstables` is already kept in). This
        // ordering is a deliberate crash-safety invariant, not an arbitrary
        // choice: for any key, a tombstone that shadows an older value
        // always lives in a *higher*-id (more recent) table than that
        // value. Deleting ascending guarantees a shadowing tombstone is
        // never removed while the value it shadows is still on disk — so a
        // crash at any point during this loop leaves a directory that,
        // read via `open`, still returns correct answers (verified in
        // `tests/crash_recovery.rs::compaction_partial_cleanup_is_still_safe`).
        for old_table in &self.sstables {
            let _ = fs::remove_file(old_table.path());
        }

        self.sstables = vec![new_table];
        Ok(())
    }
