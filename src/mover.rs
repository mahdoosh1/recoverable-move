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

//! Crash-safe state machine that relocates overlapping partition extents.
//!
//! Chunks are processed in a direction-dependent order. Each chunk uses sentinel
//! checkpoint sectors so a restart can distinguish copied data from unfinished
//! subchunks without requiring a second full-sized journal.

use crate::record::{Phase, RECORD_SECTORS, Record};
use crate::traits::{Disk, RecordStore};
use crate::{Error, Geometry, MARKER_TYPE_GUID, Placement};

/// How much of each chunk is checked against its source before the chunk is
/// declared done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verify {
    /// Skip data comparison after copying.
    None,
    /// Compare only checkpoint sectors after each chunk.
    Checkpoints,
    /// Compare every sector in each completed chunk against its source.
    Full,
}

#[derive(Clone, Copy, Debug)]
/// Parameters for starting a partition relocation.
///
/// All addresses and sizes are measured in sectors. The source and target must
/// overlap, and `chunk_size` is clamped to the displacement magnitude.
pub struct MoveRequest {
    /// Original partition start LBA.
    pub old_start: u64,
    /// Desired final partition start LBA.
    pub new_start: u64,
    /// Partition length in sectors.
    pub len: u64,
    /// Requested maximum chunk length in sectors.
    pub chunk_size: u64,
    /// Requested checkpoint spacing in sectors.
    pub subchunk_size: u64,
    /// Verification policy applied to each completed chunk.
    pub verify: Verify,
}

