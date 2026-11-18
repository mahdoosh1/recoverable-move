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

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::errors::Error;

#[derive(Clone, Copy, PartialEq)]
pub struct Partition {
    guid: [u8; 16],
    start: u64,
    len: u64,
    partition_label: [u8; 72],
}

impl Partition {
    pub fn new(start: u64, len: u64, guid: &[u8; 16], partition_label: &[u8; 72]) -> Self {
        Self {
            guid: *guid,
            start,
            len,
            partition_label: *partition_label,
        }
    }
    pub fn start(&self) -> u64 {
        self.start
    }
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn end(&self) -> u64 {
        self.start() + self.len()
    }
    pub fn guid(&self) -> &[u8; 16] {
        &self.guid
    }
    pub fn label(&self) -> &[u8; 72] {
        &self.partition_label
    }
    pub fn label_string(&self) -> String {
        String::from_utf16le_lossy(self.label())
    }
}

pub struct FileWriter {
    file: File,
}

impl FileWriter {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }
    pub fn read(&mut self, data: &mut [u8]) -> Result<(), Error> {
        self.file.read_exact(data)?;
        Ok(())
    }
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(data)?;
        self.file.sync_all()?;
        Ok(())
    }
}

// all disk read/writes can fail
// `read` through a `File` type will mutate the value, so we use &mut self instead of &self for read operations
pub trait Disk {
    fn sector_size(&self) -> usize;
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;
    fn list_partitions(&mut self) -> Result<Vec<Partition>, Error>; // ordered
    fn update_partition(&mut self, index: usize, partition: Partition) -> Result<(), Error>;
    fn is_range_free(&mut self, start_lba: u64, end_lba: u64) -> Result<bool, Error>;
}
