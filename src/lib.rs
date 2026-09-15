//! Crash-recoverable overlapping partition move.

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
const VERSION: u16 = 1;
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
    fn from_u16(v: u16) -> Option<Phase> {
        Some(match v {
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
// GPT and Disk
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpt {
    pub start: u64,
    pub len: u64,
}

pub trait Disk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;
    fn get_partition(&mut self) -> Result<Gpt, Error>;
    fn set_partition(&mut self, gpt: Gpt) -> Result<(), Error>;
}

// ---------------------------------------------------------------------------
// Metadata backup store
// ---------------------------------------------------------------------------

/// Out-of-band mirror of the metadata sector.  The mirror is written
/// *before* the sector, so it is never stale relative to the disk.  It is
/// load-bearing: without it, a crash inside the resize phase is
/// unrecoverable because the metadata sector has already been overwritten
/// by moved data.
pub trait MetaStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error>;
    /// `Ok(None)` means "no record here"; `Err` means the read itself
    /// failed and recovery must not treat the absence as authoritative.
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error>;
    fn remove(&mut self);
}

/// A store that does nothing.  Suitable only when the caller knows the
/// resize phase cannot be interrupted (e.g. battery-backed write cache).
pub struct NullStore;

impl MetaStore for NullStore {
    fn write(&mut self, _data: &[u8]) -> Result<(), Error> { Ok(()) }
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> { Ok(None) }
    fn remove(&mut self) {}
}

/// File-backed store.  The only place in the crate that touches `std::fs`.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl MetaStore for FileStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut f = fs::File::create(&self.path).map_err(|e| Error::Io(e.to_string()))?;
        f.write_all(data).map_err(|e| Error::Io(e.to_string()))?;
        f.sync_all().map_err(|e| Error::Io(e.to_string()))?;
        Ok(())
    }
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        match fs::read(&self.path) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e.to_string())),
        }
    }
    fn remove(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// In-memory store for tests.  No filesystem, no syscalls, no fsync.
#[derive(Default)]
pub struct MemStore {
    data: Option<Vec<u8>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self { data: None }
    }
}

impl MetaStore for MemStore {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.data = Some(data.to_vec());
        Ok(())
    }
    fn read(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.data.clone())
    }
    fn remove(&mut self) {
        self.data = None;
    }
}

// ---------------------------------------------------------------------------
// Checksum
// ---------------------------------------------------------------------------

fn checksum(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub phase: Phase,
    pub uses_shadow: bool,
    pub old_start: u64,
    pub len: u64,
    pub displacement: i64,
    pub chunk_size: u64,
    pub subchunk_size: u64,
    pub chunks_total: u64,
    pub chunks_done: u64,
}

