//! Crash-recoverable overlapping partition move.
//!
//! # Layout
//!
//! During a move the partition is described by the half-open interval
//! `[partition_start, partition_end)` — the *extended range*.  It is the
//! union of the old and new positions, plus one sector for the metadata.
//! Which side the extra sector sits on is recorded by a single flag,
//! `widen_forward`:
//!
//! ```text
//!   widen_forward = false:  [union_lower - 1, union_upper)
//!   widen_forward = true:   [union_lower,     union_upper + 1)
//! ```
//!
//! where:
//!
//! ```text
//!   union_lower = min(old_start, new_start)
//!   union_upper = max(old_start + len, new_start + len)
//! ```
//!
//! The metadata sector is always at `partition_start`.  When the range
//! was widened forward, `partition_start == union_lower` is also the
//! first data sector of the union, so its contents are parked at the
//! shadow (`partition_end - 1`) for the duration of the move.  When
//! widened backward, `partition_start` is a fresh sector and no shadow
//! is used.
//!
//! Every other field of `Metadata` is a raw input; `partition_start`,
//! `partition_end`, `metadata_lba`, and `shadow_lba` are derived.

pub use crate::traits::{Disk, MetaStore};

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write as IoWrite;
use std::path::PathBuf;

pub const SECTOR_SIZE: usize = 512;
pub const CHECKPOINT_SECTORS: u64 = 1;

pub mod example {
    use super::SECTOR_SIZE;
    pub const SUBCHUNK_BYTES: u64 = 128 * 1024 * 1024;
    pub const CHUNK_BYTES: u64 = 32 * 1024 * 1024 * 1024;
    pub const SUBCHUNK_SECTORS: u64 = SUBCHUNK_BYTES / SECTOR_SIZE as u64;
    pub const CHUNK_SECTORS: u64 = CHUNK_BYTES / SECTOR_SIZE as u64;
    pub const SUBCHUNKS_PER_CHUNK: u64 = CHUNK_SECTORS / SUBCHUNK_SECTORS;
}

const MAGIC: u64 = 0x4D4F_5645_5041_5254;
const VERSION: u16 = 2;
const CHECKSUM_OFFSET: usize = 508;
const SENTINEL: u8 = 0xA5;

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Phase {
    Init = 4,
    Zero = 8,
    Copy = 12,
    Finalize = 16,
    Verify = 20,
    Resize = 24,
}

impl Phase {
    fn from_u16(value: u16) -> Option<Phase> {
        Some(match value {
            4 => Phase::Init,
            8 => Phase::Zero,
            12 => Phase::Copy,
            16 => Phase::Finalize,
            20 => Phase::Verify,
            24 => Phase::Resize,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Error {
    Io(String),
    Corrupt(String),
    InvalidParams(String),
    NoTransaction,
    Verify(String),
    Crash,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(s) => write!(f, "io: {s}"),
            Error::Corrupt(s) => write!(f, "corrupt: {s}"),
            Error::InvalidParams(s) => write!(f, "invalid params: {s}"),
            Error::NoTransaction => write!(f, "no transaction and no params"),
            Error::Verify(s) => write!(f, "verify failed: {s}"),
            Error::Crash => write!(f, "simulated crash"),
        }
    }
}
impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Partition range as seen by the mover
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpt {
    pub start: u64,
    pub len: u64,
}

impl Gpt {
    pub fn new(start: u64, len: u64) -> Self { Self { start, len } }
    pub fn first_lba(&self) -> u64 { self.start }
    pub fn last_lba(&self) -> u64 { self.start + self.len - 1 }
    pub fn end_exclusive(&self) -> u64 { self.start + self.len }
}

// ---------------------------------------------------------------------------
// MetaStore implementations
// ---------------------------------------------------------------------------

pub struct NullStore;

impl MetaStore for NullStore {
    fn write(&mut self, _data: &[u8]) -> Result<(), Error> { Ok(()) }
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> { Ok(None) }
    fn remove(&mut self) {}
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self { Self { path } }
}

impl MetaStore for FileStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut file =
            fs::File::create(&self.path).map_err(|e| Error::Io(e.to_string()))?;
        file.write_all(data).map_err(|e| Error::Io(e.to_string()))?;
        file.sync_all().map_err(|e| Error::Io(e.to_string()))?;
        Ok(())
    }
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        match fs::read(&self.path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e.to_string())),
        }
    }
    fn remove(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Default)]