/// The sentinel written into a checkpoint sector while its chunk is being
/// copied. A fixed constant: recovery must recognise it from the record alone,
/// and a payload that matches it costs a subchunk redundant rewrite, not correctness.
const SENTINEL: [u8; 512] = [
    0x3F, 0x9A, 0xC4, 0x51, 0x7E, 0x22, 0xB8, 0x0D, 0xE1, 0x6C, 0x93, 0x47, 0xAF, 0x15, 0xD2, 0x88,
    0x71, 0x5B, 0x2E, 0xF9, 0x46, 0xC3, 0x0A, 0x9D, 0x36, 0xE8, 0x54, 0xBF, 0x1C, 0x7A, 0x63, 0xD0,
    0x8B, 0x47, 0xF2, 0x19, 0xA6, 0x3C, 0xE5, 0x70, 0x0F, 0xB4, 0x5D, 0xCA, 0x21, 0x96, 0x38, 0xEF,
    0x52, 0x0C, 0xAD, 0x7B, 0xE3, 0x94, 0x18, 0x6F, 0xC7, 0x2A, 0x85, 0x5E, 0xB1, 0x40, 0xF8, 0x13,
    0x6D, 0xC2, 0x37, 0x99, 0x04, 0xEB, 0x5A, 0xA1, 0x7F, 0x28, 0xD4, 0x8E, 0x41, 0xB6, 0x0B, 0x63,
    0xF5, 0x1A, 0x9C, 0xE0, 0x47, 0x7C, 0x32, 0xBB, 0x05, 0xD8, 0x6E, 0x91, 0xAC, 0x3E, 0x57, 0x24,
    0xC9, 0x80, 0x16, 0x4D, 0xA3, 0xF7, 0x0E, 0x5C, 0xE9, 0x31, 0x74, 0xBD, 0x22, 0x8A, 0x66, 0xD1,
    0x4B, 0x9F, 0x03, 0xE4, 0x58, 0xB0, 0x2D, 0x76, 0xC1, 0x3A, 0x8D, 0x55, 0xFA, 0x17, 0x6B, 0xA9,
    0x30, 0xDD, 0x42, 0x8C, 0x75, 0xE2, 0x09, 0xB9, 0x54, 0x1F, 0x6A, 0xC8, 0x93, 0x27, 0xFE, 0x4D,
    0xB7, 0x58, 0x0A, 0xE6, 0x3D, 0x81, 0xC3, 0x19, 0x76, 0xAF, 0x62, 0x0B, 0xE8, 0x45, 0x9C, 0x31,
    0x5D, 0xF4, 0x28, 0x87, 0xC0, 0x13, 0x6E, 0xBA, 0x09, 0x72, 0xEB, 0x46, 0x8F, 0x25, 0xD7, 0x63,
    0x91, 0x38, 0xAC, 0x05, 0x7E, 0xC6, 0x2B, 0xF0, 0x54, 0x9A, 0x1D, 0x67, 0xB2, 0xE4, 0x0F, 0x7B,
    0x46, 0xD9, 0x2F, 0x85, 0xE1, 0x3C, 0x70, 0xAB, 0x12, 0x6D, 0xB8, 0x4F, 0x93, 0x26, 0xEA, 0x5C,
    0x08, 0x7F, 0xC3, 0x5A, 0xB6, 0x21, 0xE9, 0x44, 0x8D, 0x0A, 0x75, 0xD2, 0x6C, 0x3B, 0x9F, 0xE7,
    0x51, 0xA4, 0x0E, 0x7C, 0xD5, 0x69, 0x32, 0xBE, 0x18, 0x95, 0xE0, 0x47, 0x8A, 0x2D, 0xF1, 0x60,
    0xBC, 0x07, 0x6F, 0xD3, 0x2A, 0x9E, 0x45, 0x71, 0xFA, 0x34, 0xC8, 0x1C, 0x83, 0x5B, 0xE6, 0x90,
    0x2C, 0xF7, 0x61, 0x9B, 0x38, 0xD0, 0x4A, 0xB5, 0x0D, 0x76, 0xE2, 0x89, 0x54, 0x1F, 0xA7, 0xC3,
    0x7D, 0x04, 0xEB, 0x52, 0x9F, 0x26, 0x6A, 0xC1, 0x33, 0x8B, 0xF0, 0x17, 0x64, 0xD9, 0x2E, 0x85,
    0xA0, 0x3F, 0x96, 0x0B, 0x74, 0xD7, 0x21, 0x5C, 0xE8, 0x43, 0x8A, 0x1E, 0xB2, 0x6F, 0x35, 0xCD,
    0x56, 0x91, 0x0C, 0xE3, 0x7A, 0x28, 0xB4, 0x4D, 0x9F, 0x60, 0xD5, 0x1A, 0x87, 0x32, 0xEB, 0x44,
    0xFE, 0x19, 0x75, 0xC2, 0x0D, 0x6B, 0x90, 0x3A, 0xB7, 0x54, 0xE1, 0x28, 0x8C, 0xF3, 0x46, 0x6D,
    0x31, 0xAC, 0x58, 0xE7, 0x02, 0x7B, 0xC9, 0x14, 0x6E, 0x9D, 0x42, 0xB0, 0x3D, 0x75, 0xE4, 0x51,
    0x8A, 0x0F, 0xD6, 0x63, 0x21, 0xBC, 0x4E, 0x97, 0x05, 0x7A, 0xE8, 0x39, 0xC4, 0x12, 0x6F, 0xAB,
    0x50, 0x9E, 0x37, 0xC1, 0x6D, 0x84, 0xFB, 0x2A, 0x0E, 0xD5, 0x79, 0x43, 0xA6, 0x1B, 0xE2, 0x68,
    0xC7, 0x24, 0x8F, 0x1D, 0x5B, 0xE0, 0x76, 0x93, 0x3A, 0xBC, 0x47, 0x09, 0xF1, 0x6C, 0x28, 0xD4,
    0x6B, 0x82, 0xEF, 0x15, 0x40, 0xA9, 0x3C, 0x77, 0xD2, 0x0B, 0x65, 0x9F, 0x31, 0xE4, 0x58, 0xAC,
    0x13, 0x76, 0xD8, 0x4A, 0xB1, 0x0F, 0xE7, 0x52, 0x9C, 0x3D, 0x61, 0x85, 0xFA, 0x27, 0x6E, 0xC0,
    0x94, 0x38, 0x7D, 0xE1, 0x0A, 0x56, 0xBF, 0x2C, 0x63, 0xD9, 0x10, 0x87, 0x4B, 0xAE, 0x35, 0xF6,
    0x21, 0xC9, 0x5E, 0x74, 0xE8, 0x03, 0x9A, 0x46, 0xBD, 0x6F, 0x12, 0xD7, 0x58, 0x83, 0xEC, 0x30,
    0x7C, 0x45, 0xA3, 0x9F, 0x2B, 0xD6, 0x60, 0x18, 0xE5, 0x71, 0x8A, 0x3F, 0xC2, 0x59, 0x04, 0xBD,
    0xD0, 0x69, 0x1E, 0x83, 0x57, 0xEC, 0x2F, 0x9B, 0x46, 0xA1, 0x70, 0x3D, 0xC8, 0x14, 0x6A, 0xF2,
    0x0B, 0x9D, 0x35, 0xE7, 0x48, 0x72, 0xC6, 0x51, 0xAF, 0x08, 0x63, 0xB4, 0x1C, 0x95, 0xDE, 0x2E,
];

