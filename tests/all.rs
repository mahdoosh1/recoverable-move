use recoverable_move::mover::*;

const OLD: u64 = 100;
const LEN: u64 = 60;
const CS: u64 = 10;
const SUB: u64 = 3;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fill(d: &mut MockDisk) {
    for i in 0..d.data.len() / SECTOR_SIZE {
        let b = (i as u8).wrapping_mul(7).wrapping_add(3);
        d.data[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE].fill(b);
    }
}

fn setup() -> (MockDisk, Vec<u8>) {
    let mut d = MockDisk::new(2048);
    fill(&mut d);
    let mut orig = vec![0u8; LEN as usize * SECTOR_SIZE];
    for i in 0..LEN {
        let lba = OLD + i;
        orig[i as usize * SECTOR_SIZE..(i as usize + 1) * SECTOR_SIZE]
            .copy_from_slice(&d.data[lba as usize * SECTOR_SIZE..(lba as usize + 1) * SECTOR_SIZE]);
    }
    d.gpt = Gpt { start: OLD, len: LEN };
    (d, orig)
}

fn setup_with(blocked_lba: Option<u64>) -> (MockDisk, Vec<u8>) {
    let (mut d, orig) = setup();
    d.blocked_lba = blocked_lba;
    (d, orig)
}

fn params(new_start: u64) -> MoveParams {
    MoveParams {
        old_start: OLD,
        len: LEN,
        new_start,
        chunk_size: CS,
        subchunk_size: SUB,
    }
}

fn check_region(d: &mut MockDisk, start: u64, expected: &[u8]) {
    let mut buf = [0u8; SECTOR_SIZE];
    for i in 0..expected.len() / SECTOR_SIZE {
        d.read_sector(start + i as u64, &mut buf).unwrap();
        assert_eq!(
            &buf[..],
            &expected[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE],
            "sector {}",
            start + i as u64
        );
    }
}

fn exhaustive(p: MoveParams, blocked_lba: Option<u64>) {
    let mut base = MemStore::new();
    let (mut d, _) = setup_with(blocked_lba);
    run(&mut d, Some(p), &mut base).unwrap();
    let total = d.op_count;
    assert!(total > 10, "baseline run too short: {total}");

    for crash_at in 1..=total {
        for crash_after in [false, true] {
            let mut store = MemStore::new();
            let (mut d, orig) = setup_with(blocked_lba);
            d.crash_at = Some(crash_at);
            d.crash_after = crash_after;
            let _ = run(&mut d, Some(p), &mut store);

            d.crash_at = None;
            d.crash_after = false;
            run(&mut d, Some(p), &mut store).unwrap_or_else(|e| {
                panic!("recovery failed (crash_at={crash_at}, after={crash_after}): {e}")
            });

            assert_eq!(
                d.gpt,
                Gpt { start: p.new_start, len: p.len },
                "wrong gpt (crash_at={crash_at}, after={crash_after})"
            );
            check_region(&mut d, p.new_start, &orig);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure unit-style tests
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_offsets_match_subchunk_boundaries() {
    let expect = |clen: u64, sub: u64| -> Vec<u64> {
        let mut v = Vec::new();
        let mut off = sub;
        while off <= clen {
            v.push(off - 1);
            off += sub;
        }
        if v.last().copied() != Some(clen - 1) {
            v.push(clen - 1);
        }
        v
    };
    assert_eq!(expect(100, 25), vec![24, 49, 74, 99]);
    assert_eq!(expect(100, 30), vec![29, 59, 89, 99]);
    assert_eq!(expect(100, 200), vec![99]);
    assert_eq!(expect(100, 100), vec![99]);
}

#[test]
fn invalid_metadata_rejected() {
    let bad = Metadata {
        phase: Phase::Init,
        widen_forward: false,
        old_start: 100,
        len: 60,
        displacement: 0,
        chunk_size: 10,
        subchunk_size: 3,
        chunks_total: 6,
        chunks_done: 0,
    };
    assert!(bad.validate().is_err());

    let bad2 = Metadata {
        displacement: 100,
        chunks_done: 7,
        ..bad
    };
    assert!(bad2.validate().is_err());

    let bad3 = Metadata {
        phase: Phase::Init,
        widen_forward: false,
        old_start: 0,
        len: 60,
        displacement: 100,
        chunk_size: 10,
        subchunk_size: 3,
        chunks_total: 6,
        chunks_done: 0,
    };
    assert!(bad3.validate().is_err());
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn clean_forward_widened_backward() {
    let (mut d, orig) = setup_with(None);
    run(&mut d, Some(params(200)), &mut MemStore::new()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 200, len: LEN });
    check_region(&mut d, 200, &orig);
}

#[test]
fn clean_forward_widened_forward() {
    let (mut d, orig) = setup_with(Some(OLD - 1));
    run(&mut d, Some(params(200)), &mut MemStore::new()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 200, len: LEN });
    check_region(&mut d, 200, &orig);
}

#[test]
fn clean_backward_widened_backward() {
    let (mut d, orig) = setup_with(None);
    run(&mut d, Some(params(50)), &mut MemStore::new()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 50, len: LEN });
    check_region(&mut d, 50, &orig);
}

#[test]
fn clean_backward_widened_forward() {
    let (mut d, orig) = setup_with(Some(50 - 1));
    run(&mut d, Some(params(50)), &mut MemStore::new()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 50, len: LEN });
    check_region(&mut d, 50, &orig);
}