pub struct MemStore {
    data: Option<Vec<u8>>,
}

impl MemStore {
    pub fn new() -> Self { Self { data: None } }
}

impl MetaStore for MemStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.data = Some(data.to_vec());
        Ok(())
    }
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.data.clone())
    }
    fn remove(&mut self) { self.data = None; }
}

fn checksum(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub phase: Phase,
    /// Direction the extended range was widened in.
    /// * `true`  — forward: metadata at `union_lower`, shadow at `union_upper`.
    /// * `false` — backward: metadata at `union_lower - 1`, no shadow.
    pub widen_forward: bool,
    pub old_start: u64,
    pub len: u64,
    pub displacement: i64,
    pub chunk_size: u64,
    pub subchunk_size: u64,
    pub chunks_total: u64,
    pub chunks_done: u64,
}

impl Metadata {
    pub fn new_start(&self) -> u64 {
        (self.old_start as i64 + self.displacement) as u64
    }

    pub fn is_forward(&self) -> bool { self.displacement > 0 }

    fn union_lower(&self) -> u64 {
        self.old_start.min(self.new_start())
    }

    fn union_upper(&self) -> u64 {
        (self.old_start + self.len).max(self.new_start() + self.len)
    }

    /// First sector of the extended range — always the metadata LBA.
    pub fn partition_start(&self) -> u64 {
        if self.widen_forward {
            self.union_lower()
        } else {
            self.union_lower() - 1
        }
    }

    /// One past the last sector of the extended range.
    pub fn partition_end(&self) -> u64 {
        if self.widen_forward {
            self.union_upper() + 1
        } else {
            self.union_upper()
        }
    }

    pub fn metadata_lba(&self) -> u64 { self.partition_start() }
    pub fn shadow_lba(&self) -> u64 { self.partition_end() - 1 }

    pub fn partition_range(&self) -> Gpt {
        Gpt::new(self.partition_start(), self.partition_end() - self.partition_start())
    }

