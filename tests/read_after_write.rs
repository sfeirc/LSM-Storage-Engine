//! Read-after-write consistency across every storage state the engine can
//! be in: memtable-only, after a flush to SSTable, after an overwrite while
//! an older SSTable still holds the previous version, and after compaction.

use lsm_storage_engine::{LsmOptions, LsmTree};
use tempfile::tempdir;

#[test]
fn read_after_write_across_memtable_sstable_and_compaction() {
    let dir = tempdir().unwrap();
    let opts = LsmOptions {
        memtable_max_bytes: 200,
        compaction_trigger: 2,
        ..LsmOptions::default()
    };
    let mut tree = LsmTree::open(dir.path(), opts).unwrap();

    // Stage 1: value lives only in the memtable.
    tree.put(b"alpha", b"1").unwrap();
    assert_eq!(tree.get(b"alpha").unwrap(), Some(b"1".to_vec()));

    // Stage 2: force a flush -> value now lives in an SSTable.
    tree.flush().unwrap();
    assert_eq!(tree.sstable_count(), 1);
    assert_eq!(tree.get(b"alpha").unwrap(), Some(b"1".to_vec()));

    // Overwrite while the old value still sits in an SSTable underneath.
    tree.put(b"alpha", b"2").unwrap();
    assert_eq!(tree.get(b"alpha").unwrap(), Some(b"2".to_vec())); // memtable shadows the sstable

    tree.flush().unwrap();
    assert_eq!(tree.get(b"alpha").unwrap(), Some(b"2".to_vec())); // still correct with 2 sstables now

    tree.put(b"beta", b"b").unwrap();
    tree.flush().unwrap();

    // Stage 3: after compaction merges everything into one sstable.
    tree.compact().unwrap();
    assert_eq!(tree.sstable_count(), 1);
    assert_eq!(tree.get(b"alpha").unwrap(), Some(b"2".to_vec()));
    assert_eq!(tree.get(b"beta").unwrap(), Some(b"b".to_vec()));
}

#[test]
fn many_keys_survive_flush_compact_and_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().to_path_buf();
    {
        let mut tree = LsmTree::open(
            &path,
            LsmOptions {
                memtable_max_bytes: 512,
                ..LsmOptions::default()
            },
        )
        .unwrap();
        for i in 0..500u32 {
            tree.put(format!("k{i:05}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        tree.flush().unwrap();
        tree.compact().unwrap();

        // read-after-write, still within the same process, right after compaction
        for i in 0..500u32 {
            let expected = format!("v{i}").into_bytes();
            assert_eq!(
                tree.get(format!("k{i:05}").as_bytes()).unwrap(),
                Some(expected)
            );
        }
    }

    // ... and again after a full close/reopen of the directory.
    let tree = LsmTree::open(&path, LsmOptions::default()).unwrap();
    for i in 0..500u32 {
        let expected = format!("v{i}").into_bytes();
        assert_eq!(
            tree.get(format!("k{i:05}").as_bytes()).unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn overwrite_many_times_before_any_flush_keeps_latest_value() {
    let dir = tempdir().unwrap();
    let mut tree = LsmTree::open(dir.path(), LsmOptions::default()).unwrap();
    for i in 0..50u32 {
        tree.put(b"hot-key", format!("v{i}").as_bytes()).unwrap();
        assert_eq!(
            tree.get(b"hot-key").unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
}
