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

impl SsTable {
    /// Build a new SSTable file from an already-sorted, already-deduplicated
    /// iterator of `(key, value_or_tombstone)` pairs (the memtable's sorted
    /// iteration order, or a compaction merge's output order). Written to a
    /// temp file and renamed into place atomically, so a crash mid-flush
    /// never leaves a half-written file visible under `path`.
    pub fn build<I>(
        path: impl AsRef<Path>,
        id: u64,
        entries: I,
        index_interval: usize,
        bloom_bits_per_key: usize,
        bloom_enabled: bool,
    ) -> io::Result<SsTable>
    where
        I: IntoIterator<Item = (Vec<u8>, Option<Vec<u8>>)>,
    {
        let path = path.as_ref().to_path_buf();
        let tmp_path = path.with_extension("sst.tmp");

        let entries: Vec<(Vec<u8>, Option<Vec<u8>>)> = entries.into_iter().collect();
        let mut bloom = if bloom_enabled {
            BloomFilter::new(entries.len().max(1), bloom_bits_per_key)
        } else {
            BloomFilter::disabled()
        };

        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        let mut sparse_index = Vec::new();
        let mut offset: u64 = 0;
        let mut min_key = None;
        let mut max_key = None;

        for (i, (key, value)) in entries.iter().enumerate() {
            if i % index_interval.max(1) == 0 {
                sparse_index.push(SparseIndexEntry {
                    key: key.clone(),
                    offset,
                });
            }
            if min_key.is_none() {
                min_key = Some(key.clone());
            }
            max_key = Some(key.clone());

            let mut buf =
                Vec::with_capacity(8 + key.len() + value.as_ref().map(|v| v.len()).unwrap_or(0));
            encode_entry(&mut buf, key, value);
            writer.write_all(&buf)?;
            offset += buf.len() as u64;

            bloom.insert(key);
        }
        let data_len = offset;

        // Index block.
        let mut index_buf = Vec::new();
        for entry in &sparse_index {
            index_buf.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
            index_buf.extend_from_slice(&entry.key);
            index_buf.extend_from_slice(&entry.offset.to_le_bytes());
        }
        writer.write_all(&index_buf)?;
        let index_len = index_buf.len() as u64;

        // Meta block: max_key + entry_count, persisted explicitly so a
        // reopen never needs to scan the data block to recover them.
        let mut meta_buf = Vec::new();
        match &max_key {
            Some(k) => {
                meta_buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
                meta_buf.extend_from_slice(k);
            }
            None => meta_buf.extend_from_slice(&0u32.to_le_bytes()),
        }
        meta_buf.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        writer.write_all(&meta_buf)?;
        let meta_len = meta_buf.len() as u64;

        // Bloom block.
        let bloom_buf = bloom.serialize();
        writer.write_all(&bloom_buf)?;
        let bloom_len = bloom_buf.len() as u64;

        // Footer.
        writer.write_all(&data_len.to_le_bytes())?;
        writer.write_all(&index_len.to_le_bytes())?;
        writer.write_all(&meta_len.to_le_bytes())?;
        writer.write_all(&bloom_len.to_le_bytes())?;
        writer.write_all(&MAGIC.to_le_bytes())?;

        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);

        fs::rename(&tmp_path, &path)?;

        Ok(SsTable {
            id,
            path,
            data_len,
            sparse_index,
            bloom,
            entry_count: entries.len(),
            min_key,
            max_key,
        })
    }

    /// Reopen an existing SSTable file written by a previous process (this
    /// is the persistence half of crash recovery: SSTables are already
    /// durable — flushed and `fsync`'d before being renamed into place — so
    /// recovery just needs to rediscover and re-index them, not replay
    /// anything).
    pub fn open(path: impl AsRef<Path>, id: u64) -> io::Result<SsTable> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        if file_len < FOOTER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sstable file too short",
            ));
        }

        file.seek(SeekFrom::Start(file_len - FOOTER_LEN))?;
        let mut footer = [0u8; FOOTER_LEN as usize];
        file.read_exact(&mut footer)?;
        let data_len = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let index_len = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        let meta_len = u64::from_le_bytes(footer[16..24].try_into().unwrap());
        let bloom_len = u64::from_le_bytes(footer[24..32].try_into().unwrap());
        let magic = u32::from_le_bytes(footer[32..36].try_into().unwrap());
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad sstable magic",
            ));
        }

        file.seek(SeekFrom::Start(data_len))?;
        let mut index_buf = vec![0u8; index_len as usize];
        file.read_exact(&mut index_buf)?;
        let mut sparse_index = Vec::new();
        let mut off = 0usize;
        while off < index_buf.len() {
            let key_len = u32::from_le_bytes(index_buf[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            let key = index_buf[off..off + key_len].to_vec();
            off += key_len;
            let entry_offset = u64::from_le_bytes(index_buf[off..off + 8].try_into().unwrap());
            off += 8;
            sparse_index.push(SparseIndexEntry {
                key,
                offset: entry_offset,
            });
        }

        file.seek(SeekFrom::Start(data_len + index_len))?;
        let mut meta_buf = vec![0u8; meta_len as usize];
        file.read_exact(&mut meta_buf)?;
        let max_key_len = u32::from_le_bytes(meta_buf[0..4].try_into().unwrap()) as usize;
        let max_key = if max_key_len > 0 {
            Some(meta_buf[4..4 + max_key_len].to_vec())
        } else {
            None
        };
        let entry_count = u64::from_le_bytes(
            meta_buf[4 + max_key_len..12 + max_key_len]
                .try_into()
                .unwrap(),
        ) as usize;

        file.seek(SeekFrom::Start(data_len + index_len + meta_len))?;
        let mut bloom_buf = vec![0u8; bloom_len as usize];
        file.read_exact(&mut bloom_buf)?;
        let bloom = BloomFilter::deserialize(&bloom_buf)?;

        let min_key = sparse_index.first().map(|e| e.key.clone());

        Ok(SsTable {
            id,
            path,
            data_len,
            sparse_index,
            bloom,
            entry_count,
            min_key,
            max_key,
        })
    }

    /// Look up `key`. Returns `Ok(Some(Some(value)))` if present,
    /// `Ok(Some(None))` if this table records a tombstone for `key` (it was
    /// deleted at or before this table's generation), or `Ok(None)` if this
    /// table has no record of `key` at all (the caller should keep
    /// searching older tables).
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Option<Vec<u8>>>> {
        if !self.bloom.might_contain(key) {
            return Ok(None);
        }
        if self.sparse_index.is_empty() {
            return Ok(None);
        }
        // Rightmost sparse index entry with key <= target.
        let idx = match self
            .sparse_index
            .binary_search_by(|e| e.key.as_slice().cmp(key))
        {
            Ok(i) => i,
            Err(0) => return Ok(None), // target is before the first indexed key
            Err(i) => i - 1,
        };
        let start_offset = self.sparse_index[idx].offset;

        let mut file = File::open(&self.path)?;
        let mut cursor = start_offset;
        while cursor < self.data_len {
            let (entry_key, value, next) = read_entry_at_stream(&mut file, cursor)?;
            match entry_key.as_slice().cmp(key) {
                std::cmp::Ordering::Equal => return Ok(Some(value)),
                std::cmp::Ordering::Greater => return Ok(None), // sorted: passed where it would be
                std::cmp::Ordering::Less => {
                    cursor = next;
                }
            }
        }
        Ok(None)
    }
