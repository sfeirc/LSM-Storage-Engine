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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get() {
        let mut mt = Memtable::new();
        mt.put(b"k".to_vec(), b"v".to_vec());
        assert_eq!(mt.get(b"k"), Some(&Some(b"v".to_vec())));
    }

    #[test]
    fn delete_records_tombstone_not_absence() {
        let mut mt = Memtable::new();
        mt.delete(b"k".to_vec());
        assert_eq!(mt.get(b"k"), Some(&None));
        assert_eq!(mt.get(b"other"), None);
    }

    #[test]
    fn overwrite_updates_size_accounting() {
        let mut mt = Memtable::new();
        mt.put(b"k".to_vec(), b"short".to_vec());
        let size_after_first = mt.approx_size_bytes();
        mt.put(b"k".to_vec(), b"a-much-longer-value".to_vec());
        let size_after_second = mt.approx_size_bytes();
        assert!(size_after_second > size_after_first);
        // Exactly one logical entry, size = key + latest value only.
        assert_eq!(mt.len(), 1);
        assert_eq!(size_after_second, b"k".len() + b"a-much-longer-value".len());
    }

    #[test]
    fn iter_sorted_is_actually_sorted() {
        let mut mt = Memtable::new();
        for k in [b"c".to_vec(), b"a".to_vec(), b"b".to_vec()] {
            mt.put(k.clone(), k);
        }
        let keys: Vec<&Vec<u8>> = mt.iter_sorted().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![&b"a".to_vec(), &b"b".to_vec(), &b"c".to_vec()]);
    }
}
