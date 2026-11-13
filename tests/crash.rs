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


//! Fault-injection integration tests for crash-safe move and recovery behaviour.
//! The mock disk counts operations and can fail before/after writes, tear writes,
//! or corrupt written data to exercise every persisted state transition.

use recoverable_move::traits::MemStore;
use recoverable_move::{
    Disk, Error, MARKER_TYPE_GUID, MoveRequest, Partition, RecordStore, Verify, execute,
};

const SECTOR: usize = 512;
const OLD: u64 = 100;
const LEN: u64 = 60;
const CHUNK: u64 = 7;
const SUB: u64 = 3;
const ORIGINAL_GUID: [u8; 16] = [0x11; 16];

#[derive(Clone, Copy, Debug, Default)]
/// Operation numbers at which the mock disk should simulate failures.
struct Faults {
    crash_before: Option<u64>,
    crash_after: Option<u64>,
    tear: Option<u64>,
    corrupt: Option<u64>,
}

/// In-memory sector device with a single GPT-like partition and fault injection.
struct MockDisk {
    data: Vec<u8>,
    partition: Partition,
    ops: u64,
    faults: Faults,
}

impl MockDisk {
    /// Build a deterministic disk image filled with sector-dependent byte patterns.
    fn filled(sectors: u64) -> Self {
        let mut data = vec![0u8; sectors as usize * SECTOR];
        for i in 0..data.len() / SECTOR {
            data[i * SECTOR..(i + 1) * SECTOR]
                .fill((i as u8).wrapping_mul(37).wrapping_add(11));
        }
        Self {
            data,
            partition: Partition { start: OLD, len: LEN, type_guid: ORIGINAL_GUID },
            ops: 0,
            faults: Faults::default(),
        }
    }

    /// Count an operation and simulate a failure configured to happen before it.
    fn begin(&mut self) -> Result<u64, Error> {
        self.ops += 1;
        if self.faults.crash_before == Some(self.ops) {
            return Err(Error::Io(std::io::Error::other("simulated crash")));
        }
        Ok(self.ops)
    }

    /// Simulate a failure configured to happen after operation `op`.
    fn end(&self, op: u64) -> Result<(), Error> {
        if self.faults.crash_after == Some(op) {
            return Err(Error::Io(std::io::Error::other("simulated crash")));
        }
        Ok(())
    }

    /// Convert an LBA and byte length into a checked range in the backing vector.
    fn span(&self, lba: u64, len: usize) -> Result<std::ops::Range<usize>, Error> {
        let start = usize::try_from(lba)
            .ok()
            .and_then(|lba| lba.checked_mul(SECTOR))
            .ok_or_else(|| Error::Io(std::io::Error::other("lba overflow")))?;
        let end = start + len;
        if end > self.data.len() {
            return Err(Error::Io(std::io::Error::other(format!(
                "lba {lba} outside the mock"
            ))));
        }
        Ok(start..end)
    }

    /// Copy a contiguous region of sectors for assertions.
    fn region(&self, lba: u64, sectors: u64) -> Vec<u8> {
        let start = lba as usize * SECTOR;
        self.data[start..start + sectors as usize * SECTOR].to_vec()
    }
}

impl Disk for MockDisk {
    fn sector_size(&self) -> usize {
        SECTOR
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        let op = self.begin()?;
        let range = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[range]);
        self.end(op)
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        let op = self.begin()?;
        let range = self.span(lba, buf.len())?;
        if self.faults.tear == Some(op) {
            let written = buf.len() / 2;
            self.data[range.start..range.start + written].copy_from_slice(&buf[..written]);
            return Err(Error::Io(std::io::Error::other("simulated torn write")));
        }
        self.data[range.clone()].copy_from_slice(buf);
        if self.faults.corrupt == Some(op) {
            self.data[range.start] ^= 0xFF;
        }
        self.end(op)
    }

    fn sync(&mut self) -> Result<(), Error> {
        let op = self.begin()?;
        self.end(op)
    }

    fn partition(&mut self) -> Result<Partition, Error> {
        let op = self.begin()?;
        self.end(op)?;
        Ok(self.partition)
    }

    fn set_partition(&mut self, start: u64, len: u64) -> Result<(), Error> {
        let op = self.begin()?;
        self.partition.start = start;
        self.partition.len = len;
        self.end(op)
    }

    fn set_type_guid(&mut self, guid: [u8; 16]) -> Result<(), Error> {
        let op = self.begin()?;
        self.partition.type_guid = guid;
        self.end(op)
    }

    fn range_is_free(&mut self, _start: u64, _len: u64) -> Result<bool, Error> {
        let op = self.begin()?;
        self.end(op)?;
        Ok(true)
    }
}

