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

use crate::consts::{CRC_OFFSET, MAGIC, VERSION};
use crate::CRC32;
use crate::traits::Partition;
use crate::mover::Phase;
use crate::errors::Error;

#[derive(Clone, Copy)]
pub struct Record {
    pub(crate) sector_size: u32,
    pub(crate) partition: Partition,
    pub(crate) displacement: i64,
    pub(crate) chunk_size: u64,
    pub(crate) subchunk_size: u64,
    pub(crate) phase: Phase,
    pub(crate) chunks_done: u64,
    pub(crate) records_lba: u64
}

impl Record {
    pub fn new(
        sector_size: u32,
        partition: Partition,
        displacement: i64,
        chunk_size: u64,
        subchunk_size: u64,
    ) -> Result<Self, Error> {
        let record = Self {
            sector_size,
            partition,
            displacement,
            chunk_size,
            subchunk_size,
            phase: Phase::Widen,
            chunks_done: 0,
            records_lba: 0
        };
        record.verify()?;
        Ok(record)
    }

    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    pub fn partition(&self) -> Partition {
        self.partition
    }

    pub fn displacement(&self) -> i64 {
        self.displacement
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    pub fn subchunk_size(&self) -> u64 {
        self.subchunk_size
    }

    pub fn records_lba(&self) -> u64 {
        self.records_lba
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if (self.sector_size as usize) < CRC_OFFSET + 4 {
            return Err(Error::invalid("sector size must be at least 256 bytes"));
        }

        let mut buf = vec![0u8; self.sector_size as usize];

        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..10].copy_from_slice(&VERSION.to_le_bytes());
        buf[10..14].copy_from_slice(&(self.sector_size as u32).to_le_bytes());
        buf[14] = self.phase as u8;
        buf[15] = 0; // reserved
        buf[16..24].copy_from_slice(&self.chunk_size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.subchunk_size.to_le_bytes());
        buf[32..40].copy_from_slice(&self.chunks_done.to_le_bytes());
        buf[40..48].copy_from_slice(&self.partition.start().to_le_bytes());
        buf[48..56].copy_from_slice(&self.partition.len().to_le_bytes());
        buf[56..64].copy_from_slice(&self.displacement.to_le_bytes());
        buf[64..80].copy_from_slice(self.partition.guid());
        buf[80..152].copy_from_slice(self.partition.label());
        buf[152..160].copy_from_slice(&self.records_lba.to_le_bytes());
        let crc = CRC32.checksum(&buf[..CRC_OFFSET]);
        buf[CRC_OFFSET..CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(buf)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, Error> {
        if buf.len() < 256 {
            return Err(Error::invalid("sector size must be at least 256 bytes"));
        }
        if buf[0..8] != MAGIC {
            return Err(Error::invalid("magic code didn't match"));
        }
        if buf[8..10] != VERSION.to_le_bytes() {
            return Err(Error::invalid("version not supported"))
        }
        
        let sector_size = u32::from_le_bytes(buf[10..14].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let phase = Phase::from_u8(buf[14])?;
        let chunk_size = u64::from_le_bytes(buf[16..24].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let subchunk_size = u64::from_le_bytes(buf[24..32].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let chunks_done = u64::from_le_bytes(buf[32..40].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let partition_start = u64::from_le_bytes(buf[40..48].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let partition_len = u64::from_le_bytes(buf[48..56].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let displacement = i64::from_le_bytes(buf[56..64].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let original_guid: &[u8; 16] = &buf[64..80].try_into().map_err(|_| Error::invalid("unreachable"))?;
        let original_label: &[u8; 72] = &buf[80..152].try_into().map_err(|_| Error::invalid("unreachable"))?;
        let records_lba = u64::from_le_bytes(buf[152..160].try_into().map_err(|_| Error::invalid("unreachable"))?);

        if buf.len() != sector_size as usize {
            return Err(Error::invalid("truncated or bigger than usual buffer"));
        }
        let crc32 = u32::from_le_bytes(buf[CRC_OFFSET..CRC_OFFSET+4].try_into().map_err(|_| Error::invalid("unreachable"))?);
        let actual_crc32 = CRC32.checksum(&buf[..CRC_OFFSET]);
        if crc32 != actual_crc32 {
            return Err(Error::invalid("checksum doesn't match"));
        }

        let partition = Partition::new(partition_start, partition_len, original_guid, original_label);
        let mut record = Self::new(sector_size, partition, displacement, chunk_size, subchunk_size)?;
        record.phase = phase;
        record.chunks_done = chunks_done;
        record.records_lba = records_lba;
        record.verify()?;
        Ok(record)
    }

    pub fn verify(&self) -> Result<(), Error> {
        if self.chunk_size == 0 || self.chunk_size > self.displacement.unsigned_abs() {
            return Err(Error::invalid("chunk size is out of range"));
        }
        if self.subchunk_size == 0 || self.subchunk_size > self.chunk_size {
            return Err(Error::invalid("subchunk size is out of range"));
        }
        if self.chunks_done > self.chunks_total() {
            return Err(Error::invalid("chunk progress exceeds the chunk count"));
        }
        if self.phase != Phase::Widen && self.records_lba == 0 {
            return Err(Error::invalid("record lba not defined ( records_lba = 0 )"))
        }
        if self.partition.len() == 0 {
            return Err(Error::invalid("zero-sized partition"));
        }
        if self.phase == Phase::Widen && self.chunks_done != 0 {
            return Err(Error::invalid("phase and chunks_done don't match"))
        }
        if self.chunks_done == self.chunk_size && self.phase != Phase::Resize {
            return Err(Error::invalid("phase and chunks_done don't match"))
        }
        Ok(())
    }
    pub fn chunks_total(&self) -> u64 {
        self.partition.len().div_ceil(self.chunk_size)
    }
    pub fn partition_after(&self) -> Partition {
        Partition::new(
            (self.partition.start() as i64 + self.displacement) as u64,
            self.partition.len(),
            self.partition.guid(),
            self.partition.label()
        )
    }
}