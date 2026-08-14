//! Helper binary used only by `tests/crash_recovery.rs`.
//!
//! Opens an `LsmTree` at the given directory and writes a deterministic,
//! ever-growing sequence of key/value pairs, printing `PROGRESS <i>` (and
//! flushing stdout) immediately after each write's WAL append has returned
//! — i.e. after that write is `fsync`'d and durable. It never shuts down
//! gracefully; the test harness sends it a real `SIGKILL`
//! (`std::process::Child::kill`) at a point of its choosing, then reopens
//! the same directory and checks that every write reported as durable
//! before the kill actually survived.
//!
//! `memtable_max_bytes` is set deliberately small so a real run flushes
//! several times before being killed — recovery then has to combine
//! already-flushed SSTables with a WAL replay of whatever came after the
//! last flush, not just one or the other.

use lsm_storage_engine::{LsmOptions, LsmTree};
use std::env;
use std::io::Write;

fn main() {
    let mut args = env::args().skip(1);
    let dir = args
        .next()
        .expect("usage: crash_worker <dir> <num_records>");
    let num_records: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);

    let opts = LsmOptions {
        memtable_max_bytes: 4096,
        compaction_trigger: 1_000_000, // keep compaction out of this test's way
        ..LsmOptions::default()
    };
    let mut tree = LsmTree::open(&dir, opts).expect("open lsm tree");

    let stdout = std::io::stdout();
    for i in 0..num_records {
        let key = format!("k{i:06}");
        let value = format!("v{i}-crash-worker-payload");
        tree.put(key.as_bytes(), value.as_bytes()).expect("put");

        let mut lock = stdout.lock();
        writeln!(lock, "PROGRESS {i}").expect("write progress line");
        lock.flush().expect("flush stdout");
    }
    println!("DONE");
}
