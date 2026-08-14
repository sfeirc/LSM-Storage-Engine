//! Write-Ahead Log.
//!
//! Every mutation (`Put` or `Delete`) is appended to this log — and, in the
//! default (durable) mode, `fsync`'d to disk — *before* it is applied to the
//! in-memory memtable. If the process crashes before the memtable is flushed
//! to an SSTable, restarting and replaying the WAL from byte 0 reconstructs
//! exactly the memtable state that existed right before the crash.
//!
//! ## On-disk record format
//!
//! ```text
//! [tag: u8]           0 = Put, 1 = Delete
//! [key_len: u32 LE]
//! [key bytes]
//! [value_len: u32 LE] (Put only)
//! [value bytes]       (Put only)
//! [checksum: u32 LE]  FNV-1a-32 over every byte above, this record only
//! ```
//!
//! The checksum is what makes crash recovery honest rather than merely
//! optimistic: a crash can leave a **torn write** at the tail of the file —
//! a record that was only partially flushed to disk. On replay, the first
//! record whose checksum doesn't match (or that runs past EOF) is treated as
//! the torn tail and replay stops there, discarding that record and any
//! bytes after it. Every record before it is guaranteed complete and is
//! applied. This mirrors how real WAL implementations (e.g. RocksDB's
//! `log_reader`) handle a truncated final record.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

fn fnv1a_32(data: &[u8]) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET;
    for &b in data {
        hash ^= b as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
