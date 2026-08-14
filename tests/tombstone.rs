//! Deletes (tombstones) must mask older values immediately, and must be
//! physically, permanently removed by compaction — never just "hidden" —
//! and must never resurface across further, unrelated compaction cycles.

use lsm_storage_engine::{LsmOptions, LsmTree};
use tempfile::tempdir;

#[test]
fn delete_masks_value_before_any_compaction() {
    let dir = tempdir().unwrap();
    let mut tree = LsmTree::open(
        dir.path(),
        LsmOptions {
            compaction_trigger: 1_000_000,
            ..LsmOptions::default()
        },
    )
    .unwrap();
    tree.put(b"k", b"v").unwrap();
    tree.flush().unwrap(); // sstable A: k=v
    tree.delete(b"k").unwrap();
    tree.flush().unwrap(); // sstable B (newer): k=tombstone, sitting on top of A
    assert_eq!(tree.sstable_count(), 2);
    assert_eq!(tree.get(b"k").unwrap(), None);
}

#[test]
fn delete_within_memtable_before_any_flush() {
    let dir = tempdir().unwrap();
    let mut tree = LsmTree::open(dir.path(), LsmOptions::default()).unwrap();
    tree.put(b"k", b"v").unwrap();
    tree.delete(b"k").unwrap();
    assert_eq!(tree.get(b"k").unwrap(), None);
    assert_eq!(tree.memtable_len(), 1); // one logical entry: a tombstone, not "nothing"
}

#[test]
fn compaction_physically_removes_the_tombstone_not_just_the_value() {
    let dir = tempdir().unwrap();
    let mut tree = LsmTree::open(
        dir.path(),
        LsmOptions {
            compaction_trigger: 1_000_000,
            ..LsmOptions::default()
        },
    )
    .unwrap();

    tree.put(b"doomed", b"will-be-deleted").unwrap();
    tree.flush().unwrap();
    tree.delete(b"doomed").unwrap();
    tree.flush().unwrap();
    assert_eq!(tree.get(b"doomed").unwrap(), None);

    tree.compact().unwrap();
    assert_eq!(tree.sstable_count(), 1);
    assert_eq!(tree.get(b"doomed").unwrap(), None);

    // Not just masked: scan every raw entry across all sstables and confirm
    // there is no record at all (neither a value nor a tombstone) for the
    // deleted key.
    let raw = tree.debug_all_sstable_entries().unwrap();
    assert!(
        raw.iter().all(|(_, k, _)| k != b"doomed"),
        "tombstone (or value) for 'doomed' should be physically gone after compaction, found: {raw:?}"
    );
}

#[test]
fn deleted_key_never_reappears_across_several_more_compaction_cycles() {
    let dir = tempdir().unwrap();
    let mut tree = LsmTree::open(
        dir.path(),
        LsmOptions {
            compaction_trigger: 1_000_000,
            ..LsmOptions::default()
        },
    )
    .unwrap();

    tree.put(b"doomed", b"v0").unwrap();
    tree.flush().unwrap();
    tree.delete(b"doomed").unwrap();
    tree.flush().unwrap();
    tree.compact().unwrap();
    assert_eq!(tree.get(b"doomed").unwrap(), None);

    // Run several more unrelated put/flush/compact rounds. A regression
    // where compaction accidentally "resurrects" an older pre-tombstone
    // value (e.g. wrong merge precedence) would show up here.
    for round in 0..8u32 {
        tree.put(format!("filler{round}").as_bytes(), b"x").unwrap();
        tree.flush().unwrap();
        tree.compact().unwrap();
        assert_eq!(
            tree.get(b"doomed").unwrap(),
            None,
            "round {round}: deleted key resurfaced!"
        );
    }
    assert_eq!(tree.sstable_count(), 1);
}

#[test]
fn delete_of_a_key_that_never_existed_is_harmless() {
    let dir = tempdir().unwrap();
    let mut tree = LsmTree::open(dir.path(), LsmOptions::default()).unwrap();
    tree.delete(b"never-existed").unwrap();
    assert_eq!(tree.get(b"never-existed").unwrap(), None);
    tree.flush().unwrap();
    tree.compact().unwrap();
    assert_eq!(tree.get(b"never-existed").unwrap(), None);
}