/// Start a move or resume one already marked as in progress.
///
/// Pass `Some(request)` to start a new move. Pass `None` to request recovery;
/// if the selected partition has the marker GUID, recovery is attempted even
/// when a request is supplied. An optional [`RecordStore`] is a secondary
/// journal copy, not a prerequisite for correctness.
///
/// # Errors
/// Returns an [`Error`] for invalid geometry, I/O or GPT failures, missing or
/// corrupt recovery records, and verification mismatches.
pub fn execute(
    disk: &mut dyn Disk,
    mut store: Option<&mut dyn RecordStore>,
    request: Option<MoveRequest>,
) -> Result<(), Error> {
    let partition = disk.partition()?;
    if partition.type_guid == MARKER_TYPE_GUID {
        let record = recover_record(disk, &mut store)?;
        return run(disk, store, record, Verify::None);
    }
    let request = request.ok_or(Error::NoRecord)?;
    start(disk, store, request)
}

/// Validate a new request, persist its initial record, mark and widen the GPT
/// partition, then enter the resumable state machine.
fn start(
    disk: &mut dyn Disk,
    mut store: Option<&mut dyn RecordStore>,
    request: MoveRequest,
) -> Result<(), Error> {
    let displacement = i64::try_from(request.new_start as i128 - request.old_start as i128)
        .map_err(|_| Error::invalid("displacement does not fit in i64"))?;
    let geometry = Geometry::new(request.old_start, request.len, displacement)?;
    if request.chunk_size == 0 || request.subchunk_size == 0 {
        return Err(Error::invalid("chunk and subchunk sizes must be nonzero"));
    }
    let chunk_size = request
        .chunk_size
        .min(geometry.displacement().unsigned_abs());
    let subchunk_size = request.subchunk_size.min(chunk_size);
    let partition = disk.partition()?;
    if partition.start != geometry.old_start() || partition.len != geometry.len() {
        return Err(Error::invalid(
            "the partition is not where the request says it is",
        ));
    }
    let placement = choose_placement(disk, geometry)?;
    let record = Record::new(
        geometry,
        placement,
        chunk_size,
        subchunk_size,
        partition.type_guid,
    )?;

    write_record(disk, &mut store, &record)?;
    disk.set_type_guid(MARKER_TYPE_GUID)?;
    let (start, end) = geometry.widened(placement)?;
    disk.set_partition(start, end - start)?;
    disk.sync()?;

    run(disk, store, record, request.verify)
}

/// Select the record side that keeps the record adjacent to the final target.
fn choose_placement(disk: &mut dyn Disk, geometry: Geometry) -> Result<Placement, Error> {
    // Keep the record adjacent to the final target extent. This makes it
    // directly discoverable if a crash occurs after the final GPT resize but
    // before the marker GUID is restored. During the earlier source-extent
    // window recovery probes outward by at most the displacement.
    let preferred = if geometry.is_forward() {
        Placement::After
    } else {
        Placement::Before
    };
    let (start, _) = geometry.record_range(preferred)?;
    if disk.range_is_free(start, RECORD_SECTORS)? {
        Ok(preferred)
    } else {
        Err(Error::invalid(
            "no free space for the record sectors adjacent to the move's union",
        ))
    }
}

