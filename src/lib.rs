//! `lsm_storage_engine`: a from-scratch Rust implementation of an
//! **LSM-Tree** (Log-Structured Merge-Tree) key-value storage engine — the
//! write-optimized on-disk architecture behind RocksDB, Cassandra, and
//! LevelDB.
//!
//! See the crate's README for the full architecture write-up (write path,
//! read path, compaction, measured benchmarks) and honest scope/limitations.
//! This module only documents the pieces:
//!
//! - [`memtable`]: the in-memory sorted table that accepts writes first.
//! - [`wal`]: the write-ahead log every write is durably appended to before
//!   touching the memtable, and the replay logic that recovers it after a
//!   crash.
//! - [`sstable`]: the immutable, sorted, on-disk file format a memtable is
//!   flushed into, with a sparse index and a Bloom filter.
//! - [`bloom`]: the from-scratch Bloom filter used to skip disk reads for
//!   keys that are definitely absent from a given SSTable.
//! - [`compaction`]: the streaming k-way merge that folds multiple SSTables
//!   into one, resolving overwritten keys and dropping tombstones.
//! - [`lsm`]: [`LsmTree`], the top-level engine tying all of the above
//!   together behind a simple `put`/`get`/`delete` API.

pub mod bloom;
pub mod compaction;
pub mod lsm;
pub mod memtable;
pub mod sstable;
pub mod wal;

pub use bloom::BloomFilter;
pub use lsm::{LsmOptions, LsmTree};
pub use sstable::SsTable;
pub use wal::WalRecord;
