//! A [`Disk`] backed by a regular file or block device, using
//! [`crate::gpt`] for the partition table.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::gpt;
use crate::mover::{Error, Extent, SECTOR_SIZE};
use crate::traits::Disk;

pub struct FileDisk {
    file: File,
    partition_index: u32,
}

impl FileDisk {
    pub fn open(path: impl AsRef<Path>, partition_index: u32) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .map_err(|e| {
                Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("open {}: {e}", path.as_ref().display()),
                ))
            })?;
        Ok(Self { file, partition_index })
    }

    pub const fn partition_index(&self) -> u32 {
        self.partition_index
    }

    fn offset(lba: u64) -> Result<u64, Error> {
        lba.checked_mul(SECTOR_SIZE as u64)
            .ok_or_else(|| Error::InvalidParams(format!("LBA {lba} overflows the disk offset")))
    }
}

impl Disk for FileDisk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(Self::offset(lba)?))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(Self::offset(lba)?))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.file.sync_all()?;
        Ok(())
    }

    fn get_partition(&mut self) -> Result<Extent, Error> {
        let gpt = gpt::Gpt::read(&mut self.file)?;
        let partition = gpt.find_partition(self.partition_index).ok_or_else(|| {
            Error::InvalidParams(format!(
                "partition {} not present in GPT",
                self.partition_index
            ))
        })?;
        Extent::new(partition.starting_lba, partition.ending_lba + 1)
    }

    fn set_partition(&mut self, extent: Extent) -> Result<(), Error> {
        if extent.is_empty() {
            return Err(Error::InvalidParams("zero-length partition".into()));
        }
        let end = extent.end().checked_sub(1).ok_or_else(|| {
            Error::InvalidParams("partition end underflows".into())
        })?;
        let mut gpt = gpt::Gpt::read(&mut self.file)?;
        gpt.set_partition_lbas(self.partition_index, extent.start(), end)?;
        gpt.write_to(&mut self.file)?;
        Ok(())
    }

    fn is_sector_free(&mut self, lba: u64) -> Result<bool, Error> {
        let gpt = gpt::Gpt::read(&mut self.file)?;
        Ok(is_free(&gpt, self.partition_index, lba, lba + 1))
    }

    fn is_range_free(&mut self, start: u64, end: u64) -> Result<bool, Error> {
        if start >= end {
            return Ok(true);
        }
        let gpt = gpt::Gpt::read(&mut self.file)?;
        Ok(is_free(&gpt, self.partition_index, start, end))
    }
}

fn is_free(gpt: &gpt::Gpt, our_index: u32, start: u64, end: u64) -> bool {
    if start < gpt.first_usable_lba() || end - 1 > gpt.last_usable_lba() {
        return false;
    }
    for partition in &gpt.partitions {
        if partition.index == our_index {
            continue;
        }
        if start <= partition.ending_lba && end >= partition.starting_lba {
            return false;
        }
    }
    true
}
