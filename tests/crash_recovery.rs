//! Crash recovery, tested against a *real* OS-level crash, not a simulated
//! one: this spawns the `crash_worker` binary as a child process, lets it
//! write a deterministic sequence of key/value pairs (each confirmed
//! durable via a `PROGRESS <i>` line printed only after that write's WAL
//! append — including its `fsync` — has returned), then sends it a real
//! `SIGKILL` via `std::process::Child::kill()` with no graceful shutdown of
//! any kind. Reopening the same directory must recover every write that was
//! reported durable before the kill.
//!
//! A second test below exercises a different, more subtle crash window:
//! a crash landing partway through compaction's old-file cleanup step.

use lsm_storage_engine::compaction::merge_tables_dropping_tombstones;
use lsm_storage_engine::{LsmOptions, LsmTree, SsTable};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tempfile::tempdir;

#[test]
fn real_sigkill_then_reopen_recovers_every_confirmed_write() {
    let dir = tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();

    let mut child = Command::new(env!("CARGO_BIN_EXE_crash_worker"))
        .arg(&dir_path)
        .arg("2000")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn crash_worker");

    let stdout = child.stdout.take().expect("child stdout");
    let reader = BufReader::new(stdout);
    let kill_after: u32 = 300;
    let mut last_confirmed: Option<u32> = None;

    for line in reader.lines() {
        let line = line.expect("read child stdout line");
        if let Some(rest) = line.strip_prefix("PROGRESS ") {
            let n: u32 = rest.trim().parse().expect("parse progress number");
            last_confirmed = Some(n);
            if n >= kill_after {
                break;
            }
        }
    }

    // Real, abrupt process kill -- SIGKILL on unix, no chance for any
    // destructor, buffered-writer flush, or graceful shutdown to run.
    child.kill().expect("SIGKILL the child");
    let _ = child.wait();

    let confirmed =
        last_confirmed.expect("should have seen at least one PROGRESS line before killing");
    assert!(
        confirmed >= kill_after,
        "expected to observe at least {kill_after} confirmed writes before killing, saw {confirmed}"
    );

    // Recovery: reopen the same directory fresh. Every key up to `confirmed`
    // was fsync'd to the WAL (or already flushed into an sstable) before
    // the kill, so this is a hard guarantee, not a best-effort hope.
    let recovered = LsmTree::open(&dir_path, LsmOptions::default()).expect("reopen after crash");
    for i in 0..=confirmed {
        let key = format!("k{i:06}");
        let expected = format!("v{i}-crash-worker-payload");
        let got = recovered.get(key.as_bytes()).expect("get after recovery");
        assert_eq!(
            got,
            Some(expected.into_bytes()),
            "key {key} was confirmed durable before the crash but did not survive recovery"
        );
    }
}

#[test]
fn compaction_partial_cleanup_is_still_safe_at_every_reachable_crash_point() {
    // Directly build the on-disk state compact() would produce partway
    // through its old-file cleanup, at each of the 3 states a crash could
    // actually leave behind (compact() deletes old files oldest-first, so
    // those -- and only those -- are the reachable intermediate states).
    let dir = tempdir().unwrap();
    let d = dir.path();

    // Table 1 (older): x = "old-value".
    let t1_path = d.join("000001.sst");
    let t1 = SsTable::build(
        &t1_path,
        1,
        vec![(b"x".to_vec(), Some(b"old-value".to_vec()))],
        4,
        10,
        true,
    )
    .unwrap();
    // Table 2 (newer): x deleted.
    let t2_path = d.join("000002.sst");
    let t2 = SsTable::build(&t2_path, 2, vec![(b"x".to_vec(), None)], 4, 10, true).unwrap();

    // Merge newest-first (t2, t1): x's newest state is a tombstone, so it's
    // dropped entirely from the compacted output.
    let merged = merge_tables_dropping_tombstones(&[&t2, &t1]).unwrap();
    assert!(
        merged.is_empty(),
        "the only key ever written was deleted; nothing should survive the merge"
    );
    let t3_path = d.join("000003.sst");
    let _t3 = SsTable::build(
        &t3_path,
        3,
        merged.into_iter().map(|(k, v)| (k, Some(v))),
        4,
        10,
        true,
    )
    .unwrap();

    // State A: crash before any cleanup at all -- t1, t2, t3 all present.
    {
        let tree = LsmTree::open(d, LsmOptions::default()).unwrap();
        assert_eq!(
            tree.get(b"x").unwrap(),
            None,
            "all 3 files present: still correctly deleted"
        );
    }

    // State B: crash after deleting the *oldest* file only (t1) -- this is
    // the only single-file-deleted state real compact() can leave behind,
    // since it deletes ascending by id. t2's tombstone is still present.
    std::fs::remove_file(&t1_path).unwrap();
    {
        let tree = LsmTree::open(d, LsmOptions::default()).unwrap();
        assert_eq!(
            tree.get(b"x").unwrap(),
            None,
            "t1 (stale value) gone, t2 (tombstone) + t3 present: still correctly deleted"
        );
    }

    // State C: crash after both old files are gone -- cleanup fully done.
    std::fs::remove_file(&t2_path).unwrap();
    {
        let tree = LsmTree::open(d, LsmOptions::default()).unwrap();
        assert_eq!(
            tree.get(b"x").unwrap(),
            None,
            "only t3 present: correctly has no entry for x at all"
        );
    }
}