/// Find the record of a marked partition. It is adjacent to the widened
/// extent during the move and adjacent to the final extent after the last
/// resize; the bounded fallback handles the brief original-extent window.
fn recover_record(
    disk: &mut dyn Disk,
    store: &mut Option<&mut dyn RecordStore>,
) -> Result<Record, Error> {
    let partition = disk.partition()?;
    let sector = disk.sector_size();

    // Try the external mirror first. It is a secondary recovery source,
    // so a missing, unreadable, or invalid mirror must not prevent recovery
    // from the primary on-disk journal.
    if let Some(store) = store.as_mut() {
        if let Ok(Some(data)) = (**store).read() {
            if let Some(record) = Record::decode(&data) {
                if extent_matches(record, &partition) {
                    // The mirror may be one journal update behind because
                    // the on-disk record is persisted before the mirror.
                    // Locate the primary journal using the mirror's geometry
                    // and prefer a valid on-disk record if one exists.
                    let (record_start, _) = record.geometry.record_range(record.placement)?;

                    if let Some(disk_record) =
                        read_record_at(disk, record_start, sector, &partition)?
                    {
                        return Ok(disk_record);
                    }

                    // Both on-disk copies are unreadable. Use the validated
                    // mirror as the recovery source.
                    return Ok(record);
                }
            }
        }
    }

    // The final resize can move the GPT extent away from the record sectors
    // before the marker GUID is restored. In that narrow crash window the
    // record is still just outside the *old union*, not adjacent to the new
    // extent. Since the displacement is strictly smaller than the partition,
    // it can be at most `partition.len` sectors from either final edge.
    // Probe the four immediate boundary locations first (the widened-state
    // fast path), then the bounded edge ranges needed for the post-resize
    // window. A valid CRC plus extent_matches prevents accepting unrelated data.

    // Fast path: the record is adjacent to the current (usually widened or
    // final) extent. The two mirror sectors are tried independently.
    let direct = [
        partition.start.checked_sub(RECORD_SECTORS),
        Some(partition.start),
        partition.end().checked_sub(RECORD_SECTORS),
        Some(partition.end()),
    ];
    for start in direct.into_iter().flatten() {
        if let Some(record) = read_record_at(disk, start, sector, &partition)? {
            return Ok(record);
        }
    }

    Err(Error::corrupt(
        "the partition is marked as in motion but no valid record was found",
    ))
}

