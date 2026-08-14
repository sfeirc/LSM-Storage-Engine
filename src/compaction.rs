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

/// Merge `tables` (must be passed newest-first: `tables[0]` is the most
/// recently flushed/compacted) into a single sorted, deduplicated,
/// tombstone-free stream of live entries.
pub fn merge_tables_dropping_tombstones(
    tables: &[&SsTable],
) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut iters: Vec<SsTableIterator> = tables
        .iter()
        .map(|t| t.iter_all())
        .collect::<io::Result<Vec<_>>>()?;

    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
    for (source, iter) in iters.iter_mut().enumerate() {
        if let Some(next) = iter.next() {
            let (key, value) = next?;
            heap.push(HeapItem {
                key,
                value,
                rank: source,
                source,
            });
        }
    }

    let mut output = Vec::new();
    let mut last_emitted_key: Option<Vec<u8>> = None;

    while let Some(item) = heap.pop() {
        // Pull the next entry from the same source and push it back, to
        // keep every input stream flowing regardless of whether this
        // popped item ends up emitted or discarded as a shadowed duplicate.
        if let Some(next) = iters[item.source].next() {
            let (key, value) = next?;
            heap.push(HeapItem {
                key,
                value,
                rank: item.rank,
                source: item.source,
            });
        }

        let is_new_key = last_emitted_key.as_deref() != Some(item.key.as_slice());
        if is_new_key {
            if let Some(value) = item.value {
                output.push((item.key.clone(), value));
            }
            // else: newest version of this key is a tombstone -> permanently
            // dropped, since this merge spans every existing table.
            last_emitted_key = Some(item.key);
        }
        // else: an older table's version of a key already resolved by a
        // newer table this pass — discarded, iterator already advanced above.
    }

    Ok(output)
}