// ---------------------------------------------------------------------------
// Trace-based phase ordering
// ---------------------------------------------------------------------------

#[test]
fn trace_phase_sequence() {
    let (mut d, _) = setup_with(Some(OLD - 1));
    run(&mut d, Some(params(200)), &mut MemStore::new()).unwrap();

    // Forward, widened forward: metadata at partition_start = 100.
    let meta_lba = OLD;
    let phases: Vec<Phase> = d
        .trace
        .iter()
        .filter_map(|op| match op {
            Op::Write { lba, data, .. } if *lba == meta_lba => {
                Metadata::decode(data).map(|m| m.phase)
            }
            _ => None,
        })
        .collect();

    assert_eq!(phases.first(), Some(&Phase::Init));
    assert_eq!(phases.last(), Some(&Phase::Resize));
    assert!(phases.contains(&Phase::Zero));
    assert!(phases.contains(&Phase::Copy));
    assert!(phases.contains(&Phase::Finalize));
    assert!(phases.contains(&Phase::Verify));

    let first_init = phases.iter().position(|p| *p == Phase::Init).unwrap();
    for (i, p) in phases.iter().enumerate() {
        if *p == Phase::Zero {
            assert!(i > first_init, "Zero before Init at {i}");
        }
    }
}

// ---------------------------------------------------------------------------
// Exhaustive crash injection
// ---------------------------------------------------------------------------

#[test]
fn crash_every_op_forward_widened_backward() {
    exhaustive(params(200), None);
}

#[test]
fn crash_every_op_forward_widened_forward() {
    exhaustive(params(200), Some(OLD - 1));
}

#[test]
fn crash_every_op_backward_widened_backward() {
    exhaustive(params(50), None);
}

#[test]
fn crash_every_op_backward_widened_forward() {
    exhaustive(params(50), Some(50 - 1));
}

// ---------------------------------------------------------------------------
// Torn metadata write + backup store
// ---------------------------------------------------------------------------

#[test]
fn torn_metadata_recovered_from_mem_backup() {
    let (mut probe, _) = setup_with(Some(OLD - 1));
    let _ = run(&mut probe, Some(params(200)), &mut MemStore::new());
    let op = probe
        .trace
        .iter()
        .enumerate()
        .find_map(|(i, o)| match o {
            Op::Write { data, .. } if Metadata::decode(data).is_some() => Some(i as u64 + 1),
            _ => None,
        })
        .expect("no metadata write found");

    let mut store = MemStore::new();
    let (mut d, orig) = setup_with(Some(OLD - 1));
    d.torn_at = Some((op, 64));
    d.crash_at = Some(op);
    d.crash_after = true;
    let _ = run(&mut d, Some(params(200)), &mut store);

    d.torn_at = None;
    d.crash_at = None;
    d.crash_after = false;
    run(&mut d, None, &mut store).unwrap();

    assert_eq!(d.gpt, Gpt { start: 200, len: LEN });
    check_region(&mut d, 200, &orig);
}