/// Build the common test request with configurable target and verification mode.
fn request(new_start: u64, verify: Verify) -> MoveRequest {
    MoveRequest { old_start: OLD, new_start, len: LEN, chunk_size: CHUNK, subchunk_size: SUB, verify }
}

/// Assert that the partition reached the target extent with original contents/GUID.
fn moved(disk: &MockDisk, new_start: u64, expected: &[u8]) {
    assert_eq!(disk.partition.start, new_start, "partition start");
    assert_eq!(disk.partition.len, LEN, "partition length");
    assert_eq!(disk.partition.type_guid, ORIGINAL_GUID, "type GUID restored");
    assert_eq!(disk.region(new_start, LEN), expected, "partition contents");
}

/// Assert that the move did not start and the original partition is unchanged.
fn untouched(disk: &MockDisk, expected: &[u8]) {
    assert_eq!(disk.partition.start, OLD, "partition start");
    assert_eq!(disk.partition.len, LEN, "partition length");
    assert_eq!(disk.partition.type_guid, ORIGINAL_GUID, "type GUID");
    assert_eq!(disk.region(OLD, LEN), expected, "partition contents");
}

/// Either the move never started, or it finished. Nothing in between.
/// Assert that recovery left the disk either fully original or fully moved.
fn assert_settled(disk: &MockDisk, new_start: u64, expected: &[u8]) {
    if disk.partition.start == OLD && disk.partition.type_guid == ORIGINAL_GUID {
        untouched(disk, expected);
    } else {
        moved(disk, new_start, expected);
    }
}

/// Inject a crash before and after every operation, then require recovery to settle.
fn crash_everywhere(new_start: u64, use_store: bool) {
    let mut baseline = MockDisk::filled(4096);
    let expected = baseline.region(OLD, LEN);
    execute(&mut baseline, None, Some(request(new_start, Verify::Full))).unwrap();
    let total = baseline.ops;
    assert!(total > 20, "baseline only performed {total} operations");

    for crash_at in 1..=total {
        for before in [true, false] {
            let mut disk = MockDisk::filled(4096);
            disk.faults.crash_before = before.then_some(crash_at);
            disk.faults.crash_after = (!before).then_some(crash_at);
            let mut store = MemStore::new();
            let store_ref = use_store.then_some(&mut store as &mut dyn RecordStore);
            let _ = execute(&mut disk, store_ref, Some(request(new_start, Verify::Full)));

            disk.faults = Faults::default();
            let store_ref = use_store.then_some(&mut store as &mut dyn RecordStore);
            match execute(&mut disk, store_ref, None) {
                Ok(()) => moved(&disk, new_start, &expected),
                Err(Error::NoRecord) => assert_settled(&disk, new_start, &expected),
                Err(e) => {
                    panic!("crash at op {crash_at} (before={before}) was not recoverable: {e}")
                }
            }
            assert_settled(&disk, new_start, &expected);
        }
    }
}

#[test]
/// Crash before/after every operation during a forward relocation.
fn every_crash_forward() {
    crash_everywhere(OLD + 23, false);
}

#[test]
/// Crash before/after every operation during a backward relocation.
fn every_crash_backward() {
    crash_everywhere(OLD - 17, false);
}

#[test]
/// Repeat forward crash injection with an external record mirror.
fn every_crash_forward_with_store() {
    crash_everywhere(OLD + 23, true);
}

