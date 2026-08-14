//! The in-memory sorted table that accepts all writes.
//!
//! A `BTreeMap` is used rather than a hand-rolled skip list — the LSM-Tree
//! literature and this project's own spec treat both as equally valid
//! memtable structures. `BTreeMap` gives the same `O(log n)` insert/lookup
//! and, crucially, the same **sorted iteration order** that a flush to
//! SSTable needs (SSTables must be written in key order), with a
//! battle-tested implementation instead of a hand-rolled lock-free
//! structure. The honest trade-off (see README "Honest scope"): a skip list
//! is what you'd reach for if you needed lock-free concurrent readers/writers
//! on the memtable itself; this engine's memtable is only ever mutated
//! behind `&mut self` on `LsmTree`, so that concurrency property isn't
//! needed here and a `BTreeMap` is the simpler, equally-correct choice.

use std::collections::BTreeMap;

/// `None` represents a tombstone (a recorded delete), not "value absent" —
/// the memtable must distinguish "this key was deleted" from "I have no
/// opinion about this key" so that a delete can correctly mask an older
/// value sitting in an on-disk SSTable.
pub type MemtableValue = Option<Vec<u8>>;

#[derive(Default)]
pub struct Memtable {
    map: BTreeMap<Vec<u8>, MemtableValue>,
    approx_bytes: usize,
}

impl Memtable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.approx_bytes += key.len() + value.len();
        if let Some(old) = self.map.insert(key.clone(), Some(value)) {
            self.approx_bytes -= key.len() + old.map(|v| v.len()).unwrap_or(0);
        } else {
            self.approx_bytes -= 0; // no-op, kept for clarity of accounting
        }
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.approx_bytes += key.len();
        if let Some(old) = self.map.insert(key.clone(), None) {
            self.approx_bytes -= key.len() + old.map(|v| v.len()).unwrap_or(0);
        }
    }

    /// `Some(Some(value))` = present, `Some(None)` = tombstone (deleted),
    /// `None` = this memtable has no record of the key at all.
    pub fn get(&self, key: &[u8]) -> Option<&MemtableValue> {
        self.map.get(key)
    }

    pub fn iter_sorted(&self) -> impl Iterator<Item = (&Vec<u8>, &MemtableValue)> {
        self.map.iter()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn approx_size_bytes(&self) -> usize {
        self.approx_bytes
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.approx_bytes = 0;
    }
}
