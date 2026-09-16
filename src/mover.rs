//! The move algorithm.
//!
//! A move proceeds through a fixed sequence of [`Phase`]s. Each phase is a
//! milestone, not an atomic step: a crash may land anywhere inside one,
//! and recovery reconstructs the state from the metadata plus whatever is
//! already on disk.
//!
//! # Data movement
//!
//! Data is copied in *chunks* of at most `|X|` sectors. A chunk's source
//! and destination never overlap, so the copy within a chunk is safe in
//! either direction. The chunks are processed so that a chunk's
//! destination never lands on another chunk's not-yet-copied source:
//! forward moves process the highest chunk first, backward moves the
//! lowest.
//!
//! Within a chunk, *checkpoints* record progress. A checkpoint is the last
//! sector of each *subchunk*, and it moves through three states:
//!
//! ```text
//! zeroed  →  sentinel  →  real data
//! ```
//!
//! The chunk copy zeroes every checkpoint first, stamps each one with a
//! sentinel as the copy reaches it, and restores it to real data once the
//! chunk is fully copied. Recovery reads the checkpoints, finds the last
//! sentinel, and resumes immediately after it.

use std::collections::HashSet;
use std::fmt;

use crate::traits::{Disk, MetaStore};

pub const SECTOR_SIZE: usize = 512;
pub const CHECKPOINT_SECTORS: u64 = 1;

const MAGIC: u64 = 0x4D4F_5645_5041_5254;
const VERSION: u16 = 3;
const CHECKSUM_OFFSET: usize = 508;
const SENTINEL: u8 = 0xA5;

/// Sizing used by the original design. Exposed as a starting point, not a
/// default.
pub mod example {
    use super::SECTOR_SIZE;

    pub const SUBCHUNK_BYTES: u64 = 128 * 1024 * 1024;
    pub const CHUNK_BYTES: u64 = 32 * 1024 * 1024 * 1024;
    pub const SUBCHUNK_SECTORS: u64 = SUBCHUNK_BYTES / SECTOR_SIZE as u64;
    pub const CHUNK_SECTORS: u64 = CHUNK_BYTES / SECTOR_SIZE as u64;
    pub const SUBCHUNKS_PER_CHUNK: u64 = CHUNK_SECTORS / SUBCHUNK_SECTORS;
}

// --- Phase -------------------------------------------------------------

/// Milestones of a move. The discriminants are spaced by 4 so that
/// intermediate states can be inserted later without renumbering.
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
    const fn from_raw(value: u16) -> Option<Self> {
        Some(match value {
            4 => Self::Init,
            8 => Self::Zero,
            12 => Self::Copy,
            16 => Self::Finalize,
            20 => Self::Verify,
            24 => Self::Resize,
            _ => return None,
        })
    }
}

// --- Error -------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Corrupt(String),
    InvalidParams(String),
    NoTransaction,
    Verify(String),
    /// Returned by the test mock when crash injection fires. Never
    /// produced by the mover itself.
    Crash,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Corrupt(s) => write!(f, "corrupt state: {s}"),
            Self::InvalidParams(s) => write!(f, "invalid parameters: {s}"),
            Self::NoTransaction => f.write_str("no transaction in progress"),
            Self::Verify(s) => write!(f, "verification failed: {s}"),
            Self::Crash => f.write_str("simulated crash"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<crate::gpt::Error> for Error {
    fn from(e: crate::gpt::Error) -> Self {
        Self::Corrupt(e.to_string())
    }
}

// --- Extent ------------------------------------------------------------

/// A half-open sector range `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    start: u64,
    end: u64,
}

impl Extent {
    /// Construct an extent. Returns an error if `start > end`.
    pub fn new(start: u64, end: u64) -> Result<Self, Error> {
        if start > end {
            return Err(Error::InvalidParams(
                "extent start greater than end".into(),
            ));
        }
        Ok(Self { start, end })
    }

    pub const fn start(self) -> u64 { self.start }
    pub const fn end(self) -> u64 { self.end }
    pub const fn len(self) -> u64 { self.end - self.start }
    pub const fn is_empty(self) -> bool { self.start == self.end }
    pub const fn contains(self, lba: u64) -> bool {
        lba >= self.start && lba < self.end
    }
}