    fn encode(&self) -> [u8; SECTOR_SIZE] {
        let mut b = [0u8; SECTOR_SIZE];
        b[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&(self.phase as u16).to_le_bytes());
        b[12] = self.widen_forward as u8;
        b[16..24].copy_from_slice(&self.old_start.to_le_bytes());
        b[24..32].copy_from_slice(&self.len.to_le_bytes());
        b[32..40].copy_from_slice(&self.displacement.to_le_bytes());
        b[40..48].copy_from_slice(&self.chunk_size.to_le_bytes());
        b[48..56].copy_from_slice(&self.subchunk_size.to_le_bytes());
        b[56..64].copy_from_slice(&self.chunks_total.to_le_bytes());
        b[64..72].copy_from_slice(&self.chunks_done.to_le_bytes());
        let cs = checksum(&b[0..CHECKSUM_OFFSET]);
        b[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&cs.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Metadata> {
        if b.len() < SECTOR_SIZE { return None; }
        if u64::from_le_bytes(b[0..8].try_into().ok()?) != MAGIC { return None; }
        if u16::from_le_bytes(b[8..10].try_into().ok()?) != VERSION { return None; }
        let stored = u32::from_le_bytes(b[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].try_into().ok()?);
        if stored != checksum(&b[0..CHECKSUM_OFFSET]) { return None; }
        let m = Metadata {
            phase: Phase::from_u16(u16::from_le_bytes(b[10..12].try_into().ok()?))?,
            widen_forward: b[12] != 0,
            old_start: u64::from_le_bytes(b[16..24].try_into().ok()?),
            len: u64::from_le_bytes(b[24..32].try_into().ok()?),
            displacement: i64::from_le_bytes(b[32..40].try_into().ok()?),
            chunk_size: u64::from_le_bytes(b[40..48].try_into().ok()?),
            subchunk_size: u64::from_le_bytes(b[48..56].try_into().ok()?),
            chunks_total: u64::from_le_bytes(b[56..64].try_into().ok()?),
            chunks_done: u64::from_le_bytes(b[64..72].try_into().ok()?),
        };
        m.validate().ok()?;
        Some(m)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.len == 0 {
            return Err(Error::Corrupt("zero len".into()));
        }
        if self.displacement == 0 {
            return Err(Error::Corrupt("zero displacement".into()));
        }
        if self.chunk_size == 0 || self.chunk_size > self.displacement.unsigned_abs() {
            return Err(Error::Corrupt("chunk_size out of range".into()));
        }
        if self.subchunk_size == 0 || self.subchunk_size > self.chunk_size {
            return Err(Error::Corrupt("subchunk_size out of range".into()));
        }
        if self.chunks_total == 0 {
            return Err(Error::Corrupt("zero chunks_total".into()));
        }
        if self.chunks_done > self.chunks_total {
            return Err(Error::Corrupt("chunks_done > chunks_total".into()));
        }
        let expected_chunks = (self.len + self.chunk_size - 1) / self.chunk_size;
        if self.chunks_total != expected_chunks {
            return Err(Error::Corrupt("chunks_total inconsistent".into()));
        }

        // Widening backward means partition_start = union_lower - 1, so
        // union_lower must be at least 1.
        if !self.widen_forward && self.union_lower() == 0 {
            return Err(Error::Corrupt("no room before union".into()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Redirects
// ---------------------------------------------------------------------------

/// Physical LBA to read from when the copy wants `source`.
///
/// Only fires in the widened-forward, forward-move case: the first sector
/// of the extended range is the metadata, and its original contents are
/// parked at the shadow.
fn source_lba(m: &Metadata, source: u64) -> u64 {
    if m.widen_forward && m.is_forward() && source == m.metadata_lba() {
        m.shadow_lba()
    } else {
        source
    }
}

/// Physical LBA to write to when the copy wants `destination`.
///
/// Only fires in the widened-forward, backward-move case: the first
/// sector of the extended range is the metadata, and writes intended for
/// it are routed to the shadow instead.
fn destination_lba(m: &Metadata, dst: u64) -> u64 {
    if m.widen_forward && !m.is_forward() && dst == m.metadata_lba() {
        m.shadow_lba()
    } else {
        dst
    }
}

fn chunk_bounds(m: &Metadata, chunk_index: u64) -> (u64, u64, u64) {
    debug_assert!(chunk_index < m.chunks_total);
    let source_start = m.old_start + chunk_index * m.chunk_size;
    let destination_start = (source_start as i64 + m.displacement) as u64;
    let chunk_len = m.chunk_size.min(m.len - chunk_index * m.chunk_size);
    (source_start, destination_start, chunk_len)
}

fn processing_index(m: &Metadata, chunks_done: u64) -> u64 {
    if m.is_forward() {
        m.chunks_total - 1 - chunks_done
    } else {
        chunks_done
    }
}

fn is_sentinel(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == SENTINEL)
}

fn checkpoint_offsets(chunk_len: u64, subchunk: u64) -> Vec<u64> {
    let mut offsets = Vec::new();
    let mut offset = subchunk;
    while offset <= chunk_len {
        offsets.push(offset - 1);
        offset += subchunk;
    }
    if offsets.last().copied() != Some(chunk_len - 1) {
        offsets.push(chunk_len - 1);
    }
    offsets
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

fn write_metadata(
    disk: &mut dyn Disk,
    lba: u64,
    m: &Metadata,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let encoded = m.encode();
    store.write(&encoded)?;
    disk.write_sector(lba, &encoded)?;
    disk.sync()
}

fn read_metadata(
    disk: &mut dyn Disk,
    lba: u64,
    store: &mut dyn MetaStore,
) -> Result<Option<Metadata>, Error> {
    if let Some(data) = store.read()? {
        if let Some(m) = Metadata::decode(&data) {
            return Ok(Some(m));
        }
    }
    let mut buf = [0u8; SECTOR_SIZE];
    disk.read_sector(lba, &mut buf)?;
    Ok(Metadata::decode(&buf))
}

// ---------------------------------------------------------------------------
// Parameters and entry points
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct MoveParams {
    pub old_start: u64,
    pub len: u64,
    pub new_start: u64,
    pub chunk_size: u64,
    pub subchunk_size: u64,
}

pub fn run(
    disk: &mut dyn Disk,
    params: Option<MoveParams>,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let gpt = disk.get_partition()?;

    if let Some(m) = read_metadata(disk, gpt.start, store)? {
        if m.phase != Phase::Resize && gpt != m.partition_range() {
            return Err(Error::Corrupt(format!(
                "GPT entry {:?} does not match metadata partition {:?}",
                gpt,
                m.partition_range()
            )));
        }
        return resume(disk, m.metadata_lba(), m, store);
    }

    match params {
        Some(p) => start(disk, p, store),
        None => Err(Error::NoTransaction),
    }
}

pub fn start(
    disk: &mut dyn Disk,
    params: MoveParams,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let displacement = params.new_start as i64 - params.old_start as i64;
    if displacement == 0 {
        return Err(Error::InvalidParams("zero displacement".into()));
    }
    if params.len == 0 {
        return Err(Error::InvalidParams("zero length".into()));
    }
    if params.subchunk_size < CHECKPOINT_SECTORS {
        return Err(Error::InvalidParams("subchunk smaller than a checkpoint".into()));
    }

    let chunk_size = params.chunk_size.min(displacement.unsigned_abs());
    if chunk_size == 0 {
        return Err(Error::InvalidParams("chunk size clamps to zero".into()));
    }
    let subchunk_size = params.subchunk_size.min(chunk_size);
    if subchunk_size < CHECKPOINT_SECTORS {
        return Err(Error::InvalidParams("subchunk clamps below one checkpoint".into()));
    }

    let union_lower = params.old_start.min(params.new_start);
    let union_upper = (params.old_start + params.len)
        .max(params.new_start + params.len);

    // Destination must not overlap any other partition.
    if !disk.is_range_free(params.new_start, params.new_start + params.len)? {
        return Err(Error::InvalidParams(
            "destination range overlaps another partition or is out of bounds".into(),
        ));
    }

    // Widen backward if the sector before the union is free; otherwise
    // widen forward, but only if the sector after the union is also free.
    let widen_forward = match union_lower.checked_sub(1) {
        Some(candidate) if disk.is_sector_free(candidate)? => false,
        _ => {
            if !disk.is_sector_free(union_upper)? {
                return Err(Error::InvalidParams(
                    "no free sector before or after the union".into(),
                ));
            }
            true
        }
    };

    let metadata = Metadata {
        phase: Phase::Init,
        widen_forward,
        old_start: params.old_start,
        len: params.len,
        displacement,
        chunk_size,
        subchunk_size,
        chunks_total: (params.len + chunk_size - 1) / chunk_size,
        chunks_done: 0,
    };
    metadata.validate()?;

    disk.set_partition(metadata.partition_range())?;
    disk.sync()?;

    // Forward move widened forward: the metadata sector is a live source
    // sector.  Park its contents in the shadow before the metadata
    // overwrites it.
    if metadata.widen_forward && metadata.is_forward() {
        let mut saved = [0u8; SECTOR_SIZE];
        disk.read_sector(metadata.metadata_lba(), &mut saved)?;
        disk.write_sector(metadata.shadow_lba(), &saved)?;
        disk.sync()?;
    }

    write_metadata(disk, metadata.metadata_lba(), &metadata, store)?;
    resume(disk, metadata.metadata_lba(), metadata, store)
}

// ---------------------------------------------------------------------------
// Phase driver
// ---------------------------------------------------------------------------

fn resume(
    disk: &mut dyn Disk,
    metadata_lba: u64,
    mut metadata: Metadata,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    loop {
        match metadata.phase {
            Phase::Init => {
                metadata.phase = Phase::Zero;
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }
            Phase::Zero => {
                if metadata.chunks_done >= metadata.chunks_total {
                    metadata.phase = Phase::Verify;
                    write_metadata(disk, metadata_lba, &metadata, store)?;
                    continue;
                }
                let index = processing_index(&metadata, metadata.chunks_done);
                zero_checkpoints(disk, &metadata, index)?;
                disk.sync()?;
                metadata.phase = Phase::Copy;
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }
            Phase::Copy => {
                let index = processing_index(&metadata, metadata.chunks_done);
                copy_chunk(disk, &metadata, index)?;
                disk.sync()?;
                metadata.phase = Phase::Finalize;
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }
            Phase::Finalize => {
                let index = processing_index(&metadata, metadata.chunks_done);
                finalize_chunk(disk, &metadata, index)?;
                disk.sync()?;
                metadata.chunks_done += 1;
                metadata.phase = if metadata.chunks_done >= metadata.chunks_total {
                    Phase::Verify
                } else {
                    Phase::Zero
                };
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }
            Phase::Verify => {
                verify(disk, &metadata)?;
                metadata.phase = Phase::Resize;
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }
            Phase::Resize => {
                return do_resize(disk, metadata_lba, &metadata, store);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase bodies
// ---------------------------------------------------------------------------

fn zero_checkpoints(
    disk: &mut dyn Disk,
    m: &Metadata,
    chunk_index: u64,
) -> Result<(), Error> {
    let (_, destination_start, chunk_len) = chunk_bounds(m, chunk_index);
    let zero = [0u8; SECTOR_SIZE];
    for offset in checkpoint_offsets(chunk_len, m.subchunk_size) {
        disk.write_sector(destination_lba(m, destination_start + offset), &zero)?;
    }
    Ok(())
}

fn copy_chunk(
    disk: &mut dyn Disk,
    m: &Metadata,
    chunk_index: u64,
) -> Result<(), Error> {
    debug_assert_eq!(m.phase, Phase::Copy);
    let (source_start, destination_start, chunk_len) = chunk_bounds(m, chunk_index);
    let checkpoints = checkpoint_offsets(chunk_len, m.subchunk_size);

    let mut resume_offset = 0u64;
    let mut buf = [0u8; SECTOR_SIZE];
    for &offset in &checkpoints {
        disk.read_sector(destination_lba(m, destination_start + offset), &mut buf)?;
        if is_sentinel(&buf) {
            resume_offset = offset + 1;
        } else {
            break;
        }
    }

    let checkpoint_set: HashSet<u64> = checkpoints.iter().copied().collect();
    let sentinel = [SENTINEL; SECTOR_SIZE];
    let mut offset = resume_offset;
    while offset < chunk_len {
        let physical_destination = destination_lba(m, destination_start + offset);
        if checkpoint_set.contains(&offset) {
            disk.write_sector(physical_destination, &sentinel)?;
        } else {
            let physical_source = source_lba(m, source_start + offset);
            let mut data = [0u8; SECTOR_SIZE];
            disk.read_sector(physical_source, &mut data)?;
            disk.write_sector(physical_destination, &data)?;
        }
        offset += 1;
    }
    Ok(())
}

fn finalize_chunk(
    disk: &mut dyn Disk,
    m: &Metadata,
    chunk_index: u64,
) -> Result<(), Error> {
    let (source_start, destination_start, chunk_len) = chunk_bounds(m, chunk_index);
    let mut buf = [0u8; SECTOR_SIZE];
    for offset in checkpoint_offsets(chunk_len, m.subchunk_size) {
        let physical_destination = destination_lba(m, destination_start + offset);
        disk.read_sector(physical_destination, &mut buf)?;
        if is_sentinel(&buf) {
            let physical_source = source_lba(m, source_start + offset);
            let mut data = [0u8; SECTOR_SIZE];
            disk.read_sector(physical_source, &mut data)?;
            disk.write_sector(physical_destination, &data)?;
        }
    }
    Ok(())
}

fn verify(disk: &mut dyn Disk, m: &Metadata) -> Result<(), Error> {
    for chunk_index in 0..m.chunks_total {
        let (_, destination_start, chunk_len) = chunk_bounds(m, chunk_index);
        let mut buf = [0u8; SECTOR_SIZE];
        for offset in checkpoint_offsets(chunk_len, m.subchunk_size) {
            disk.read_sector(destination_lba(m, destination_start + offset), &mut buf)?;
            if is_sentinel(&buf) {
                return Err(Error::Verify(format!(
                    "sentinel at chunk {chunk_index} offset {offset}"
                )));
            }
        }
    }
    Ok(())
}

fn do_resize(
    disk: &mut dyn Disk,
    metadata_lba: u64,
    m: &Metadata,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    disk.set_partition(Gpt::new(m.new_start(), m.len))?;
    disk.sync()?;

    if m.widen_forward && !m.is_forward() {
        // Metadata sector is now the first data sector of the final
        // partition.  Restore it from the shadow.
        let mut saved = [0u8; SECTOR_SIZE];
        disk.read_sector(m.shadow_lba(), &mut saved)?;
        disk.write_sector(metadata_lba, &saved)?;
    } else {
        let zero = [0u8; SECTOR_SIZE];
        disk.write_sector(metadata_lba, &zero)?;
    }
    disk.sync()?;

    store.remove();
    Ok(())
}
