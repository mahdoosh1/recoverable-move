//! `Disk` implementation backed by a regular file or block device,
//! using `crate::gpt` for the partition table.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::gpt;
use crate::mover::{Error, Gpt as PartitionRange, SECTOR_SIZE};
use crate::traits::Disk;

pub struct FileDisk {
    file: File,
    partition_index: u32,
}

impl FileDisk {
    pub fn open<P: AsRef<Path>>(path: P, partition_index: u32) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .map_err(|e| Error::Io(format!("open {}: {e}", path.as_ref().display())))?;
        Ok(Self { file, partition_index })
    }

    pub fn partition_index(&self) -> u32 {
        self.partition_index
    }

    fn offset(lba: u64) -> u64 {
        lba * SECTOR_SIZE as u64
    }
}

impl Disk for FileDisk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.file
            .seek(SeekFrom::Start(Self::offset(lba)))
            .map_err(|e| Error::Io(format!("seek LBA {lba}: {e}")))?;
        self.file
            .read_exact(buf)
            .map_err(|e| Error::Io(format!("read LBA {lba}: {e}")))?;
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        self.file
            .seek(SeekFrom::Start(Self::offset(lba)))
            .map_err(|e| Error::Io(format!("seek LBA {lba}: {e}")))?;
        self.file
            .write_all(buf)
            .map_err(|e| Error::Io(format!("write LBA {lba}: {e}")))?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.file
            .sync_all()
            .map_err(|e| Error::Io(format!("fsync: {e}")))
    }

    fn get_partition(&mut self) -> Result<PartitionRange, Error> {
        let g = gpt::Gpt::read(&mut self.file)
            .map_err(|e| Error::Io(format!("read GPT: {e}")))?;
        let p = g
            .find_partition(self.partition_index)
            .ok_or_else(|| {
                Error::InvalidParams(format!(
                    "partition {} not present in GPT",
                    self.partition_index
                ))
            })?;
        Ok(PartitionRange {
            start: p.starting_lba,
            len: p.size_in_lbas(),
        })
    }

    fn set_partition(&mut self, range: PartitionRange) -> Result<(), Error> {
        if range.len == 0 {
            return Err(Error::InvalidParams("zero-length partition".into()));
        }
        let end = range
            .start
            .checked_add(range.len - 1)
            .ok_or_else(|| Error::InvalidParams("partition end overflows u64".into()))?;

        let mut g = gpt::Gpt::read(&mut self.file)
            .map_err(|e| Error::Io(format!("read GPT: {e}")))?;

        g.set_partition_lbas(self.partition_index, range.start, end)
            .map_err(|e| Error::Io(format!("update GPT entry: {e}")))?;

        g.write_to(&mut self.file)
            .map_err(|e| Error::Io(format!("write GPT: {e}")))?;
        Ok(())
    }

    fn is_sector_free(&mut self, lba: u64) -> Result<bool, Error> {
        let g = gpt::Gpt::read(&mut self.file)
            .map_err(|e| Error::Io(format!("read GPT: {e}")))?;

        if lba < g.first_usable_lba() || lba > g.last_usable_lba() {
            return Ok(false);
        }
        for p in &g.partitions {
            if p.index == self.partition_index {
                continue;
            }
            if lba >= p.starting_lba && lba <= p.ending_lba {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn is_range_free(&mut self, start: u64, end: u64) -> Result<bool, Error> {
        if start >= end {
            return Ok(true);
        }
        let g = gpt::Gpt::read(&mut self.file)
            .map_err(|e| Error::Io(format!("read GPT: {e}")))?;

        let last = end - 1;
        if start < g.first_usable_lba() || last > g.last_usable_lba() {
            return Ok(false);
        }
        for p in &g.partitions {
            if p.index == self.partition_index {
                continue;
            }
            if start <= p.ending_lba && last >= p.starting_lba {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