// --- Metadata ----------------------------------------------------------

/// Persistent record of an in-flight move. Always encoded into exactly
/// one sector, checksummed so a torn write is detected on recovery.
///
/// Fields are private; the only way to obtain a `Metadata` is through
/// [`Metadata::new`] or [`Metadata::decode`], both of which run the same
/// invariant checks. A live `Metadata` is always internally consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    phase: Phase,
    widen_forward: bool,
    old_start: u64,
    len: u64,
    displacement: i64,
    chunk_size: u64,
    subchunk_size: u64,
    chunks_total: u64,
    chunks_done: u64,
}

impl Metadata {
    /// Create a fresh metadata record for a new move. Returns an error if
    /// any invariant fails; the caller cannot hold an invalid `Metadata`.
    pub fn new(
        old_start: u64,
        len: u64,
        displacement: i64,
        chunk_size: u64,
        subchunk_size: u64,
        widen_forward: bool,
    ) -> Result<Self, Error> {
        if len == 0 {
            return Err(Error::InvalidParams("zero length".into()));
        }
        if displacement == 0 {
            return Err(Error::InvalidParams("zero displacement".into()));
        }
        if old_start.checked_add_signed(displacement).is_none() {
            return Err(Error::InvalidParams("target start overflows".into()));
        }
        if chunk_size == 0 || chunk_size > displacement.unsigned_abs() {
            return Err(Error::InvalidParams("chunk size out of range".into()));
        }
        if subchunk_size == 0 || subchunk_size > chunk_size {
            return Err(Error::InvalidParams("subchunk size out of range".into()));
        }
        if subchunk_size < CHECKPOINT_SECTORS {
            return Err(Error::InvalidParams(
                "subchunk smaller than one checkpoint".into(),
            ));
        }

        let target_start = old_start
            .checked_add_signed(displacement)
            .ok_or_else(|| Error::InvalidParams("target start overflows".into()))?;

        if old_start.checked_add(len).is_none() {
            return Err(Error::InvalidParams("source end overflows".into()));
        }
        if target_start.checked_add(len).is_none() {
            return Err(Error::InvalidParams("target end overflows".into()));
        }

        let union_lower = old_start.min(target_start);
        if !widen_forward && union_lower == 0 {
            return Err(Error::InvalidParams("no sector before union".into()));
        }

        let chunks_total = len.div_ceil(chunk_size);
        if chunks_total == 0 {
            return Err(Error::InvalidParams("zero chunks".into()));
        }

        Ok(Self {
            phase: Phase::Init,
            widen_forward,
            old_start,
            len,
            displacement,
            chunk_size,
            subchunk_size,
            chunks_total,
            chunks_done: 0,
        })
    }

    // --- Accessors ---

    pub const fn phase(self) -> Phase { self.phase }
    pub const fn widen_forward(self) -> bool { self.widen_forward }
    pub const fn old_start(self) -> u64 { self.old_start }
    pub const fn partition_len(self) -> u64 { self.len }
    pub const fn displacement(self) -> i64 { self.displacement }
    pub const fn chunk_size(self) -> u64 { self.chunk_size }
    pub const fn subchunk_size(self) -> u64 { self.subchunk_size }
    pub const fn chunks_total(self) -> u64 { self.chunks_total }
    pub const fn chunks_done(self) -> u64 { self.chunks_done }

    /// First LBA of the target position. Infallible: `new` rejects any
    /// value that would overflow, so a live `Metadata` cannot fail here.
    pub const fn target_start(self) -> u64 {
        self.old_start
            .checked_add_signed(self.displacement)
            .expect("displacement is validated at construction")
    }

    pub const fn is_forward(self) -> bool {
        self.displacement > 0
    }

    const fn union_lower(self) -> u64 {
        if self.old_start < self.target_start() {
            self.old_start
        } else {
            self.target_start()
        }
    }

    const fn union_upper(self) -> u64 {
        let old_end = self.old_start + self.len;
        let new_end = self.target_start() + self.len;
        if old_end > new_end { old_end } else { new_end }
    }

