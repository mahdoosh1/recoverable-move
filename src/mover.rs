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

use crate::consts::{MARKER_TYPE_GUID, sentinel_for};
use crate::errors::Error;
use crate::record::Record;
use crate::traits::{Disk, FileWriter, Partition};

// we need a phase
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Phase {
    Widen = 0,
    Zero = 4,
    Copy = 8,
    Finalize = 12,
    NextChunk = 16,
    Resize = 20,
}

impl Phase {
    pub fn from_u8(value: u8) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::Widen,
            4 => Self::Zero,
            8 => Self::Copy,
            12 => Self::Finalize,
            16 => Self::NextChunk,
            20 => Self::Resize,
            _ => return Err(Error::invalid(format!("Invalid phase: {value}"))),
        })
    }
}

pub fn recover_record(
    disk: &mut dyn Disk,
    mirror: Option<Record>,
) -> Result<(usize, Record), Error> {
    let partitions = disk.list_partitions()?;
    for (index, partition) in partitions.iter().enumerate() {
        // try anyways
        let checks = [
            partition.start(),
            partition.start() + 1,
            partition.end() - 2,
            partition.end() - 1,
        ];
        let mut buf = vec![0u8; disk.sector_size()];
        for check in checks {
            if let Some(mirror_record) = mirror
                && check == mirror_record.records_lba()
            {
                return Ok((index, mirror_record));
            }
            disk.read_sector(check, &mut buf)?;
            if let Ok(record) = Record::decode(&buf) {
                if let Some(newest_record) = mirror
                    && record.partition == newest_record.partition
                {
                    return Ok((index, newest_record));
                }
                return Ok((index, record));
            }
        }
    }
    Err(Error::invalid(
        "failed to recover the record and partition index",
    ))
}

fn write_record(
    record: Record,
    disk: &mut dyn Disk,
    records_lba: u64,
    file: Option<&mut FileWriter>,
) -> Result<(), Error> {
    let bytes = record.encode()?;
    if let Some(file) = file {
        file.write(&bytes)?;
    }
    for lba in records_lba..records_lba + 2 {
        disk.write_sector(lba, &bytes)?;
    }
    Ok(())
}

fn which_chunk(record: Record) -> u64 {
    if record.displacement() > 0 {
        record.chunks_total() - 1 - record.chunks_done
    } else {
        record.chunks_done
    }
}

fn chunk_to_lba(record: &Record, current_chunk: u64) -> (u64, u64, u64) {
    let offset = current_chunk * record.chunk_size(); // in lba
    let source = record.partition().start() + offset; // in lba
    let dest = record.partition_after().start() + offset; // in lba
    let len = record.chunk_size().min(record.partition().len() - offset); // subtract until not as big as a full chunk
    (source, dest, len)
}

fn checkpoints(len: u64, subchunk: u64) -> Vec<u64> {
    let mut offsets: Vec<u64> = (1..)
        .map(|n| n * subchunk - 1)
        .take_while(|&offset| offset < len)
        .collect();
    if offsets.last() != Some(&(len - 1)) {
        offsets.push(len - 1);
    }
    offsets
}

