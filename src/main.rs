
use std::cmp::min;
use std::fmt;

pub const SECTOR: usize = 512;

/// Byte pattern written to a checkpoint that the copy has reached but that has
/// not yet been restored to real data.
const SENTINEL_BYTE: u8 = 0xA5;

const JOURNAL_MAGIC: u64 = 0x4A52_4E4C_4D4F_5601; // "JRNLMOV" + version 1

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Error {
    Io(String),
    Corrupt(String),
    /// Simulated power loss (test harness only).
    Crash,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(s) => write!(f, "io error: {s}"),
            Error::Corrupt(s) => write!(f, "corrupt state: {s}"),
            Error::Crash => write!(f, "simulated crash"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Disk abstraction
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpt {
    pub start: u64,
    pub len: u64,
}

/// Everything the algorithm needs from the outside world.
///
/// `journal` and `gpt` are modelled as small, separately-persisted pieces of
/// metadata (a reserved region / the partition table itself).
pub trait Disk {
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;

    fn read_journal(&self) -> Result<Vec<u8>, Error>;
    fn write_journal(&mut self, data: &[u8]) -> Result<(), Error>;

    fn read_gpt(&self) -> Result<Gpt, Error>;
    fn write_gpt(&mut self, gpt: &Gpt) -> Result<(), Error>;
}

// ---------------------------------------------------------------------------
// In-memory disk with crash injection
// ---------------------------------------------------------------------------

pub struct MemDisk {
    pub data: Vec<u8>,
    pub journal: Vec<u8>,
    pub gpt: Gpt,
    /// Crash just before the n-th mutating operation (`None` = never crash).
    pub crash_at: Option<u64>,
    /// Number of mutating operations attempted so far.
    pub ops: u64,
}

impl MemDisk {
    pub fn new(sectors: u64) -> Self {
        MemDisk {
            data: vec![0u8; sectors as usize * SECTOR],
            journal: Vec::new(),
            gpt: Gpt { start: 0, len: 0 },
            crash_at: None,
            ops: 0,
        }
    }

    #[inline]
    fn tick(&mut self) -> Result<(), Error> {
        self.ops += 1;
        if Some(self.ops) == self.crash_at {
            return Err(Error::Crash);
        }
        Ok(())
    }

    fn span(&self, lba: u64, len: usize) -> Result<std::ops::Range<usize>, Error> {
        let off = lba as usize * SECTOR;
        let end = off + len;
        if end > self.data.len() {
            return Err(Error::Io(format!("lba {lba} out of range")));
        }
        Ok(off..end)
    }
}

impl Disk for MemDisk {
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        let r = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[r]);
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        let r = self.span(lba, buf.len())?;
        self.tick()?; // crash *before* the write lands
        self.data[r].copy_from_slice(buf);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.tick()
    }

    fn read_journal(&self) -> Result<Vec<u8>, Error> {
        Ok(self.journal.clone())
    }

    fn write_journal(&mut self, data: &[u8]) -> Result<(), Error> {
        self.tick()?;
        self.journal = data.to_vec();
        Ok(())
    }

    fn read_gpt(&self) -> Result<Gpt, Error> {
        Ok(self.gpt)
    }

    fn write_gpt(&mut self, gpt: &Gpt) -> Result<(), Error> {
        self.tick()?;
        self.gpt = *gpt;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    /// Transaction recorded; nothing else done yet.
    Recorded = 0,
    /// Temporary (enlarged) GPT extent established.
    ExtentSet = 1,
    /// All chunks copied and finalized.
    DataMoved = 2,
    /// Final GPT extent installed.
    GeometryFinal = 3,
    /// Everything done, temp state removed.
    Complete = 4,
}

impl Phase {
    fn from_u8(v: u8) -> Result<Phase, Error> {
        Ok(match v {
            0 => Phase::Recorded,
            1 => Phase::ExtentSet,
            2 => Phase::DataMoved,
            3 => Phase::GeometryFinal,
            4 => Phase::Complete,
            _ => return Err(Error::Corrupt(format!("bad phase {v}"))),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Journal {
    pub phase: Phase,
    pub old_start: u64,
    pub len: u64,
    pub new_start: u64,
    /// Signed displacement `new_start - old_start`.
    pub x: i64,
    /// Major chunk size, always `|x|`.
    pub chunk_len: u64,
    pub n_chunks: u64,
    /// Number of chunks (in *processing order*) that are fully finalized.
    pub chunks_done: u64,
    /// Checkpoints for the active chunk have been zeroed.
    pub prepared: bool,
    /// The active chunk's data copy has been completed.
    pub copy_done: bool,
    /// Sparse checkpoint interval, in sectors.
    pub interval: u64,
}

fn put_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn rd_u64(b: &[u8], o: &mut usize) -> u64 {
    let x = u64::from_le_bytes(b[*o..*o + 8].try_into().unwrap());
    *o += 8;
    x
}

impl Journal {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(96);
        put_u64(&mut v, JOURNAL_MAGIC);
        v.push(self.phase as u8);
        v.push(self.prepared as u8);
        v.push(self.copy_done as u8);
        v.push(0); // pad
        put_u64(&mut v, self.old_start);
        put_u64(&mut v, self.len);
        put_u64(&mut v, self.new_start);
        put_u64(&mut v, self.x as u64);
        put_u64(&mut v, self.chunk_len);
        put_u64(&mut v, self.n_chunks);
        put_u64(&mut v, self.chunks_done);
        put_u64(&mut v, self.interval);
        v
    }

    fn decode(b: &[u8]) -> Result<Journal, Error> {
        if b.len() < 8 + 4 + 8 * 8 {
            return Err(Error::Corrupt("journal too short".into()));
        }
        let mut o = 0usize;
        if rd_u64(b, &mut o) != JOURNAL_MAGIC {
            return Err(Error::Corrupt("bad journal magic".into()));
        }
        let phase = Phase::from_u8(b[o])?;
        o += 1;
        let prepared = b[o] != 0;
        o += 1;
        let copy_done = b[o] != 0;
        o += 1;
        o += 1; // pad
        Ok(Journal {
            phase,
            old_start: rd_u64(b, &mut o),
            len: rd_u64(b, &mut o),
            new_start: rd_u64(b, &mut o),
            x: rd_u64(b, &mut o) as i64,
            chunk_len: rd_u64(b, &mut o),
            n_chunks: rd_u64(b, &mut o),
            chunks_done: rd_u64(b, &mut o),
            interval: rd_u64(b, &mut o),
            prepared,
            copy_done,
        })
    }
}

fn persist(disk: &mut dyn Disk, j: &Journal) -> Result<(), Error> {
    disk.write_journal(&j.encode())?;
    disk.sync()
}

fn load_journal(disk: &dyn Disk) -> Result<Option<Journal>, Error> {
    let raw = disk.read_journal()?;
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(Some(Journal::decode(&raw)?))
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// The enlarged extent that covers both the old and the new partition.
fn temp_extent(j: &Journal) -> (u64, u64) {
    let old_end = j.old_start + j.len;
    let new_end = j.new_start + j.len;
    let start = min(j.old_start, j.new_start);
    let end = old_end.max(new_end);
    (start, end - start)
}

/// Index (in processing order) -> index (in address order) of the chunk.
///
/// Forward moves (`x > 0`) are processed from the highest chunk downwards so
/// that a chunk's destination is never another chunk's not-yet-copied source.
/// Backward moves (`x < 0`) are processed upwards for the same reason.
fn chunk_index(j: &Journal, k: u64) -> u64 {
    if j.x > 0 {
        j.n_chunks - 1 - k
    } else {
        k
    }
}

/// `(source_lba, dest_lba, length)` of chunk `ci`.
fn chunk_bounds(j: &Journal, ci: u64) -> (u64, u64, u64) {
    let src = j.old_start + ci * j.chunk_len;
    let dst = (src as i64 + j.x) as u64;
    let clen = min(j.chunk_len, j.len - ci * j.chunk_len);
    (src, dst, clen)
}

// ---------------------------------------------------------------------------
// The move itself
// ---------------------------------------------------------------------------

/// Begin a new move.  `interval` is the checkpoint spacing in sectors.
pub fn start_move(
    disk: &mut dyn Disk,
    old_start: u64,
    len: u64,
    new_start: u64,
    interval: u64,
) -> Result<(), Error> {
    if interval == 0 {
        return Err(Error::Io("checkpoint interval must be >= 1".into()));
    }
    if len == 0 {
        return Err(Error::Io("partition length must be >= 1".into()));
    }
    let x = new_start as i64 - old_start as i64;
    if x == 0 {
        return Err(Error::Io("no displacement".into()));
    }
    let chunk_len = x.unsigned_abs();
    let n_chunks = (len + chunk_len - 1) / chunk_len;

    let j = Journal {
        phase: Phase::Recorded,
        old_start,
        len,
        new_start,
        x,
        chunk_len,
        n_chunks,
        chunks_done: 0,
        prepared: false,
        copy_done: false,
        interval,
    };

    // Milestone 1: the transaction exists.
    persist(disk, &j)?;
    resume(disk)
}

/// Drive the move (or a fresh boot's recovery) to completion.
///
/// Safe to call at any time; if there is no pending transaction it is a no-op.
pub fn resume(disk: &mut dyn Disk) -> Result<(), Error> {
    let mut j = match load_journal(disk)? {
        Some(j) => j,
        None => return Ok(()),
    };

    loop {
        match j.phase {
            // -- Milestone 2: temporary enlarged extent ----------------------
            // Establishing it is idempotent, so if we crashed before the
            // ExtentSet record landed we simply establish it again.
            Phase::Recorded => {
                let (s, l) = temp_extent(&j);
                disk.write_gpt(&Gpt { start: s, len: l })?;
                disk.sync()?;
                j.phase = Phase::ExtentSet;
                persist(disk, &j)?;
            }

            // -- Milestones 3..6: chunked data movement ----------------------
            Phase::ExtentSet => {
                move_all_chunks(disk, &mut j)?;
                j.phase = Phase::DataMoved;
                persist(disk, &j)?;
            }

            // -- Milestone 8: final geometry ---------------------------------
            Phase::DataMoved => {
                disk.write_gpt(&Gpt {
                    start: j.new_start,
                    len: j.len,
                })?;
                disk.sync()?;
                j.phase = Phase::GeometryFinal;
                persist(disk, &j)?;
            }

            // -- Milestone 9: cleanup ----------------------------------------
            Phase::GeometryFinal => {
                j.phase = Phase::Complete;
                persist(disk, &j)?;
            }

            Phase::Complete => {
                disk.write_journal(&[])?;
                disk.sync()?;
                return Ok(());
            }
        }
    }
}

fn move_all_chunks(disk: &mut dyn Disk, j: &mut Journal) -> Result<(), Error> {
    while j.chunks_done < j.n_chunks {
        let ci = chunk_index(j, j.chunks_done);
        let (src, dst, clen) = chunk_bounds(j, ci);

        // Step 3: prepare the sparse checkpoints of the active chunk.
        if !j.prepared {
            prepare_checkpoints(disk, dst, clen, j.interval)?;
            disk.sync()?;
            j.prepared = true;
            j.copy_done = false;
            persist(disk, j)?;
        }

        // Step 4: move the chunk.
        if !j.copy_done {
            copy_chunk(disk, src, dst, clen, j.interval)?;
            disk.sync()?;
            j.copy_done = true;
            persist(disk, j)?;
        }

        // Step 5: restore the checkpoints from the (still intact) source.
        finalize_chunk(disk, src, dst, clen, j.interval)?;
        disk.sync()?;

        // Step 6: this chunk is done; never touched again.
        j.chunks_done += 1;
        j.prepared = false;
        j.copy_done = false;
        persist(disk, j)?;
    }
    Ok(())
}

/// Zero every checkpoint sector in the destination.  Idempotent.
fn prepare_checkpoints(
    disk: &mut dyn Disk,
    dst: u64,
    clen: u64,
    interval: u64,
) -> Result<(), Error> {
    let zero = [0u8; SECTOR];
    let mut o = 0u64;
    while o < clen {
        disk.write(dst + o, &zero)?;
        o += interval;
    }
    Ok(())
}

/// Copy the chunk, resuming from the last checkpoint that carries a sentinel.
fn copy_chunk(
    disk: &mut dyn Disk,
    src: u64,
    dst: u64,
    clen: u64,
    interval: u64,
) -> Result<(), Error> {
    // Determine where to resume: the first checkpoint still holding zeros has
    // not been reached, so everything up to the previous one is good.
    let mut resume = 0u64;
    let mut o = 0u64;
    let mut buf = [0u8; SECTOR];
    while o < clen {
        disk.read(dst + o, &mut buf)?;
        if buf.iter().all(|&b| b == 0) {
            break;
        }
        resume = o + 1;
        o += interval;
    }

    let sentinel = [SENTINEL_BYTE; SECTOR];
    let mut off = resume;
    while off < clen {
        if off % interval == 0 {
            // A checkpoint only records *progress*; its real data is written
            // later by `finalize_chunk`, which keeps write amplification at
            // 2 extra writes per checkpoint.
            disk.write(dst + off, &sentinel)?;
        } else {
            disk.read(src + off, &mut buf)?;
            disk.write(dst + off, &buf)?;
        }
        off += 1;
    }
    Ok(())
}

/// Replace temporary checkpoint data with the real data.
///
/// The source is still intact here (the next chunk has not been started), so
/// the data is re-read rather than journaled.  Idempotent.
fn finalize_chunk(
    disk: &mut dyn Disk,
    src: u64,
    dst: u64,
    clen: u64,
    interval: u64,
) -> Result<(), Error> {
    let mut buf = [0u8; SECTOR];
    let mut o = 0u64;
    while o < clen {
        disk.read(src + o, &mut buf)?;
        disk.write(dst + o, &buf)?;
        o += interval;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Demo + tests
// ---------------------------------------------------------------------------

fn main() {
    const OLD_START: u64 = 100;
    const LEN: u64 = 600;
    const NEW_START: u64 = 400;
    const INTERVAL: u64 = 64;

    let (mut disk, original) = setup(OLD_START, LEN, NEW_START, INTERVAL);
    println!(
        "moving {} sectors from {} to {} (displacement {})",
        LEN,
        OLD_START,
        NEW_START,
        NEW_START as i64 - OLD_START as i64
    );

    start_move(&mut disk, OLD_START, LEN, NEW_START, INTERVAL).expect("move failed");

    check_region(&disk, NEW_START, &original);
    println!(
        "move complete; gpt = [{}, {}) after {} writes",
        disk.gpt.start,
        disk.gpt.start + disk.gpt.len,
        disk.ops
    );
}

// --- test helpers ----------------------------------------------------------

fn fill(disk: &mut MemDisk) {
    for i in 0..disk.data.len() / SECTOR {
        let b = (i as u8).wrapping_mul(7).wrapping_add(3);
        disk.data[i * SECTOR..(i + 1) * SECTOR].fill(b);
    }
}

fn setup(old_start: u64, len: u64, new_start: u64, interval: u64) -> (MemDisk, Vec<u8>) {
    let _ = (new_start, interval);
    let mut disk = MemDisk::new(2048);
    fill(&mut disk);

    let mut orig = vec![0u8; len as usize * SECTOR];
    for i in 0..len {
        let lba = old_start + i;
        orig[i as usize * SECTOR..(i as usize + 1) * SECTOR]
            .copy_from_slice(&disk.data[lba as usize * SECTOR..(lba as usize + 1) * SECTOR]);
    }
    disk.gpt = Gpt {
        start: old_start,
        len,
    };
    (disk, orig)
}

fn check_region(disk: &MemDisk, start: u64, expected: &[u8]) {
    let mut buf = [0u8; SECTOR];
    for i in 0..expected.len() / SECTOR {
        let lba = start + i as u64;
        disk.read(lba, &mut buf).unwrap();
        assert_eq!(
            &buf[..],
            &expected[i * SECTOR..(i + 1) * SECTOR],
            "sector {lba} mismatch"
        );
    }
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: u64 = 100;
    const LEN: u64 = 600;
    const NEW: u64 = 400;
    const INTERVAL: u64 = 64;

    #[test]
    fn clean_move_forward() {
        let (mut d, orig) = setup(OLD, LEN, NEW, INTERVAL);
        start_move(&mut d, OLD, LEN, NEW, INTERVAL).unwrap();
        assert_eq!(
            d.gpt,
            Gpt {
                start: NEW,
                len: LEN
            }
        );
        check_region(&d, NEW, &orig);
        assert!(d.journal.is_empty(), "journal should be cleaned up");
    }

    #[test]
    fn clean_move_backward() {
        let (mut d, orig) = setup(NEW, LEN, OLD, INTERVAL);
        start_move(&mut d, NEW, LEN, OLD, INTERVAL).unwrap();
        assert_eq!(
            d.gpt,
            Gpt {
                start: OLD,
                len: LEN
            }
        );
        check_region(&d, OLD, &orig);
        assert!(d.journal.is_empty());
    }

    /// Crash at every single write, recover, and check the outcome.
    #[test]
    fn crash_recovery_exhaustive_forward() {
        exhaustive(OLD, LEN, NEW, INTERVAL);
    }

    #[test]
    fn crash_recovery_exhaustive_backward() {
        exhaustive(NEW, LEN, OLD, INTERVAL);
    }

    fn exhaustive(old: u64, len: u64, new: u64, interval: u64) {
        // Baseline run to learn how many writes a full move performs.
        let (mut d, _orig) = setup(old, len, new, interval);
        start_move(&mut d, old, len, new, interval).unwrap();
        let total = d.ops;
        assert!(total > 10);

        for crash_at in 1..=total {
            let (mut d, orig) = setup(old, len, new, interval);
            d.crash_at = Some(crash_at);

            let r = start_move(&mut d, old, len, new, interval);
            assert!(r.is_err(), "crash {crash_at} was not injected");

            // "Reboot": crash injection off, run recovery.
            d.crash_at = None;
            resume(&mut d).unwrap_or_else(|e| panic!("recovery after {crash_at} failed: {e}"));

            let target = Gpt {
                start: new,
                len,
            };
            if d.gpt == target {
                check_region(&d, new, &orig);
            } else {
                // The crash landed before the transaction was durably
                // recorded: the move must be a clean no-op.
                assert_eq!(crash_at, 1, "crash {crash_at} left gpt = {:?}", d.gpt);
                assert_eq!(d.gpt, Gpt { start: old, len });
                check_region(&d, old, &orig);
            }
        }
    }
}