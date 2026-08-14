//! SSTable ("Sorted String Table"): the immutable on-disk file format a
//! memtable is flushed into, and that compaction merges.
//!
//! ## File layout
//!
//! ```text
//! [ DATA BLOCK  ]  sorted entries: [key_len:u32][value_len:u32|TOMBSTONE][key][value]
//! [ INDEX BLOCK ]  sparse index: one (key, offset) every `index_interval` entries
//! [ META BLOCK  ]  max_key_len:u32, max_key bytes, entry_count:u64
//! [ BLOOM BLOCK ]  serialized Bloom filter over every key in this table
//! [ FOOTER      ]  data_len:u64, index_len:u64, meta_len:u64, bloom_len:u64, magic:u32 (36 bytes, fixed size, at EOF)
//! ```
//!
//! The footer is fixed-size and always at the end, so opening an existing
//! file only requires one seek-to-end read to locate the index/meta/bloom
//! blocks — the data block itself is never scanned on open, only on a
//! `get()` that the bloom filter didn't reject, or during compaction's full
//! scan. `entry_count` and `max_key` are persisted explicitly in the meta
//! block rather than recomputed by scanning, so reopening a table is exact,
//! not an estimate.
//!
//! A value length of `u32::MAX` is a sentinel meaning "this entry is a
//! tombstone" (a recorded delete), not a real 4-GiB-sized value — see
//! "Honest scope" in the README for the resulting (harmless, in practice)
//! value-size ceiling.

use crate::bloom::BloomFilter;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const TOMBSTONE_SENTINEL: u32 = u32::MAX;
const MAGIC: u32 = 0x4C53_4D31; // "LSM1"
const FOOTER_LEN: u64 = 8 + 8 + 8 + 8 + 4;

struct SparseIndexEntry {
    key: Vec<u8>,
    offset: u64,
}

pub struct SsTable {
    pub id: u64,
    path: PathBuf,
    data_len: u64,
    sparse_index: Vec<SparseIndexEntry>,
    bloom: BloomFilter,
    pub entry_count: usize,
    pub min_key: Option<Vec<u8>>,
    pub max_key: Option<Vec<u8>>,
}

fn encode_entry(buf: &mut Vec<u8>, key: &[u8], value: &Option<Vec<u8>>) {
    buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
    match value {
        Some(v) => {
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            buf.extend_from_slice(key);
            buf.extend_from_slice(v);
        }
        None => {
            buf.extend_from_slice(&TOMBSTONE_SENTINEL.to_le_bytes());
            buf.extend_from_slice(key);
        }
    }
}
