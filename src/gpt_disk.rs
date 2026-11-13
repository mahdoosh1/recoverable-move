// Copyright (C) 2026 mahdoosh1
//
// This file is part of recoverable-move.
//
// recoverable-move is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 2 of the License, or
// (at your option) any later version.
//
// recoverable-move is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with recoverable-move. If not, see <https://www.gnu.org/licenses/>.


//! `Disk` implementation backed by a seekable file and a GPT parsed by
//! [`gptman`]. This works with regular disk-image files as well as block-device
//! files opened with read/write permissions.
//!
//! The implementation uses gptman to parse and rewrite the GPT while raw
//! sector contents are accessed through seek/read/write operations. The file
//! handle is kept open for the lifetime of [`GptDisk`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use gptman::{GPT, GPTPartitionEntry};

use crate::traits::{Disk, Partition};
use crate::Error;

/// A disk image or block device whose partition table is managed by gptman.
///
/// `partition_number` uses GPT's conventional one-based numbering.
pub struct GptDisk {
    file: File,
    gpt: GPT,
    partition_number: u32,
    sector_size: usize,
}

impl GptDisk {
    /// Open a disk image or block device and load its GPT.
    /// Open `path` read/write and select a one-based GPT partition entry.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened, no GPT is found, or the
    /// requested entry is zero, out of range, or unused.
    pub fn open(path: impl AsRef<Path>, partition_number: u32) -> Result<Self, Error> {
        if partition_number == 0 {
            return Err(Error::invalid("GPT partition numbers start at 1"));
        }
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let gpt = GPT::find_from(&mut file).map_err(|e| Error::table(e.to_string()))?;
        let sector_size = usize::try_from(gpt.sector_size)
            .map_err(|_| Error::table("sector size does not fit in usize"))?;
        if partition_number > gpt.header.number_of_partition_entries {
            return Err(Error::invalid("GPT partition number is out of range"));
        }
        if gpt[partition_number].is_unused() {
            return Err(Error::invalid("selected GPT entry is unused"));
        }
        Ok(Self { file, gpt, partition_number, sector_size })
    }

    /// Borrow the parsed GPT for inspection.
    /// Borrow the currently loaded GPT structure for read-only inspection.
    pub fn gpt(&self) -> &GPT {
        &self.gpt
    }

    /// Return the selected partition entry.
    fn entry(&self) -> &GPTPartitionEntry {
        &self.gpt[self.partition_number]
    }

    /// Mutably access the selected partition entry.
    fn entry_mut(&mut self) -> &mut GPTPartitionEntry {
        &mut self.gpt[self.partition_number]
    }

    /// Serialize the in-memory GPT and flush it to the backing file.
    fn write_table(&mut self) -> Result<(), Error> {
        self.gpt
            .write_into(&mut self.file)
            .map_err(|e| Error::table(e.to_string()))?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Convert an LBA into a checked byte offset.
    fn byte_offset(&self, lba: u64) -> Result<u64, Error> {
        lba.checked_mul(self.gpt.sector_size)
            .ok_or_else(|| Error::invalid("LBA byte offset overflows"))
    }
}

impl Disk for GptDisk {
    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        if buf.len() != self.sector_size {
            return Err(Error::invalid("read buffer must be exactly one sector"));
        }
        self.file.seek(SeekFrom::Start(self.byte_offset(lba)?))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        if buf.len() != self.sector_size {
            return Err(Error::invalid("write buffer must be exactly one sector"));
        }
        self.file.seek(SeekFrom::Start(self.byte_offset(lba)?))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.file.sync_all()?;
        Ok(())
    }

    fn partition(&mut self) -> Result<Partition, Error> {
        let entry = self.entry();
        let len = entry.ending_lba.checked_sub(entry.starting_lba)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Error::table("selected partition has an invalid extent"))?;
        Ok(Partition {
            start: entry.starting_lba,
            len,
            type_guid: entry.partition_type_guid,
        })
    }

    fn set_partition(&mut self, start: u64, len: u64) -> Result<(), Error> {
        if len == 0 {
            return Err(Error::invalid("partition length is zero"));
        }
        let end = start.checked_add(len - 1)
            .ok_or_else(|| Error::invalid("partition end overflows"))?;
        self.entry_mut().starting_lba = start;
        self.entry_mut().ending_lba = end;
        self.write_table()
    }

    fn set_type_guid(&mut self, guid: [u8; 16]) -> Result<(), Error> {
        self.entry_mut().partition_type_guid = guid;
        self.write_table()
    }

    fn range_is_free(&mut self, start: u64, len: u64) -> Result<bool, Error> {
        if len == 0 {
            return Ok(true);
        }
        let Some(end_exclusive) = start.checked_add(len) else {
            return Ok(false);
        };
        if start < self.gpt.header.first_usable_lba
            || end_exclusive - 1 > self.gpt.header.last_usable_lba
        {
            return Ok(false);
        }
        for (_number, entry) in self.gpt.iter() {
            if entry.is_unused() {
                continue;
            }
            if start <= entry.ending_lba && entry.starting_lba < end_exclusive {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
