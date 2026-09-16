//! Abstract interfaces between the mover and the outside world.

use crate::mover::{Error, Gpt};

/// Raw sector I/O plus partition-table access.
pub trait Disk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;
    fn get_partition(&mut self) -> Result<Gpt, Error>;
    fn set_partition(&mut self, gpt: Gpt) -> Result<(), Error>;

    /// True if `lba` may be used as the mover's metadata sector:
    /// inside the GPT's usable range, and not claimed by any partition
    /// other than the one this Disk is operating on.
    fn is_sector_free(&mut self, lba: u64) -> Result<bool, Error>;

    /// True if every sector in `start..end` (half-open) may be written:
    /// inside the GPT's usable range, and not claimed by any partition
    /// other than the one this Disk is operating on.
    fn is_range_free(&mut self, start: u64, end: u64) -> Result<bool, Error>;
}

/// Out-of-band mirror of the metadata sector.
///
/// The mirror is written *before* the sector, so it is never stale
/// relative to the disk.  It is load-bearing: without it, a crash inside
/// the resize phase is unrecoverable because the metadata sector has
/// already been overwritten by moved data.
pub trait MetaStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error>;
    /// `Ok(None)` means "no record here"; `Err` means the read itself
    /// failed and recovery must not treat the absence as authoritative.
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error>;
    fn remove(&mut self);
}
