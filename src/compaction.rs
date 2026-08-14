//! Compaction: merging multiple SSTables into one.
//!
//! ## Strategy implemented: full merge ("major compaction")
//!
//! All currently-existing SSTables are merged into a single new SSTable in
//! one pass. This is the simplest correct compaction strategy — it is not
//! size-tiered or leveled (see README "Honest scope" for what that would
//! add) — but it is a *real* streaming k-way merge, not a placeholder: it
//! never materializes an entire table's contents in one `Vec`, only the
//! current head element of each input table's iterator.
//!
//! Because this merge always spans **every** currently-existing SSTable
//! (there is no older table left underneath), it is always safe to drop a
//! tombstone permanently once compaction sees it — nothing beneath this new
//! table could still need the tombstone to mask an older value, so a
//! deleted key's on-disk footprint (even the tombstone marker itself) goes
//! to zero after one compaction. This is what `tests/tombstone.rs` proves
//! directly by scanning the merged table's raw contents.
//!
//! ## Version resolution
//!
//! Tables are merged newest-first: when the same key appears in more than
//! one input table, a min-heap ordered by `(key ascending, source rank
//! ascending)` guarantees the newest table's version of that key is popped
//! and emitted first; every subsequent pop of an already-emitted key from an
//! older table is silently discarded (its iterator is still advanced, so
//! forward progress is guaranteed) rather than re-emitted.

use crate::sstable::{SsTable, SsTableIterator};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;

struct HeapItem {
    key: Vec<u8>,
    value: Option<Vec<u8>>,
    rank: usize, // 0 = newest source table
    source: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.rank == other.rank
    }
}
impl Eq for HeapItem {}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; we want it to pop the item with the
        // *smallest* key, and among equal keys the *smallest* rank (i.e.
        // the newest table). Reversing both comparisons achieves that.
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.rank.cmp(&self.rank))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
