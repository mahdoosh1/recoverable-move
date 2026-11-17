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
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

//! Fault-injection tests for the current `Disk` / `Record` / `continue_` API.

// THIS CODE IS AI GENERATED (by ChatGPT)

use recoverable_move::consts::MARKER_TYPE_GUID;
use recoverable_move::errors::Error;
use recoverable_move::mover::{continue_, recover_record};
use recoverable_move::record::Record;
use recoverable_move::traits::{Disk, Partition};

const SECTOR: usize = 512;
const OLD: u64 = 100;
const LEN: u64 = 60;
const CHUNK: u64 = 7;
const SUBCHUNK: u64 = 3;
const ORIGINAL_GUID: [u8; 16] = [0x11; 16];
const LABEL: [u8; 72] = [0; 72];

#[derive(Clone, Copy, Debug, Default)]
struct Faults {
    before: Option<u64>,
    after: Option<u64>,
    tear: Option<u64>,
}

/// A sector-addressable disk with one mutable partition and deterministic data.
struct MockDisk {
    data: Vec<u8>,
    partition: Partition,
    operations: u64,
    faults: Faults,
    unavailable: Vec<(u64, u64)>,
}

impl MockDisk {
    fn filled(sectors: usize) -> Self {
        let mut data = vec![0; sectors * SECTOR];
        for (lba, sector) in data.as_chunks_mut::<SECTOR>().0.iter_mut().enumerate() {
            sector.fill((lba as u8).wrapping_mul(37).wrapping_add(11));
        }
        Self {
            data,
            partition: Partition::new(OLD, LEN, &ORIGINAL_GUID, &LABEL),
            operations: 0,
            faults: Faults::default(),
            unavailable: Vec::new(),
        }
    }

    fn begin(&mut self) -> Result<u64, Error> {
        self.operations += 1;
        if self.faults.before == Some(self.operations) {
            return Err(io_error("simulated crash before operation"));
        }
        Ok(self.operations)
    }

    fn end(&self, operation: u64) -> Result<(), Error> {
        if self.faults.after == Some(operation) {
            return Err(io_error("simulated crash after operation"));
        }
        Ok(())
    }

    fn span(&self, lba: u64, len: usize) -> Result<std::ops::Range<usize>, Error> {
        let start = usize::try_from(lba)
            .ok()
            .and_then(|n| n.checked_mul(SECTOR))
            .ok_or_else(|| io_error("LBA overflow"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| io_error("range overflow"))?;
        if end > self.data.len() || len != SECTOR {
            return Err(io_error("sector outside mock disk or wrong sector size"));
        }
        Ok(start..end)
    }

    fn region(&self, lba: u64, sectors: u64) -> Vec<u8> {
        let start = lba as usize * SECTOR;
        let end = start + sectors as usize * SECTOR;
        self.data[start..end].to_vec()
    }

    fn block_range(&mut self, start: u64, end: u64) {
        self.unavailable.push((start, end));
    }
}

fn io_error(message: &str) -> Error {
    Error::ioerror(message)
}

impl Disk for MockDisk {
    fn sector_size(&self) -> usize {
        SECTOR
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        let operation = self.begin()?;
        let range = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[range]);
        self.end(operation)
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        let operation = self.begin()?;
        let range = self.span(lba, buf.len())?;
        if self.faults.tear == Some(operation) {
            let half = buf.len() / 2;
            self.data[range.start..range.start + half].copy_from_slice(&buf[..half]);
            return Err(io_error("simulated torn sector write"));
        }
        self.data[range.clone()].copy_from_slice(buf);
        self.end(operation)
    }

    fn sync(&mut self) -> Result<(), Error> {
        let operation = self.begin()?;
        self.end(operation)
    }

    fn list_partitions(&mut self) -> Result<Vec<Partition>, Error> {
        let operation = self.begin()?;
        let partition = self.partition;
        self.end(operation)?;
        Ok(vec![partition])
    }

    fn update_partition(&mut self, index: usize, partition: Partition) -> Result<(), Error> {
        let operation = self.begin()?;
        if index != 0 {
            return Err(io_error("invalid partition index"));
        }
        self.partition = partition;
        self.end(operation)
    }

    /// `start_lba..end_lba` is an exclusive-end range.
    fn is_range_free(&mut self, start_lba: u64, end_lba: u64) -> Result<bool, Error> {
        let operation = self.begin()?;
        let free = !self
            .unavailable
            .iter()
            .any(|&(start, end)| start_lba < end && start < end_lba);
        self.end(operation)?;
        Ok(free)
    }
}

fn make_record(disk: &MockDisk, displacement: i64) -> Record {
    Record::new(SECTOR as u32, disk.partition, displacement, CHUNK, SUBCHUNK)
        .unwrap_or_else(|e| panic!("invalid test record: {e}"))
}

fn start_move(disk: &mut MockDisk, displacement: i64) -> Result<(), Error> {
    let record = make_record(disk, displacement);
    continue_(disk, 0, record, None)
}

fn moved(disk: &MockDisk, new_start: u64, original: &[u8]) {
    assert_eq!(disk.partition.start(), new_start, "partition start");
    assert_eq!(disk.partition.len(), LEN, "partition length");
    assert_eq!(disk.partition.guid(), &ORIGINAL_GUID, "partition GUID");
    assert_eq!(disk.region(new_start, LEN), original, "partition contents");
}