    /// First sector of the extended range. Always the metadata LBA.
    pub const fn partition_start(self) -> u64 {
        if self.widen_forward {
            self.union_lower()
        } else {
            self.union_lower() - 1
        }
    }

    /// One past the last sector of the extended range.
    pub const fn partition_end(self) -> u64 {
        if self.widen_forward {
            self.union_upper() + 1
        } else {
            self.union_upper()
        }
    }

    pub const fn metadata_lba(self) -> u64 {
        self.partition_start()
    }

    pub const fn shadow_lba(self) -> u64 {
        self.partition_end() - 1
    }

    pub const fn partition_range(self) -> Extent {
        Extent {
            start: self.partition_start(),
            end: self.partition_end(),
        }
    }

    // --- Mutators (crate-internal) ---

    pub(crate) fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
    }

    pub(crate) fn advance_chunk(&mut self) {
        debug_assert!(self.chunks_done < self.chunks_total);
        self.chunks_done += 1;
    }

    // --- Serialization ---

    fn encode(self) -> [u8; SECTOR_SIZE] {
        let mut buf = [0u8; SECTOR_SIZE];
        buf[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        buf[8..10].copy_from_slice(&VERSION.to_le_bytes());
        buf[10..12].copy_from_slice(&(self.phase as u16).to_le_bytes());
        buf[12] = self.widen_forward as u8;
        buf[16..24].copy_from_slice(&self.old_start.to_le_bytes());
        buf[24..32].copy_from_slice(&self.len.to_le_bytes());
        buf[32..40].copy_from_slice(&self.displacement.to_le_bytes());
        buf[40..48].copy_from_slice(&self.chunk_size.to_le_bytes());
        buf[48..56].copy_from_slice(&self.subchunk_size.to_le_bytes());
        buf[56..64].copy_from_slice(&self.chunks_total.to_le_bytes());
        buf[64..72].copy_from_slice(&self.chunks_done.to_le_bytes());
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&cs.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < SECTOR_SIZE {
            return None;
        }
        if u64::from_le_bytes(buf[0..8].try_into().ok()?) != MAGIC {
            return None;
        }
        if u16::from_le_bytes(buf[8..10].try_into().ok()?) != VERSION {
            return None;
        }
        let stored = u32::from_le_bytes(
            buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].try_into().ok()?,
        );
        if stored != checksum(&buf[..CHECKSUM_OFFSET]) {
            return None;
        }
        let metadata = Self {
            phase: Phase::from_raw(u16::from_le_bytes(buf[10..12].try_into().ok()?))?,
            widen_forward: buf[12] != 0,
            old_start: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            len: u64::from_le_bytes(buf[24..32].try_into().ok()?),
            displacement: i64::from_le_bytes(buf[32..40].try_into().ok()?),
            chunk_size: u64::from_le_bytes(buf[40..48].try_into().ok()?),
            subchunk_size: u64::from_le_bytes(buf[48..56].try_into().ok()?),
            chunks_total: u64::from_le_bytes(buf[56..64].try_into().ok()?),
            chunks_done: u64::from_le_bytes(buf[64..72].try_into().ok()?),
        };
        metadata.validate().ok()?;
        Some(metadata)
    }

    fn validate(self) -> Result<(), Error> {
        if self.len == 0 {
            return Err(Error::Corrupt("zero length".into()));
        }
        if self.displacement == 0 {
            return Err(Error::Corrupt("zero displacement".into()));
        }
        if self.chunk_size == 0 || self.chunk_size > self.displacement.unsigned_abs() {
            return Err(Error::Corrupt("chunk size out of range".into()));
        }
        if self.subchunk_size == 0 || self.subchunk_size > self.chunk_size {
            return Err(Error::Corrupt("subchunk size out of range".into()));
        }
        if self.chunks_total == 0 {
            return Err(Error::Corrupt("zero chunks".into()));
        }
        if self.chunks_done > self.chunks_total {
            return Err(Error::Corrupt("chunks done exceeds total".into()));
        }
        let expected = self.len.div_ceil(self.chunk_size);
        if self.chunks_total != expected {
            return Err(Error::Corrupt("chunk count inconsistent".into()));
        }
        if self.old_start.checked_add_signed(self.displacement).is_none() {
            return Err(Error::Corrupt("target start overflows".into()));
        }
        if self.old_start.checked_add(self.len).is_none() {
            return Err(Error::Corrupt("source end overflows".into()));
        }
        if self.target_start().checked_add(self.len).is_none() {
            return Err(Error::Corrupt("target end overflows".into()));
        }
        if !self.widen_forward && self.union_lower() == 0 {
            return Err(Error::Corrupt("no sector before union".into()));
        }
        Ok(())
    }
}

