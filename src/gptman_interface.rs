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

use std::{fs::{File, OpenOptions}, os::unix::fs::FileExt, path::Path};

use gptman::GPT;

use crate::{errors::Error, traits::{Disk, Partition}};

pub struct GptDisk {
    file: File,
    gpt: GPT,
    sector_size: usize
}

impl GptDisk {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let gpt = GPT::find_from(&mut file)?;
        let sector_size = gpt.sector_size as usize;
        Ok(Self {
            file,
            gpt,
            sector_size
        })
    }
}

impl Disk for GptDisk {
    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        if buf.len() != self.sector_size {
            return Err(Error::invalid("the buffer is not the same size as sector_size"))
        }
        self.file.read_exact_at(buf, lba * self.sector_size as u64)?;
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        if buf.len() != self.sector_size {
            return Err(Error::invalid("the buffer is not the same size as sector_size"))
        }
        self.file.write_all_at(buf, lba * self.sector_size as u64)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.file.sync_all()?;
        Ok(())
    }

    fn list_partitions(&mut self) -> Result<Vec<Partition>, Error> {
        let mut partitions = Vec::new();
        for (index, entry) in self.gpt.iter() {
            let index = index as usize;
            let mut partition_name: Vec<u8> = entry.partition_name
                .as_str()
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            partition_name.resize(72, 0);
            let partition_name: [u8; 72] = *partition_name
                .as_array()
                .unwrap(); // won't crash; trust me
            let partition = Partition::new(
                entry.starting_lba, 
                entry.size()?, 
                &entry.partition_type_guid,
                &partition_name
            );
            if index >= partitions.len() {
                partitions.resize_with(index + 1, || None);
            }
            if partitions[index].is_some() {
                return Err(Error::invalid(format!("multiple partitions with index {}", index)))
            }
            partitions[index] = Some(partition);
        }
        if let Some(index) = partitions.iter().position(|x| x.is_none()) {
            return Err(Error::invalid(format!("no partition with index {}", index)))
        }
        Ok(partitions.into_iter().map(Option::unwrap).collect())
    }

    fn update_partition(&mut self, index: usize, partition: Partition) -> Result<(), Error> {
        let index = index as u32;
        if !self.gpt.iter().any(|(i, _)| index == i) {
            return Err(Error::invalid("invalid index"))
        }
        self.gpt[index].starting_lba = partition.start();
        self.gpt[index].ending_lba = partition.end()-1;
        self.gpt[index].partition_type_guid = *partition.guid();
        self.gpt.write_into(&mut self.file)?;
        self.file.sync_all()?;
        Ok(())
    }

    fn is_range_free(&mut self, start_lba: u64, mut end_lba: u64) -> Result<bool, Error> {
        if start_lba == end_lba {
            end_lba += 1;
        }
        if start_lba > end_lba {
            return Ok(false)
        }
        if start_lba < self.gpt.header.first_usable_lba
            || end_lba - 1 > self.gpt.header.last_usable_lba
        {
            return Ok(false);
        }
        for (_index, entry) in self.gpt.iter() {
            if entry.is_unused() {
                continue;
            }
            if start_lba <= entry.ending_lba && entry.starting_lba < end_lba {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub fn create_disk(path: impl AsRef<Path>) -> Result<GptDisk, Error> {
    Ok(GptDisk::open(path)?)
}