impl Metadata {
    fn encode(&self) -> [u8; SECTOR_SIZE] {
        let mut b = [0u8; SECTOR_SIZE];
        b[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&(self.phase as u16).to_le_bytes());
        b[12] = self.uses_shadow as u8;
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
        if b.len() < SECTOR_SIZE {
            return None;
        }
        if u64::from_le_bytes(b[0..8].try_into().ok()?) != MAGIC {
            return None;
        }
        if u16::from_le_bytes(b[8..10].try_into().ok()?) != VERSION {
            return None;
        }
        let stored = u32::from_le_bytes(b[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].try_into().ok()?);
        if stored != checksum(&b[0..CHECKSUM_OFFSET]) {
            return None;
        }
        let m = Metadata {
            phase: Phase::from_u16(u16::from_le_bytes(b[10..12].try_into().ok()?))?,
            uses_shadow: b[12] != 0,
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
        let expected = (self.len + self.chunk_size - 1) / self.chunk_size;
        if self.chunks_total != expected {
            return Err(Error::Corrupt("chunks_total inconsistent".into()));
        }
        if !self.uses_shadow {
            let new_start = (self.old_start as i64 + self.displacement) as u64;
            if self.old_start.min(new_start) == 0 {
                return Err(Error::Corrupt("no room behind union".into()));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn is_forward(m: &Metadata) -> bool {
    m.displacement > 0
}

fn union_bounds(m: &Metadata) -> (u64, u64) {
    let new_start = (m.old_start as i64 + m.displacement) as u64;
    let u = m.old_start.min(new_start);
    let e = (m.old_start + m.len).max(new_start + m.len);
    (u, e)
}

fn metadata_lba(m: &Metadata) -> u64 {
    let (u, _) = union_bounds(m);
    if m.uses_shadow { u } else { u - 1 }
}

fn extended_gpt(m: &Metadata) -> Gpt {
    let (u, e) = union_bounds(m);
    if m.uses_shadow {
        Gpt { start: u, len: e - u + 1 }
    } else {
        Gpt { start: u - 1, len: e - u + 1 }
    }
}

fn chunk_bounds(m: &Metadata, ci: u64) -> (u64, u64, u64) {
    debug_assert!(ci < m.chunks_total);
    let src = m.old_start + ci * m.chunk_size;
    let dst = (src as i64 + m.displacement) as u64;
    let clen = m.chunk_size.min(m.len - ci * m.chunk_size);
    (src, dst, clen)
}

fn processing_index(m: &Metadata, k: u64) -> u64 {
    if is_forward(m) {
        m.chunks_total - 1 - k
    } else {
        k
    }
}

fn map_source(m: &Metadata, src: u64) -> u64 {
    let (u, e) = union_bounds(m);
    if m.uses_shadow && is_forward(m) && src == u { e } else { src }
}

fn map_dest(m: &Metadata, dst: u64) -> u64 {
    let (u, e) = union_bounds(m);
    if m.uses_shadow && !is_forward(m) && dst == u { e } else { dst }
}

fn is_sentinel(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == SENTINEL)
}

fn checkpoint_offsets(clen: u64, subchunk: u64) -> Vec<u64> {
    let mut v = Vec::new();
    let mut off = subchunk;
    while off <= clen {
        v.push(off - 1);
        off += subchunk;
    }
    if v.last().copied() != Some(clen - 1) {
        v.push(clen - 1);
    }
    v
}

// ---------------------------------------------------------------------------
// Metadata persistence
// ---------------------------------------------------------------------------

fn write_metadata(
    disk: &mut dyn Disk,
    lba: u64,
    m: &Metadata,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let buf = m.encode();
    store.write(&buf)?;
    disk.write_sector(lba, &buf)?;
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
// Parameters
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct MoveParams {
    pub old_start: u64,
    pub len: u64,
    pub new_start: u64,
    pub chunk_size: u64,
    pub subchunk_size: u64,
    pub uses_shadow: bool,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn run(
    disk: &mut dyn Disk,
    params: Option<MoveParams>,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let gpt = disk.get_partition()?;
    if let Some(m) = read_metadata(disk, gpt.start, store)? {
        return resume(disk, metadata_lba(&m), m, store);
    }
    match params {
        Some(p) => start(disk, p, store),
        None => Err(Error::NoTransaction),
    }
}

pub fn start(
    disk: &mut dyn Disk,
    p: MoveParams,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let x = p.new_start as i64 - p.old_start as i64;
    if x == 0 {
        return Err(Error::InvalidParams("zero displacement".into()));
    }
    if p.len == 0 {
        return Err(Error::InvalidParams("zero length".into()));
    }
    if p.subchunk_size < CHECKPOINT_SECTORS {
        return Err(Error::InvalidParams("subchunk smaller than a checkpoint".into()));
    }

    let chunk_size = p.chunk_size.min(x.unsigned_abs());
    if chunk_size == 0 {
        return Err(Error::InvalidParams("chunk size clamps to zero".into()));
    }
    let subchunk_size = p.subchunk_size.min(chunk_size);
    if subchunk_size < CHECKPOINT_SECTORS {
        return Err(Error::InvalidParams("subchunk clamps below one checkpoint".into()));
    }

    let new_start = p.new_start;
    let u = p.old_start.min(new_start);
    let e = (p.old_start + p.len).max(new_start + p.len);
    if !p.uses_shadow && u == 0 {
        return Err(Error::InvalidParams(
            "no room behind union and no shadow requested".into(),
        ));
    }

    let m = Metadata {
        phase: Phase::Init,
        uses_shadow: p.uses_shadow,
        old_start: p.old_start,
        len: p.len,
        displacement: x,
        chunk_size,
        subchunk_size,
        chunks_total: (p.len + chunk_size - 1) / chunk_size,
        chunks_done: 0,
    };
    m.validate()?;

    disk.set_partition(extended_gpt(&m))?;
    disk.sync()?;

    if m.uses_shadow && is_forward(&m) {
        let mut tmp = [0u8; SECTOR_SIZE];
        disk.read_sector(u, &mut tmp)?;
        disk.write_sector(e, &tmp)?;
        disk.sync()?;
    }

    write_metadata(disk, metadata_lba(&m), &m, store)?;
    resume(disk, metadata_lba(&m), m, store)
}

// ---------------------------------------------------------------------------
// Phase driver
// ---------------------------------------------------------------------------

fn resume(
    disk: &mut dyn Disk,
    meta_lba: u64,
    mut m: Metadata,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    loop {
        match m.phase {
            Phase::Init => {
                m.phase = Phase::Zero;
                write_metadata(disk, meta_lba, &m, store)?;
            }

            Phase::Zero => {
                if m.chunks_done >= m.chunks_total {
                    m.phase = Phase::Verify;
                    write_metadata(disk, meta_lba, &m, store)?;
                    continue;
                }
                let ci = processing_index(&m, m.chunks_done);
                zero_checkpoints(disk, &m, ci)?;
                disk.sync()?;
                m.phase = Phase::Copy;
                write_metadata(disk, meta_lba, &m, store)?;
            }

            Phase::Copy => {
                let ci = processing_index(&m, m.chunks_done);
                copy_chunk(disk, &m, ci)?;
                disk.sync()?;
                m.phase = Phase::Finalize;
                write_metadata(disk, meta_lba, &m, store)?;
            }

            Phase::Finalize => {
                let ci = processing_index(&m, m.chunks_done);
                finalize_chunk(disk, &m, ci)?;
                disk.sync()?;
                m.chunks_done += 1;
                m.phase = if m.chunks_done >= m.chunks_total {
                    Phase::Verify
                } else {
                    Phase::Zero
                };
                write_metadata(disk, meta_lba, &m, store)?;
            }

            Phase::Verify => {
                verify(disk, &m)?;
                m.phase = Phase::Resize;
                write_metadata(disk, meta_lba, &m, store)?;
            }

            Phase::Resize => {
                return do_resize(disk, meta_lba, &m, store);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase bodies
// ---------------------------------------------------------------------------

fn zero_checkpoints(disk: &mut dyn Disk, m: &Metadata, ci: u64) -> Result<(), Error> {
    let (_, dst, clen) = chunk_bounds(m, ci);
    let zero = [0u8; SECTOR_SIZE];
    for off in checkpoint_offsets(clen, m.subchunk_size) {
        disk.write_sector(map_dest(m, dst + off), &zero)?;
    }
    Ok(())
}

fn copy_chunk(disk: &mut dyn Disk, m: &Metadata, ci: u64) -> Result<(), Error> {
    debug_assert_eq!(m.phase, Phase::Copy);
    let (src, dst, clen) = chunk_bounds(m, ci);
    let checkpoints = checkpoint_offsets(clen, m.subchunk_size);

    let mut resume_off = 0u64;
    let mut buf = [0u8; SECTOR_SIZE];
    for &c in &checkpoints {
        disk.read_sector(map_dest(m, dst + c), &mut buf)?;
        if is_sentinel(&buf) {
            resume_off = c + 1;
        } else {
            break;
        }
    }

    let cp_set: HashSet<u64> = checkpoints.iter().copied().collect();
    let sentinel = [SENTINEL; SECTOR_SIZE];
    let mut off = resume_off;
    while off < clen {
        let phys_dst = map_dest(m, dst + off);
        if cp_set.contains(&off) {
            disk.write_sector(phys_dst, &sentinel)?;
        } else {
            let phys_src = map_source(m, src + off);
            let mut data = [0u8; SECTOR_SIZE];
            disk.read_sector(phys_src, &mut data)?;
            disk.write_sector(phys_dst, &data)?;
        }
        off += 1;
    }
    Ok(())
}

fn finalize_chunk(disk: &mut dyn Disk, m: &Metadata, ci: u64) -> Result<(), Error> {
    let (src, dst, clen) = chunk_bounds(m, ci);
    let mut buf = [0u8; SECTOR_SIZE];
    for off in checkpoint_offsets(clen, m.subchunk_size) {
        let phys_dst = map_dest(m, dst + off);
        disk.read_sector(phys_dst, &mut buf)?;
        if is_sentinel(&buf) {
            let phys_src = map_source(m, src + off);
            let mut data = [0u8; SECTOR_SIZE];
            disk.read_sector(phys_src, &mut data)?;
            disk.write_sector(phys_dst, &data)?;
        }
    }
    Ok(())
}

fn verify(disk: &mut dyn Disk, m: &Metadata) -> Result<(), Error> {
    for ci in 0..m.chunks_total {
        let (_, dst, clen) = chunk_bounds(m, ci);
        let mut buf = [0u8; SECTOR_SIZE];
        for off in checkpoint_offsets(clen, m.subchunk_size) {
            disk.read_sector(map_dest(m, dst + off), &mut buf)?;
            if is_sentinel(&buf) {
                return Err(Error::Verify(format!(
                    "sentinel at chunk {ci} offset {off}"
                )));
            }
        }
    }
    Ok(())
}

fn do_resize(
    disk: &mut dyn Disk,
    meta_lba: u64,
    m: &Metadata,
    store: &mut dyn MetaStore,
) -> Result<(), Error> {
    let final_start = (m.old_start as i64 + m.displacement) as u64;
    disk.set_partition(Gpt { start: final_start, len: m.len })?;
    disk.sync()?;

    if m.uses_shadow && !is_forward(m) {
        let (_, e) = union_bounds(m);
        let mut tmp = [0u8; SECTOR_SIZE];
        disk.read_sector(e, &mut tmp)?;
        disk.write_sector(meta_lba, &tmp)?;
    } else {
        let zero = [0u8; SECTOR_SIZE];
        disk.write_sector(meta_lba, &zero)?;
    }
    disk.sync()?;

    store.remove();
    Ok(())
}
