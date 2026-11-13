//! Storage abstraction and optional durable record mirrors.
//!
//! Implement [`Disk`] to use the mover with another storage backend. Implement
//! [`RecordStore`] only when an additional metadata copy is desired.

use crate::Error;

/// One partition as the mover sees it: its sector extent and its type GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// First LBA in the partition.
    pub start: u64,
    /// Number of sectors in the partition.
    pub len: u64,
    /// GPT partition type GUID.
    pub type_guid: [u8; 16],
}

impl Partition {
    /// Return the exclusive end LBA (`start + len`).
    pub const fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// Sector I/O plus the two partition-table operations a move needs.
///
/// `set_partition` and `set_type_guid` must be durable when they return, so
/// the mover never has to know how a table is laid out.
/// Required interface to the disk being modified.
///
/// Implementations must make `set_partition` and `set_type_guid` durable before
/// returning. `sync` must flush prior sector writes. Sector buffers must be exactly
/// [`sector_size`](Self::sector_size) bytes long.
pub trait Disk {
    /// Return the physical/logical sector size in bytes.
    fn sector_size(&self) -> usize;
    /// Read exactly one sector at `lba` into `buf`.
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    /// Write exactly one sector from `buf` to `lba`.
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    /// Flush previous writes to stable storage.
    fn sync(&mut self) -> Result<(), Error>;

    /// Read the currently selected partition extent and type GUID.
    fn partition(&mut self) -> Result<Partition, Error>;
    /// Set the selected partition to `[start, start + len)` and persist the table.
    fn set_partition(&mut self, start: u64, len: u64) -> Result<(), Error>;
    /// Replace the selected partition type GUID and persist the table.
    fn set_type_guid(&mut self, guid: [u8; 16]) -> Result<(), Error>;

    /// Whether `[start, start + len)` holds no partition and lies inside the
    /// usable area of the disk.
    fn range_is_free(&mut self, start: u64, len: u64) -> Result<bool, Error>;
}

/// Durable copy of the record away from the disk being moved.
///
/// The mover writes this after both record sectors are on disk, so a store
/// is never newer than the disk. It is used only when both record sectors are
/// unreadable, and it is never required for the mover to be correct.
/// Optional durable mirror for the move record. The on-disk record remains primary.
pub trait RecordStore {
    /// Replace the mirrored record and make it durable before returning.
    fn write(&mut self, record: &[u8]) -> Result<(), Error>;
    /// Read the mirrored record, or return `None` when no record exists.
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error>;
    /// Remove the mirrored record after a successful move.
    fn clear(&mut self) -> Result<(), Error>;
}

/// [`RecordStore`] implementation that stores the encoded record in a file.
///
/// The file is replaced on each write and synced before the call returns.
pub struct FileStore {
    path: std::path::PathBuf,
}

impl FileStore {
    /// Create a file-backed store at `path`.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl RecordStore for FileStore {
    fn write(&mut self, record: &[u8]) -> Result<(), Error> {
        use std::io::Write;
        let mut file = std::fs::File::create(&self.path)?;
        file.write_all(record)?;
        file.sync_all()?;
        Ok(())
    }

    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        match std::fs::read(&self.path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn clear(&mut self) -> Result<(), Error> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default)]
/// In-memory [`RecordStore`] useful for tests and ephemeral workflows.
/// It does not survive process termination or power loss.
pub struct MemStore {
    data: Option<Vec<u8>>,
}

impl MemStore {
    /// Create an empty in-memory record store.
    pub const fn new() -> Self {
        Self { data: None }
    }
}

impl RecordStore for MemStore {
    fn write(&mut self, record: &[u8]) -> Result<(), Error> {
        self.data = Some(record.to_vec());
        Ok(())
    }

    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.data.clone())
    }

    fn clear(&mut self) -> Result<(), Error> {
        self.data = None;
        Ok(())
    }
}