pub fn continue_(
    disk: &mut dyn Disk,
    partition_index: usize,
    mut record: Record,
    mut file: Option<FileWriter>,
) -> Result<(), Error> {
    // false: left, true: right
    let mut placement;
    let mut records_lba = record.records_lba;
    let mut current_chunk;
    let sentinel = sentinel_for(disk.sector_size());
    loop {
        match record.phase {
            Phase::Widen => {
                // if a power loss happens in this phase, data is still safe even if the record won't be found by recover_record
                // the inconveniance is that the user will have to redo the operation.
                let old = record.partition();
                let new = record.partition_after();
                let min_ = old
                    .start()
                    .min(new.start());
                let max_ = old
                    .end()
                    .max(new.end());
                if record.displacement > 0 {
                    if disk.is_range_free(min_ - 2, min_)? {
                        placement = false;
                    } else if disk.is_range_free(max_, max_ + 2)? {
                        placement = true;
                    } else {
                        return Err(Error::invalid("no space left for the record"));
                    }
                    if !disk.is_range_free(new.start().max(old.end()), new.end())? {
                        return Err(Error::invalid("destination overlaps with occupied space (possibly another partition)"));
                    }
                } else {
                    if disk.is_range_free(max_, max_ + 2)? {
                        placement = true;
                    } else if disk.is_range_free(min_ - 2, min_)? {
                        placement = false;
                    } else {
                        return Err(Error::invalid("no space left for the record"));
                    }
                    if !disk.is_range_free(new.start(), new.end().min(old.start()))? {
                        return Err(Error::invalid("destination overlaps with occupied space (possibly another partition)"));
                    }
                }

                let partition;
                if placement {
                    partition = Partition::new(
                        min_,
                        max_ - min_ + 2,
                        &MARKER_TYPE_GUID,
                        old.label(),
                    );
                    records_lba = max_;
                } else {
                    partition = Partition::new(
                        min_ - 2,
                        max_ - min_ + 2,
                        &MARKER_TYPE_GUID,
                        old.label(),
                    );
                    records_lba = min_ - 2;
                };
                record.records_lba = records_lba;
                record.phase = Phase::Zero;
                write_record(record, disk, records_lba, file.as_mut())?;
                disk.sync()?;
                disk.update_partition(partition_index, partition)?;
                disk.sync()?;
            }
            Phase::Zero => {
                current_chunk = which_chunk(record);
                let (_, dest, len) = chunk_to_lba(&record, current_chunk);
                let zero = vec![0u8; disk.sector_size()];
                for offset in checkpoints(len, record.subchunk_size()) {
                    disk.write_sector(dest + offset, &zero)?;
                }
                disk.sync()?;
                record.phase = Phase::Copy;
                write_record(record, disk, records_lba, file.as_mut())?;
                disk.sync()?;
            }
            Phase::Copy => {
                current_chunk = which_chunk(record);
                let (source, dest, len) = chunk_to_lba(&record, current_chunk);
                let mut buf = vec![0u8; disk.sector_size()];
                let mut offset = 0;
                let checkpoints = checkpoints(len, record.subchunk_size);
                for checkpoint in &checkpoints {
                    disk.read_sector(dest + checkpoint, &mut buf)?;
                    if buf != sentinel {
                        break;
                    }
                    offset = checkpoint + 1;
                }

                // closest checkpoint after or at offset
                let mut next_checkpoint = checkpoints
                    .iter()
                    .position(|&c| c >= offset)
                    .unwrap_or(checkpoints.len());
                while offset < len {
                    if checkpoints.get(next_checkpoint) == Some(&offset) {
                        // if offset is checkpoint, we write the sentinel instead
                        disk.write_sector(dest + offset, &sentinel)?;
                        next_checkpoint += 1;
                    } else {
                        disk.read_sector(source + offset, &mut buf)?;
                        disk.write_sector(dest + offset, &buf)?;
                    }
                    disk.sync()?;
                    offset += 1;
                }
                record.phase = Phase::Finalize;
                write_record(record, disk, records_lba, file.as_mut())?;
                disk.sync()?;
            }
            Phase::Finalize => {
                current_chunk = which_chunk(record);
                let (source, dest, len) = chunk_to_lba(&record, current_chunk);
                let mut buf = vec![0u8; disk.sector_size()];
                for offset in checkpoints(len, record.subchunk_size) {
                    disk.read_sector(source + offset, &mut buf)?;
                    disk.write_sector(dest + offset, &buf)?;
                    disk.sync()?;
                }
                record.phase = Phase::NextChunk;
                write_record(record, disk, records_lba, file.as_mut())?;
                disk.sync()?;
            }
            Phase::NextChunk => {
                record.chunks_done += 1;
                if record.chunks_done >= record.chunks_total() {
                    record.phase = Phase::Resize;
                } else {
                    record.phase = Phase::Zero;
                }
                write_record(record, disk, records_lba, file.as_mut())?;
                disk.sync()?;
            }
            Phase::Resize => {
                let partition_after = record.partition_after();
                disk.update_partition(partition_index, partition_after)?;
                disk.sync()?;
                let zeros = vec![0u8; disk.sector_size()];
                for lba in records_lba..records_lba + 2 {
                    disk.write_sector(lba, &zeros)?;
                }
                disk.sync()?;
                break;
            }
        }
    }

    Ok(())
}
