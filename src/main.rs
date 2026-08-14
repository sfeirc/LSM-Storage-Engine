//! Small CLI demo of the LSM-Tree engine: writes a batch of keys (crossing a
//! flush boundary), reads some back, deletes one, flushes, compacts, and
//! prints what happened at each stage. Useful as a smoke test (this is what
//! the Docker image runs by default) and as a runnable usage example.
//!
//! Usage: `lsm-cli [data-dir]` — defaults to a fresh temp directory if no
//! directory is given.

use lsm_storage_engine::{LsmOptions, LsmTree};
use std::env;
use std::path::PathBuf;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let dir: PathBuf = match env::args().nth(1) {
        Some(d) => PathBuf::from(d),
        None => {
            let tmp = tempfile_dir()?;
            println!("no directory given, using temp dir: {}", tmp.display());
            tmp
        }
    };

    // Small memtable threshold so this short demo actually exercises a
    // flush and a compaction, not just the in-memory path.
    let opts = LsmOptions {
        memtable_max_bytes: 512,
        compaction_trigger: 3,
        ..LsmOptions::default()
    };

    let mut tree = LsmTree::open(&dir, opts)?;
    println!("opened LSM-Tree at {}", dir.display());

    println!("\n-- writing 200 key/value pairs --");
    for i in 0..200u32 {
        let key = format!("user:{i:05}");
        let value = format!("{{\"id\":{i},\"name\":\"user-{i}\"}}");
        tree.put(key.as_bytes(), value.as_bytes())?;
    }
    println!(
        "after writes: memtable_len={} sstable_count={}",
        tree.memtable_len(),
        tree.sstable_count()
    );

    println!("\n-- read-after-write check --");
    for i in [0u32, 42, 199] {
        let key = format!("user:{i:05}");
        let value = tree.get(key.as_bytes())?;
        println!(
            "  get({key}) = {:?}",
            value.map(|v| String::from_utf8_lossy(&v).into_owned())
        );
    }

    println!("\n-- deleting user:00042 --");
    tree.delete(b"user:00042")?;
    println!("  get(user:00042) = {:?}", tree.get(b"user:00042")?);

    println!("\n-- forcing a flush --");
    tree.flush()?;
    println!(
        "sstable_count={} memtable_len={}",
        tree.sstable_count(),
        tree.memtable_len()
    );
    println!(
        "  get(user:00042) after flush = {:?}",
        tree.get(b"user:00042")?
    );

    println!("\n-- forcing a compaction --");
    let before = tree.sstable_count();
    tree.compact()?;
    println!("sstable_count: {before} -> {}", tree.sstable_count());
    println!(
        "  get(user:00042) after compaction = {:?}",
        tree.get(b"user:00042")?
    );
    println!(
        "  get(user:00000) after compaction = {:?}",
        tree.get(b"user:00000")?
            .map(|v| String::from_utf8_lossy(&v).into_owned())
    );

    println!("\ndemo complete.");
    Ok(())
}

fn tempfile_dir() -> std::io::Result<PathBuf> {
    let mut dir = env::temp_dir();
    let unique = format!(
        "lsm-cli-demo-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    dir.push(unique);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
