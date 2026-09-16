//! The interfaces the mover uses to reach the outside world.

use crate::mover::{Error, Extent};

pub trait Disk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;
    fn get_partition(&mut self) -> Result<Extent, Error>;
    fn set_partition(&mut self, extent: Extent) -> Result<(), Error>;
    fn is_sector_free(&mut self, lba: u64) -> Result<bool, Error>;
    fn is_range_free(&mut self, start: u64, end: u64) -> Result<bool, Error>;
}

/// Out-of-band mirror of the metadata sector. Optional.
///
/// When present, the mirror is written *before* the on-disk sector, so it
/// is never stale relative to the disk. It is the only way to recover a
/// crash that lands inside the resize phase, because by then the on-disk
/// metadata has already been overwritten by moved data. When absent, a
/// crash inside resize is unrecoverable — that is the entire trade-off.
pub trait MetaStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error>;

    /// `Ok(None)` means "no record"; `Err` means the read itself failed
    /// and the caller must not treat the absence as authoritative.
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error>;

    /// Remove any stored record. Called after a successful move.
    ///
    /// Implementations should be best-effort: a failure to remove the
    /// record leaves it in place, which is safe because the next call to
    /// [`crate::mover::execute`] will re-enter the resize phase and redo
    /// the (idempotent) cleanup.
    fn remove(&mut self);
}

/// A store backed by a file. The file is written and fsynced before the
/// on-disk sector, so it is never stale.
pub struct FileStore {
    path: std::path::PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl MetaStore for FileStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        use std::io::Write;
        let mut file = std::fs::File::create(&self.path)?;
        file.write_all(data)?;
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

    fn remove(&mut self) {
        // Best-effort. A failed removal leaves the record in place and
        // recovery re-runs the resize phase, which is idempotent.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A store that lives in memory. For tests and for callers that already
/// have a durable channel of their own.
#[derive(Default)]
pub struct MemStore {
    data: Option<Vec<u8>>,
}

impl MemStore {
    pub const fn new() -> Self {
        Self { data: None }
    }
}

impl MetaStore for MemStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.data = Some(data.to_vec());
        Ok(())
    }

    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.data.clone())
    }

    fn remove(&mut self) {
        self.data = None;
    }
}