/// Read both mirror sectors at one possible record location. An unreadable
/// mirror is ignored so the other copy can still be used.
/// Try both redundant journal sectors at one candidate starting LBA.
fn read_record_at(
    disk: &mut dyn Disk,
    start: u64,
    sector_size: usize,
    partition: &crate::traits::Partition,
) -> Result<Option<Record>, Error> {
    let mut buf = vec![0u8; sector_size];
    for offset in 0..RECORD_SECTORS {
        if disk.read_sector(start + offset, &mut buf).is_err() {
            continue;
        }
        if let Some(record) = Record::decode(&buf)
            && extent_matches(record, partition)
        {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

/// The record belongs to this partition either at the widened extent or, if
/// the crash landed before the widen, at the source extent.
/// Check whether the current GPT extent is a valid source, widened, or target
/// extent for the decoded record.
fn extent_matches(record: Record, partition: &crate::traits::Partition) -> bool {
    if record
        .geometry
        .widened(record.placement)
        .is_ok_and(|(start, end)| start == partition.start && end == partition.end())
    {
        return true;
    }
    let (target_start, target_end) = record.geometry.target();
    let source_start = record.geometry.old_start();
    let source_end = source_start + record.geometry.len();
    // TODO: is this check below correct?
    (partition.start == target_start && partition.end() == target_end)
        || (partition.start == source_start && partition.end() == source_end)
}

/// Execute journal phases until the final extent is restored and the record cleared.
/// Every phase is written to the journal only after its preceding work is synced.
fn run(
    disk: &mut dyn Disk,
    mut store: Option<&mut dyn RecordStore>,
    mut record: Record,
    verify: Verify,
) -> Result<(), Error> {
    if record.phase != Phase::Resize {
        ensure_widened(disk, &record)?;
    }

    loop {
        match record.phase {
            Phase::Zero => {
                if record.chunks_done == record.chunks_total() {
                    record.phase = Phase::Resize;
                    write_record(disk, &mut store, &record)?;
                    continue;
                }
                let index = processing_index(&record);
                zero_checkpoints(disk, &record, index)?;
                disk.sync()?;
                record.phase = Phase::Copy;
                write_record(disk, &mut store, &record)?;
                disk.sync()?;
            }
            Phase::Copy => {
                let index = processing_index(&record);
                copy_chunk(disk, &record, index)?;
                disk.sync()?;
                record.phase = Phase::Finalize;
                write_record(disk, &mut store, &record)?;
                disk.sync()?;
            }
            Phase::Finalize => {
                let index = processing_index(&record);
                finalize_chunk(disk, &record, index)?;
                disk.sync()?;
                verify_chunk(disk, &record, index, verify)?;
                record.chunks_done += 1;
                record.phase = if record.chunks_done == record.chunks_total() {
                    Phase::Resize
                } else {
                    Phase::Zero
                };
                write_record(disk, &mut store, &record)?;
                disk.sync()?;
            }
            Phase::Resize => {
                resize(disk, &record)?;
                if let Some(store) = store.as_mut() {
                    (**store).clear()?;
                }
                return Ok(());
            }
        }
    }
}

/// Ensure the GPT partition covers the source/target union and carries the marker GUID.
fn ensure_widened(disk: &mut dyn Disk, record: &Record) -> Result<(), Error> {
    let (start, end) = record.geometry.widened(record.placement)?;
    let partition = disk.partition()?;
    if partition.start != start || partition.end() != end {
        // below are some checks, but do we need them?
        let source_start = record.geometry.old_start();
        let source_end = source_start + record.geometry.len();
        if partition.start != source_start || partition.end() != source_end {
            return Err(Error::corrupt(
                "the partition is neither widened nor at its source",
            ));
        }
        disk.set_partition(start, end - start)?;
        disk.sync()?;
    }
    // TODO: is this check even possible? setting the type guid must be one of the first steps when moving a partition.
    if disk.partition()?.type_guid != MARKER_TYPE_GUID {
        disk.set_type_guid(MARKER_TYPE_GUID)?;
    }
    Ok(())
}

/// Restore the final target extent and original type GUID, then erase both record sectors.
fn resize(disk: &mut dyn Disk, record: &Record) -> Result<(), Error> {
    let (start, end) = record.geometry.target();
    let partition = disk.partition()?;
    if partition.start != start || partition.end() != end {
        disk.set_partition(start, end - start)?;
        disk.sync()?;
    }
    disk.set_type_guid(record.original_type_guid)?;

    let (record_start, _) = record.geometry.record_range(record.placement)?;
    let zero = vec![0u8; disk.sector_size()];
    for offset in 0..RECORD_SECTORS {
        disk.write_sector(record_start + offset, &zero)?;
    }
    disk.sync()?;
    Ok(())
}

/// Select the next chunk index in the safe direction for forward/backward moves.
fn processing_index(record: &Record) -> u64 {
    if record.geometry.is_forward() {
        record.chunks_total() - 1 - record.chunks_done
    } else {
        record.chunks_done
    }
}

/// `(source, destination, len)` in sectors for one chunk.
fn chunk_bounds(record: &Record, index: u64) -> (u64, u64, u64) {
    let offset = index * record.chunk_size;
    let source = record.geometry.old_start() + offset;
    let len = record.chunk_size.min(record.geometry.len() - offset);
    (source, record.geometry.target_start() + offset, len)
}

/// Last sector of every subchunk, plus the chunk's last sector.
/// Return the last sector of each subchunk and always include the chunk tail.
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

/// Return whether every 512-byte block in `buf` matches the checkpoint sentinel.
fn is_sentinel(buf: &[u8]) -> bool {
    buf.len() % SENTINEL.len() == 0 && buf.chunks(SENTINEL.len()).all(|part| part == SENTINEL)
}

/// Repeat the fixed sentinel pattern to fill one device sector.
fn sentinel_for(sector_size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(sector_size);
    while buf.len() < sector_size {
        let take = (sector_size - buf.len()).min(SENTINEL.len());
        buf.extend_from_slice(&SENTINEL[..take]);
    }
    buf
}

/// Zero destination checkpoint sectors so unfinished work is detectable on restart.
fn zero_checkpoints(disk: &mut dyn Disk, record: &Record, index: u64) -> Result<(), Error> {
    let (_, destination, len) = chunk_bounds(record, index);
    let zero = vec![0u8; disk.sector_size()];
    for offset in checkpoints(len, record.subchunk_size) {
        disk.write_sector(destination + offset, &zero)?;
    }
    Ok(())
}

/// Copy non-checkpoint sectors, using sentinels to resume at the first unfinished subchunk.
fn copy_chunk(disk: &mut dyn Disk, record: &Record, index: u64) -> Result<(), Error> {
    let (source, destination, len) = chunk_bounds(record, index);
    let sentinel = sentinel_for(disk.sector_size());
    let checkpoints = checkpoints(len, record.subchunk_size);

    // finding the first unfinished subchunk
    let mut buf = vec![0u8; disk.sector_size()];
    let mut offset = 0;
    for &checkpoint in &checkpoints {
        disk.read_sector(destination + checkpoint, &mut buf)?;
        if !is_sentinel(&buf) {
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
            disk.write_sector(destination + offset, &sentinel)?;
            next_checkpoint += 1;
        } else {
            disk.read_sector(source + offset, &mut buf)?;
            disk.write_sector(destination + offset, &buf)?;
        }
        offset += 1;
    }
    Ok(())
}

/// Restore real data to all checkpoint sectors after the ordinary sectors are copied.
fn finalize_chunk(disk: &mut dyn Disk, record: &Record, index: u64) -> Result<(), Error> {
    let (source, destination, len) = chunk_bounds(record, index);
    // Every checkpoint is rewritten unconditionally. A torn restore can leave
    // a sector that is neither the sentinel nor the real data, and the source
    // is intact until the chunk is marked done.
    let mut buf = vec![0u8; disk.sector_size()];
    for offset in checkpoints(len, record.subchunk_size) {
        disk.read_sector(source + offset, &mut buf)?;
        disk.write_sector(destination + offset, &buf)?;
    }
    Ok(())
}

/// Compare destination sectors with their source according to the requested policy.
fn verify_chunk(
    disk: &mut dyn Disk,
    record: &Record,
    index: u64,
    verify: Verify,
) -> Result<(), Error> {
    if verify == Verify::None {
        return Ok(());
    }
    let (source, destination, len) = chunk_bounds(record, index);
    let offsets: Vec<u64> = match verify {
        Verify::Checkpoints => checkpoints(len, record.subchunk_size),
        Verify::Full => (0..len).collect(),
        Verify::None => unreachable!(),
    };

    let mut expected = vec![0u8; disk.sector_size()];
    let mut actual = vec![0u8; disk.sector_size()];
    for offset in offsets {
        disk.read_sector(source + offset, &mut expected)?;
        disk.read_sector(destination + offset, &mut actual)?;
        // TODO: this error should warn the user that the disk is not behaving correctly and
        // it should be mirrored to another disk for backup. and the operation should continue
        // from there, or otherwise the user can ignore the warning and retry the chunk
        if expected != actual {
            return Err(Error::Verify(format!(
                "chunk {index} differs at sector {offset}"
            )));
        }
    }
    Ok(())
}

/// Write the record to both sectors, lower first, syncing between them, then
/// to the store. A crash leaves at most one sector stale, and the store is
/// never newer than the disk.
/// Encode and persist both on-disk journal copies in order, syncing each write,
/// then update the optional external mirror.
fn write_record(
    disk: &mut dyn Disk,
    store: &mut Option<&mut dyn RecordStore>,
    record: &Record,
) -> Result<(), Error> {
    let encoded = record.encode(disk.sector_size())?;
    let (start, _) = record.geometry.record_range(record.placement)?;
    for offset in 0..RECORD_SECTORS {
        disk.write_sector(start + offset, &encoded)?;
        disk.sync()?;
    }
    if let Some(store) = store.as_mut() {
        (**store).write(&encoded)?;
    }
    Ok(())
}