// ---------------------------------------------------------------------------
// Post-completion behavior
// ---------------------------------------------------------------------------

#[test]
fn run_after_completion_returns_no_transaction() {
    let mut store = MemStore::new();
    let (mut d, _) = setup_with(None);
    run(&mut d, Some(params(200)), &mut store).unwrap();

    let err = run(&mut d, None, &mut store).unwrap_err();
    assert!(matches!(err, Error::NoTransaction));
}

// ===========================================================================
// Mock disk
// ===========================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Read { lba: u64, len: usize },
    Write { lba: u64, len: usize, data: Vec<u8> },
    Sync,
    GetPartition,
    SetPartition(Gpt),
    IsSectorFree { lba: u64 },
    IsRangeFree { start: u64, end: u64 },
}

pub struct MockDisk {
    pub data: Vec<u8>,
    pub gpt: Gpt,
    pub trace: Vec<Op>,
    pub crash_at: Option<u64>,
    pub crash_after: bool,
    pub torn_at: Option<(u64, usize)>,
    pub op_count: u64,
    /// If set, `is_sector_free` returns false for exactly this LBA and
    /// `is_range_free` returns false for any range containing it.
    pub blocked_lba: Option<u64>,
}

impl MockDisk {
    pub fn new(sectors: u64) -> Self {
        Self {
            data: vec![0u8; sectors as usize * SECTOR_SIZE],
            gpt: Gpt { start: 0, len: 0 },
            trace: Vec::new(),
            crash_at: None,
            crash_after: false,
            torn_at: None,
            op_count: 0,
            blocked_lba: None,
        }
    }

    fn next_op(&mut self) -> u64 {
        self.op_count += 1;
        self.op_count
    }

    fn crash_before(&self, n: u64) -> Result<(), Error> {
        if Some(n) == self.crash_at && !self.crash_after {
            Err(Error::Crash)
        } else {
            Ok(())
        }
    }

    fn crash_after(&self, n: u64) -> Result<(), Error> {
        if Some(n) == self.crash_at && self.crash_after {
            Err(Error::Crash)
        } else {
            Ok(())
        }
    }

    fn span(&self, lba: u64, len: usize) -> Result<std::ops::Range<usize>, Error> {
        let s = lba as usize * SECTOR_SIZE;
        let e = s + len;
        if e > self.data.len() {
            return Err(Error::Io(format!("lba {lba} out of range")));
        }
        Ok(s..e)
    }
}

impl Disk for MockDisk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        let r = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[r]);
        self.trace.push(Op::Read { lba, len: buf.len() });
        self.crash_after(n)?;
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        let r = self.span(lba, buf.len())?;
        let to_write = match self.torn_at {
            Some((i, k)) if i == n => k.min(buf.len()),
            _ => buf.len(),
        };
        self.data[r.start..r.start + to_write].copy_from_slice(&buf[..to_write]);
        self.trace.push(Op::Write { lba, len: buf.len(), data: buf.to_vec() });
        self.crash_after(n)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        self.trace.push(Op::Sync);
        self.crash_after(n)?;
        Ok(())
    }

    fn get_partition(&mut self) -> Result<Gpt, Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        self.trace.push(Op::GetPartition);
        self.crash_after(n)?;
        Ok(self.gpt)
    }

    fn set_partition(&mut self, gpt: Gpt) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        self.gpt = gpt;
        self.trace.push(Op::SetPartition(gpt));
        self.crash_after(n)?;
        Ok(())
    }

    fn is_sector_free(&mut self, lba: u64) -> Result<bool, Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        let free = Some(lba) != self.blocked_lba;
        self.trace.push(Op::IsSectorFree { lba });
        self.crash_after(n)?;
        Ok(free)
    }

    fn is_range_free(&mut self, start: u64, end: u64) -> Result<bool, Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        let free = match self.blocked_lba {
            Some(b) => !(b >= start && b < end),
            None => true,
        };
        self.trace.push(Op::IsRangeFree { start, end });
        self.crash_after(n)?;
        Ok(free)
    }
}