#[test]
/// Repeat backward crash injection with an external record mirror.
fn every_crash_backward_with_store() {
    crash_everywhere(OLD - 17, true);
}

#[test]
/// Verify recovery after each possible simulated half-sector write.
fn every_torn_write_is_recoverable() {
    for new_start in [OLD + 23, OLD - 17] {
        let mut baseline = MockDisk::filled(4096);
        let expected = baseline.region(OLD, LEN);
        execute(&mut baseline, None, Some(request(new_start, Verify::Full))).unwrap();
        let total = baseline.ops;

        for op in 1..=total {
            let mut disk = MockDisk::filled(4096);
            disk.faults.tear = Some(op);
            let _ = execute(&mut disk, None, Some(request(new_start, Verify::Full)));
            disk.faults = Faults::default();
            if let Err(e) = execute(&mut disk, None, None)
                && !matches!(e, Error::NoRecord)
            {
                panic!("tearing op {op} was not recoverable: {e}");
            }
            assert_settled(&disk, new_start, &expected);
        }
    }
}

#[test]
/// Verify ordinary forward and backward moves preserve all data.
fn clean_moves_both_directions() {
    for new_start in [OLD + 23, OLD - 17] {
        let mut disk = MockDisk::filled(4096);
        let expected = disk.region(OLD, LEN);
        execute(&mut disk, None, Some(request(new_start, Verify::Full))).unwrap();
        moved(&disk, new_start, &expected);
    }
}

#[test]
/// Ensure full verification detects data corruption in a copied chunk.
fn full_verification_rejects_a_corrupted_copy() {
    let mut baseline = MockDisk::filled(4096);
    execute(&mut baseline, None, Some(request(OLD + 23, Verify::Full))).unwrap();

    for op in 1..=baseline.ops {
        let mut disk = MockDisk::filled(4096);
        disk.faults.corrupt = Some(op);
        let result = execute(&mut disk, None, Some(request(OLD + 23, Verify::Full)));
        if matches!(result, Err(Error::Verify(_))) {
            return;
        }
    }
    panic!("no corrupted write produced a verification failure");
}

#[test]
/// Exercise all verification policies on forward and backward moves.
fn all_verification_modes_move_the_data() {
    for verify in [Verify::None, Verify::Checkpoints, Verify::Full] {
        let mut disk = MockDisk::filled(4096);
        let expected = disk.region(OLD, LEN);
        execute(&mut disk, None, Some(request(OLD - 17, verify))).unwrap();
        moved(&disk, OLD - 17, &expected);
    }
}

#[test]
/// Ensure a non-overlapping relocation is rejected as unsupported.
fn disjoint_move_is_unsupported() {
    let mut disk = MockDisk::filled(4096);
    let err = execute(&mut disk, None, Some(request(OLD + LEN, Verify::None))).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
}

#[test]
/// Ensure a zero-distance move is rejected.
fn zero_displacement_is_invalid() {
    let mut disk = MockDisk::filled(4096);
    let err = execute(&mut disk, None, Some(request(OLD, Verify::None))).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err}");
}

#[test]
/// Ensure recovery reports no record when no move is active.
fn a_clean_disk_has_nothing_to_recover() {
    let mut disk = MockDisk::filled(4096);
    assert!(matches!(execute(&mut disk, None, None), Err(Error::NoRecord)));
}

#[test]
/// Ensure a marker without a valid record is reported as corruption.
fn a_marked_partition_without_a_record_is_an_error() {
    let mut disk = MockDisk::filled(4096);
    disk.partition.type_guid = MARKER_TYPE_GUID;
    let err = execute(&mut disk, None, None).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err}");
}

#[test]
/// Ensure chunk and subchunk values are bounded to safe sizes.
fn oversized_chunk_and_subchunk_are_clamped() {
    let mut disk = MockDisk::filled(4096);
    let expected = disk.region(OLD, LEN);
    let mut req = request(OLD + 5, Verify::Full);
    req.chunk_size = 1_000_000;
    req.subchunk_size = 1_000_000;
    execute(&mut disk, None, Some(req)).unwrap();
    moved(&disk, OLD + 5, &expected);
}