fn untouched(disk: &MockDisk, original: &[u8]) {
    assert_eq!(disk.partition.start(), OLD, "partition start");
    assert_eq!(disk.partition.len(), LEN, "partition length");
    assert_eq!(disk.partition.guid(), &ORIGINAL_GUID, "partition GUID");
    assert_eq!(disk.region(OLD, LEN), original, "partition contents");
}

fn assert_settled(disk: &MockDisk, new_start: u64, original: &[u8]) {
    if disk.partition.start() == OLD && disk.partition.guid() == &ORIGINAL_GUID {
        untouched(disk, original);
    } else {
        moved(disk, new_start, original);
    }
}

/// A crash during Widen can precede the partition-table update, leaving the
/// new record outside the old partition's scan locations. That case is safe
/// to retry; otherwise recover the on-disk record and finish the move.
fn recover_or_accept_unstarted(disk: &mut MockDisk, new_start: u64) {
    match recover_record(disk, None) {
        Ok((index, record)) => {
            continue_(disk, index, record, None).unwrap_or_else(|e| panic!("recovery failed: {e}"));
        }
        Err(Error::Invalid(_))
            if disk.partition.start() == OLD
                && disk.partition.len() == LEN
                && disk.partition.guid() == &ORIGINAL_GUID =>
        {
            // The move did not reach the marker-partition update.
        }
        Err(Error::Invalid(_))
            if disk.partition.start() == new_start
                && disk.partition.len() == LEN
                && disk.partition.guid() == &ORIGINAL_GUID =>
        {
            // Resize updated the partition table, but record cleanup
            // may not have completed. The move itself is complete.
        }
        Err(e) => panic!("record recovery failed: {e}"),
    }
}

fn crash_at_every_operation(displacement: i64, blocked: Option<(u64, u64)>) {
    let new_start = (OLD as i64 + displacement) as u64;
    let mut baseline = MockDisk::filled(4096);
    if let Some((start, end)) = blocked {
        baseline.block_range(start, end);
    }
    let original = baseline.region(OLD, LEN);
    start_move(&mut baseline, displacement).unwrap_or_else(|e| panic!("baseline move failed: {e}"));
    let operation_count = baseline.operations;
    assert!(
        operation_count > 20,
        "unexpectedly few operations: {operation_count}"
    );

    for operation in 1..=operation_count {
        for crash_before in [true, false] {
            let mut disk = MockDisk::filled(4096);
            if let Some((start, end)) = blocked {
                disk.block_range(start, end);
            }
            if crash_before {
                disk.faults.before = Some(operation);
            } else {
                disk.faults.after = Some(operation);
            }

            let _ = start_move(&mut disk, displacement);
            disk.faults = Faults::default();
            recover_or_accept_unstarted(&mut disk, new_start);
            assert_settled(&disk, new_start, &original);
        }
    }
}

#[test]
fn clean_moves_preserve_data_in_both_directions() {
    for displacement in [23, -17] {
        let mut disk = MockDisk::filled(4096);
        let original = disk.region(OLD, LEN);
        start_move(&mut disk, displacement).unwrap_or_else(|e| panic!("move failed: {e}"));
        moved(&disk, (OLD as i64 + displacement) as u64, &original);
    }
}

#[test]
fn every_operation_is_crash_recoverable_forward() {
    crash_at_every_operation(23, None);
}

#[test]
fn every_operation_is_crash_recoverable_backward() {
    crash_at_every_operation(-17, None);
}

#[test]
fn backward_move_uses_the_other_record_placement_when_preferred_side_is_blocked() {
    // A backward move prefers the high-LBA record slot. Block it so Widen
    // must place the two record sectors on the low-LBA side instead.
    crash_at_every_operation(-17, Some((OLD + LEN, OLD + LEN + 2)));
}

#[test]
fn every_sector_write_can_be_torn_without_losing_recoverability() {
    for displacement in [23, -17] {
        let new_start = (OLD as i64 + displacement) as u64;
        let mut baseline = MockDisk::filled(4096);
        let original = baseline.region(OLD, LEN);
        start_move(&mut baseline, displacement)
            .unwrap_or_else(|e| panic!("baseline move failed: {e}"));
        let operation_count = baseline.operations;

        for operation in 1..=operation_count {
            let mut disk = MockDisk::filled(4096);
            disk.faults.tear = Some(operation);
            let _ = start_move(&mut disk, displacement);
            disk.faults = Faults::default();
            recover_or_accept_unstarted(&mut disk, new_start);
            assert_settled(&disk, new_start, &original);
        }
    }
}

#[test]
fn a_clean_disk_has_no_recoverable_record() {
    let mut disk = MockDisk::filled(4096);
    assert!(matches!(
        recover_record(&mut disk, None),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn marker_partition_without_a_valid_record_is_rejected() {
    let mut disk = MockDisk::filled(4096);
    disk.partition = Partition::new(OLD, LEN, &MARKER_TYPE_GUID, &LABEL);
    assert!(matches!(
        recover_record(&mut disk, None),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn record_rejects_zero_or_out_of_range_chunk_sizes() {
    let disk = MockDisk::filled(4096);
    assert!(Record::new(SECTOR as u32, disk.partition, 23, 0, 1).is_err());
    assert!(Record::new(SECTOR as u32, disk.partition, 23, 24, 1).is_err());
    assert!(Record::new(SECTOR as u32, disk.partition, 23, 7, 0).is_err());
    assert!(Record::new(SECTOR as u32, disk.partition, 23, 7, 8).is_err());
}
