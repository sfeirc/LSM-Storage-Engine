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

fn encode(record: &WalRecord) -> Vec<u8> {
    let mut buf = Vec::new();
    match record {
        WalRecord::Put(k, v) => {
            buf.push(0u8);
            buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
            buf.extend_from_slice(k);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            buf.extend_from_slice(v);
        }
        WalRecord::Delete(k) => {
            buf.push(1u8);
            buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
            buf.extend_from_slice(k);
        }
    }
    let checksum = fnv1a_32(&buf);
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf
}

pub struct Wal {
    path: PathBuf,
    file: File,
    sync_on_write: bool,
}

impl Wal {
    /// Open (creating if needed) a WAL file at `path` for appending.
    pub fn open(path: impl AsRef<Path>, sync_on_write: bool) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        Ok(Wal {
            path,
            file,
            sync_on_write,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. In durable mode this fsyncs before returning, so a
    /// successful `append` is a durability guarantee: the record survives a
    /// crash from this point on, even if the memtable update that follows
    /// never happens.
    pub fn append(&mut self, record: &WalRecord) -> io::Result<()> {
        let bytes = encode(record);
        self.file.write_all(&bytes)?;
        if self.sync_on_write {
            self.file.sync_data()?;
        }
        Ok(())
    }

    /// Replay every complete record in the WAL file at `path`, in order. A
    /// truncated/corrupt final record is silently discarded (see module
    /// docs) rather than treated as an error, since that's the expected
    /// shape of a crash mid-write, not a bug.
    pub fn replay(path: impl AsRef<Path>) -> io::Result<Vec<WalRecord>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut records = Vec::new();

        loop {
            let start_pos = reader.stream_position()?;
            match Self::read_one_record(&mut reader) {
                Ok(Some(record)) => records.push(record),
                Ok(None) => break, // clean EOF between records
                Err(_) => {
                    // Torn / corrupt tail record: rewind is unnecessary since
                    // we stop reading entirely; just stop replay here.
                    let _ = start_pos;
                    break;
                }
            }
        }
        Ok(records)
    }

    fn read_one_record<R: Read>(reader: &mut R) -> io::Result<Option<WalRecord>> {
        use io::ErrorKind;

        let mut tag_buf = [0u8; 1];
        match reader.read_exact(&mut tag_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let mut record_bytes = vec![tag_buf[0]];

        let read_u32 = |reader: &mut R, record_bytes: &mut Vec<u8>| -> io::Result<u32> {
            let mut b = [0u8; 4];
            reader.read_exact(&mut b)?;
            record_bytes.extend_from_slice(&b);
            Ok(u32::from_le_bytes(b))
        };

        let key_len = read_u32(reader, &mut record_bytes)? as usize;
        if key_len > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "key too large, likely corrupt",
            ));
        }
        let mut key = vec![0u8; key_len];
        reader.read_exact(&mut key)?;
        record_bytes.extend_from_slice(&key);

        let record = match tag_buf[0] {
            0 => {
                let value_len = read_u32(reader, &mut record_bytes)? as usize;
                if value_len > 256 * 1024 * 1024 {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "value too large, likely corrupt",
                    ));
                }
                let mut value = vec![0u8; value_len];
                reader.read_exact(&mut value)?;
                record_bytes.extend_from_slice(&value);
                WalRecord::Put(key, value)
            }
            1 => WalRecord::Delete(key),
            _ => return Err(io::Error::new(ErrorKind::InvalidData, "unknown WAL tag")),
        };

        let mut checksum_buf = [0u8; 4];
        reader.read_exact(&mut checksum_buf)?;
        let stored_checksum = u32::from_le_bytes(checksum_buf);
        let computed = fnv1a_32(&record_bytes);
        if stored_checksum != computed {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "WAL checksum mismatch",
            ));
        }
        Ok(Some(record))
    }

    /// Truncate this WAL back to empty (used right after a successful
    /// memtable flush, since the WAL's contents are now durably captured in
    /// the new SSTable and replaying them again would be redundant work —
    /// not incorrect, since re-applying the same puts/deletes to an already
    /// flushed memtable is idempotent, but unbounded WAL growth is not
    /// something a real system tolerates).
    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        Ok(())
    }
}