// --- Helpers -----------------------------------------------------------

fn checksum(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Physical LBA to read from when the copy wants `source`.
///
/// Only fires in the widened-forward, forward-move case: the first sector
/// of the extended range is the metadata, and its original contents are
/// parked at the shadow.
fn source_lba(m: Metadata, source: u64) -> u64 {
    if m.widen_forward() && m.is_forward() && source == m.metadata_lba() {
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
fn destination_lba(m: Metadata, destination: u64) -> u64 {
    if m.widen_forward() && !m.is_forward() && destination == m.metadata_lba() {
        m.shadow_lba()
    } else {
        destination
    }
}

fn chunk_bounds(m: Metadata, chunk_index: u64) -> (u64, u64, u64) {
    debug_assert!(chunk_index < m.chunks_total());
    let source = m.old_start() + chunk_index * m.chunk_size();
    let destination = (source as i128 + m.displacement() as i128) as u64;
    let len = m.chunk_size().min(m.partition_len() - chunk_index * m.chunk_size());
    (source, destination, len)
}

/// Which chunk to process next, given how many are already done.
fn processing_index(m: Metadata, chunks_done: u64) -> u64 {
    if m.is_forward() {
        m.chunks_total() - 1 - chunks_done
    } else {
        chunks_done
    }
}

fn is_sentinel(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == SENTINEL)
}

/// Offsets within a chunk that carry a checkpoint. The last sector of
/// every subchunk, plus the final sector of the chunk if the chunk is not
/// a whole multiple of the subchunk size.
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

fn write_metadata(
    disk: &mut dyn Disk,
    lba: u64,
    metadata: &Metadata,
    store: &mut Option<&mut dyn MetaStore>,
) -> Result<(), Error> {
    let encoded = metadata.encode();
    if let Some(s) = store.as_deref_mut() {
        s.write(&encoded)?;
    }
    disk.write_sector(lba, &encoded)?;
    disk.sync()
}

fn read_metadata(
    disk: &mut dyn Disk,
    lba: u64,
    store: &mut Option<&mut dyn MetaStore>,
) -> Result<Option<Metadata>, Error> {
    if let Some(s) = store.as_deref_mut() {
        if let Some(data) = s.read()? {
            if let Some(m) = Metadata::decode(&data) {
                return Ok(Some(m));
            }
        }
    }
    let mut buf = [0u8; SECTOR_SIZE];
    disk.read_sector(lba, &mut buf)?;
    Ok(Metadata::decode(&buf))
}

// --- Parameters --------------------------------------------------------

/// A request to start a new move. Constructed once, validated once.
#[derive(Clone, Copy, Debug)]
pub struct MoveRequest {
    pub old_start: u64,
    pub len: u64,
    pub new_start: u64,
    pub chunk_size: u64,
    pub subchunk_size: u64,
}

impl MoveRequest {
    pub fn new(
        old_start: u64,
        len: u64,
        new_start: u64,
        chunk_size: u64,
        subchunk_size: u64,
    ) -> Result<Self, Error> {
        if len == 0 {
            return Err(Error::InvalidParams("zero length".into()));
        }
        if new_start == old_start {
            return Err(Error::InvalidParams("zero displacement".into()));
        }
        if subchunk_size < CHECKPOINT_SECTORS {
            return Err(Error::InvalidParams(
                "subchunk smaller than one checkpoint".into(),
            ));
        }
        Ok(Self { old_start, len, new_start, chunk_size, subchunk_size })
    }
}

// --- Entry points ------------------------------------------------------

/// Start a new move, or resume an interrupted one.
///
/// If `request` is `None`, only recovery is attempted. If a move is
/// already in flight, the request is ignored — the stored metadata is
/// authoritative.
///
/// The store is optional. When supplied, it is written before every
/// on-disk metadata sector so that a crash inside the resize phase can
/// be recovered. When absent, a crash inside resize is unrecoverable and
/// the caller must accept that risk.
pub fn execute(
    disk: &mut dyn Disk,
    request: Option<MoveRequest>,
    mut store: Option<&mut dyn MetaStore>,
) -> Result<(), Error> {
    let partition = disk.get_partition()?;

    if let Some(metadata) = read_metadata(disk, partition.start(), &mut store)? {
        if metadata.phase() != Phase::Resize && partition != metadata.partition_range() {
            return Err(Error::Corrupt(format!(
                "partition extent {partition:?} does not match metadata {expected:?}",
                expected = metadata.partition_range(),
            )));
        }
        return resume(disk, metadata.metadata_lba(), metadata, &mut store);
    }

    match request {
        Some(req) => start(disk, req, store),
        None => Err(Error::NoTransaction),
    }
}

/// Begin a new move.
pub fn start(
    disk: &mut dyn Disk,
    request: MoveRequest,
    mut store: Option<&mut dyn MetaStore>,
) -> Result<(), Error> {
    let displacement_i128 = request.new_start as i128 - request.old_start as i128;
    let displacement = i64::try_from(displacement_i128).map_err(|_| {
        Error::InvalidParams("displacement does not fit in i64".into())
    })?;

    let chunk_size = request.chunk_size.min(displacement.unsigned_abs());
    if chunk_size == 0 {
        return Err(Error::InvalidParams("chunk size clamps to zero".into()));
    }
    let subchunk_size = request.subchunk_size.min(chunk_size);
    if subchunk_size < CHECKPOINT_SECTORS {
        return Err(Error::InvalidParams(
            "subchunk clamps below one checkpoint".into(),
        ));
    }

    let union_lower = request.old_start.min(request.new_start);
    let union_upper = (request.old_start + request.len)
        .max(request.new_start + request.len);

    if !disk.is_range_free(request.new_start, request.new_start + request.len)? {
        return Err(Error::InvalidParams(
            "target range overlaps another partition or is out of bounds".into(),
        ));
    }

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

    let metadata = Metadata::new(
        request.old_start,
        request.len,
        displacement,
        chunk_size,
        subchunk_size,
        widen_forward,
    )?;

    disk.set_partition(metadata.partition_range())?;
    disk.sync()?;

    if metadata.widen_forward() && metadata.is_forward() {
        let mut saved = [0u8; SECTOR_SIZE];
        disk.read_sector(metadata.metadata_lba(), &mut saved)?;
        disk.write_sector(metadata.shadow_lba(), &saved)?;
        disk.sync()?;
    }

    write_metadata(disk, metadata.metadata_lba(), &metadata, &mut store)?;
    resume(disk, metadata.metadata_lba(), metadata, &mut store)
}

// --- Phase driver ------------------------------------------------------

fn resume(
    disk: &mut dyn Disk,
    metadata_lba: u64,
    mut metadata: Metadata,
    store: &mut Option<&mut dyn MetaStore>,
) -> Result<(), Error> {
    loop {
        match metadata.phase() {
            Phase::Init => {
                metadata.set_phase(Phase::Zero);
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }

            Phase::Zero => {
                if metadata.chunks_done() >= metadata.chunks_total() {
                    metadata.set_phase(Phase::Verify);
                    write_metadata(disk, metadata_lba, &metadata, store)?;
                    continue;
                }
                let index = processing_index(metadata, metadata.chunks_done());
                zero_checkpoints(disk, metadata, index)?;
                disk.sync()?;
                metadata.set_phase(Phase::Copy);
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }

            Phase::Copy => {
                let index = processing_index(metadata, metadata.chunks_done());
                copy_chunk(disk, metadata, index)?;
                disk.sync()?;
                metadata.set_phase(Phase::Finalize);
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }

            Phase::Finalize => {
                let index = processing_index(metadata, metadata.chunks_done());
                finalize_chunk(disk, metadata, index)?;
                disk.sync()?;
                metadata.advance_chunk();
                metadata.set_phase(if metadata.chunks_done() >= metadata.chunks_total() {
                    Phase::Verify
                } else {
                    Phase::Zero
                });
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }

            Phase::Verify => {
                verify(disk, metadata)?;
                metadata.set_phase(Phase::Resize);
                write_metadata(disk, metadata_lba, &metadata, store)?;
            }

            Phase::Resize => {
                return resize(disk, metadata_lba, metadata, store);
            }
        }
    }
}

// --- Phase bodies ------------------------------------------------------

fn zero_checkpoints(
    disk: &mut dyn Disk,
    m: Metadata,
    chunk_index: u64,
) -> Result<(), Error> {
    let (_, destination, chunk_len) = chunk_bounds(m, chunk_index);
    let zero = [0u8; SECTOR_SIZE];
    for offset in checkpoint_offsets(chunk_len, m.subchunk_size()) {
        disk.write_sector(destination_lba(m, destination + offset), &zero)?;
    }
    Ok(())
}

fn copy_chunk(
    disk: &mut dyn Disk,
    m: Metadata,
    chunk_index: u64,
) -> Result<(), Error> {
    debug_assert_eq!(m.phase(), Phase::Copy);
    let (source, destination, chunk_len) = chunk_bounds(m, chunk_index);
    let checkpoints = checkpoint_offsets(chunk_len, m.subchunk_size());

    let mut resume_offset = 0u64;
    let mut buf = [0u8; SECTOR_SIZE];
    for &offset in &checkpoints {
        disk.read_sector(destination_lba(m, destination + offset), &mut buf)?;
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
        let physical_destination = destination_lba(m, destination + offset);
        if checkpoint_set.contains(&offset) {
            disk.write_sector(physical_destination, &sentinel)?;
        } else {
            let physical_source = source_lba(m, source + offset);
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
    m: Metadata,
    chunk_index: u64,
) -> Result<(), Error> {
    let (source, destination, chunk_len) = chunk_bounds(m, chunk_index);
    let mut buf = [0u8; SECTOR_SIZE];
    for offset in checkpoint_offsets(chunk_len, m.subchunk_size()) {
        let physical_destination = destination_lba(m, destination + offset);
        disk.read_sector(physical_destination, &mut buf)?;
        if is_sentinel(&buf) {
            let physical_source = source_lba(m, source + offset);
            let mut data = [0u8; SECTOR_SIZE];
            disk.read_sector(physical_source, &mut data)?;
            disk.write_sector(physical_destination, &data)?;
        }
    }
    Ok(())
}

fn verify(disk: &mut dyn Disk, m: Metadata) -> Result<(), Error> {
    for chunk_index in 0..m.chunks_total() {
        let (_, destination, chunk_len) = chunk_bounds(m, chunk_index);
        let mut buf = [0u8; SECTOR_SIZE];
        for offset in checkpoint_offsets(chunk_len, m.subchunk_size()) {
            disk.read_sector(destination_lba(m, destination + offset), &mut buf)?;
            if is_sentinel(&buf) {
                return Err(Error::Verify(format!(
                    "sentinel remains at chunk {chunk_index} offset {offset}"
                )));
            }
        }
    }
    Ok(())
}

fn resize(
    disk: &mut dyn Disk,
    metadata_lba: u64,
    m: Metadata,
    store: &mut Option<&mut dyn MetaStore>,
) -> Result<(), Error> {
    disk.set_partition(Extent::new(m.target_start(), m.target_start() + m.partition_len())?)?;
    disk.sync()?;

    if m.widen_forward() && !m.is_forward() {
        let mut saved = [0u8; SECTOR_SIZE];
        disk.read_sector(m.shadow_lba(), &mut saved)?;
        disk.write_sector(metadata_lba, &saved)?;
    } else {
        let zero = [0u8; SECTOR_SIZE];
        disk.write_sector(metadata_lba, &zero)?;
    }
    disk.sync()?;

    if let Some(s) = store.as_deref_mut() {
        s.remove();
    }
    Ok(())
}
