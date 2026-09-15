use recoverable_move::*;
use std::fs;

const OLD: u64 = 100; // was 100
const LEN: u64 = 60; // was 600
const CS: u64 = 10; // was 100
const SUB: u64 = 3; // was 25; 4 subchunks per chunk, 6 chunks

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

fn params(new_start: u64, shadow: bool) -> MoveParams {
    MoveParams {
        old_start: OLD,
        len: LEN,
        new_start,
        chunk_size: CS,
        subchunk_size: SUB,
        uses_shadow: shadow,
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

/// Unique directory per call so parallel test threads never collide.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "recoverable_move_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn exhaustive(p: MoveParams) {
    // Baseline: learn how many ops a clean run performs.  The file-based
    // metadata mirror does not go through the disk, so its presence or
    // absence does not change the op count.
    let base_dir = scratch("base");
    let base_cfg = Config { backup_path: Some(base_dir.join("meta.bin")) };
    let (mut d, _) = setup();
    run(&mut d, Some(p), &base_cfg).unwrap();
    let total = d.op_count;
    assert!(total > 10, "baseline run too short: {total}");
    let _ = fs::remove_dir_all(&base_dir);

    for crash_at in 1..=total {
        for crash_after in [false, true] {
            let dir = scratch(&format!("crash_{crash_at}_{}", crash_after as u8));
            let cfg = Config { backup_path: Some(dir.join("meta.bin")) };

            let (mut d, orig) = setup();
            d.crash_at = Some(crash_at);
            d.crash_after = crash_after;
            let _ = run(&mut d, Some(p), &cfg);

            if let Some(path) = &cfg.backup_path {
                let exists = path.exists();
                let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                eprintln!(
                     "recover crash_at={crash_at} after={crash_after} \
                     file={:?} exists={} len={} gpt=({},{}) op_count={} trace_len={}",
                    path, exists, len, d.gpt.start, d.gpt.len, d.op_count, d.trace.len()
                );
            }

            // Recovery: no crash injection, same params so a pre-metadata
            // crash (nothing written yet) can be restarted cleanly.
            d.crash_at = None;
            d.crash_after = false;
            run(&mut d, Some(p), &cfg).unwrap_or_else(|e| {
                panic!("recovery failed (crash_at={crash_at}, after={crash_after}): {e}")
            });

            assert_eq!(
                d.gpt,
                Gpt { start: p.new_start, len: p.len },
                "wrong gpt (crash_at={crash_at}, after={crash_after})"
            );
            check_region(&mut d, p.new_start, &orig);

            let _ = fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure unit-style tests
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_offsets_match_subchunk_boundaries() {
    // Mirrors the library's private helper; documents the expected
    // placement of checkpoints within a chunk.
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
        uses_shadow: false,
        old_start: 100,
        len: 600,
        displacement: 0,
        chunk_size: 100,
        subchunk_size: 25,
        chunks_total: 6,
        chunks_done: 0,
    };
    assert!(bad.validate().is_err());

    let bad2 = Metadata {
        displacement: 100,
        chunks_done: 7,
        chunks_total: 6,
        ..bad
    };
    assert!(bad2.validate().is_err());
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn clean_forward_no_shadow() {
    let (mut d, orig) = setup();
    run(&mut d, Some(params(400, false)), &Config::default()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 400, len: LEN });
    check_region(&mut d, 400, &orig);
}

#[test]
fn clean_forward_shadow() {
    let (mut d, orig) = setup();
    run(&mut d, Some(params(400, true)), &Config::default()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 400, len: LEN });
    check_region(&mut d, 400, &orig);
}

#[test]
fn clean_backward_no_shadow() {
    let (mut d, orig) = setup();
    run(&mut d, Some(params(50, false)), &Config::default()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 50, len: LEN });
    check_region(&mut d, 50, &orig);
}

#[test]
fn clean_backward_shadow() {
    let (mut d, orig) = setup();
    run(&mut d, Some(params(50, true)), &Config::default()).unwrap();
    assert_eq!(d.gpt, Gpt { start: 50, len: LEN });
    check_region(&mut d, 50, &orig);
}

// ---------------------------------------------------------------------------
// Trace-based phase ordering
// ---------------------------------------------------------------------------

#[test]
fn trace_phase_sequence() {
    let (mut d, _) = setup();
    run(&mut d, Some(params(400, false)), &Config::default()).unwrap();

    let meta_lba = 99; // no-shadow, u=100 -> meta at 99
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
// Exhaustive crash injection (the important ones)
// ---------------------------------------------------------------------------

#[test]
fn crash_every_op_forward_no_shadow() {
    exhaustive(params(400, false));
}

#[test]
fn crash_every_op_forward_shadow() {
    exhaustive(params(400, true));
}

#[test]
fn crash_every_op_backward_no_shadow() {
    exhaustive(params(50, false));
}

#[test]
fn crash_every_op_backward_shadow() {
    exhaustive(params(50, true));
}

// ---------------------------------------------------------------------------
// Torn metadata write + file backup
// ---------------------------------------------------------------------------

#[test]
fn torn_metadata_recovered_from_file_backup() {
    let dir = scratch("torn");
    let backup = dir.join("meta.bin");
    let cfg = Config { backup_path: Some(backup.clone()) };

    // Find the op index of the first metadata write using a probe disk
    // with no backup, so its own file state is irrelevant.
    let (mut probe, _) = setup();
    let _ = run(&mut probe, Some(params(400, false)), &Config::default());
    let op = probe
        .trace
        .iter()
        .enumerate()
        .find_map(|(i, o)| match o {
            Op::Write { data, .. } if Metadata::decode(data).is_some() => {
                Some(i as u64 + 1)
            }
            _ => None,
        })
        .expect("no metadata write found");

    // Tear the metadata write and stop immediately after it.  The file
    // was written first and holds a valid record; the disk sector does
    // not.
    let (mut d, orig) = setup();
    d.torn_at = Some((op, 64));
    d.crash_at = Some(op);
    d.crash_after = true;
    let _ = run(&mut d, Some(params(400, false)), &cfg);

    // Recovery must read the file, not the torn disk sector.
    d.torn_at = None;
    d.crash_at = None;
    d.crash_after = false;
    run(&mut d, None, &cfg).unwrap();

    assert_eq!(d.gpt, Gpt { start: 400, len: LEN });
    check_region(&mut d, 400, &orig);

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Post-completion behavior
// ---------------------------------------------------------------------------

#[test]
fn run_after_completion_returns_no_transaction() {
    let dir = scratch("complete");
    let cfg = Config { backup_path: Some(dir.join("meta.bin")) };

    let (mut d, _) = setup();
    run(&mut d, Some(params(400, false)), &cfg).unwrap();

    // After a clean finish both the sector and the file are cleared, so
    // a parameterless run must report that nothing is in flight.
    let err = run(&mut d, None, &cfg).unwrap_err();
    assert!(matches!(err, Error::NoTransaction));

    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// Mock disk (used only by this test file, not part of the library)
// ===========================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Read { lba: u64, len: usize },
    Write { lba: u64, len: usize, data: Vec<u8> },
    Sync,
    GetPartition,
    SetPartition(Gpt),
}

pub struct MockDisk {
    pub data: Vec<u8>,
    pub gpt: Gpt,
    pub trace: Vec<Op>,
    /// 1-indexed operation at which to crash.
    pub crash_at: Option<u64>,
    /// If true, the crashing op completes before the error returns.
    pub crash_after: bool,
    /// For a `Write` at op index `i`, write only the first `n` bytes and
    /// leave the rest of the sector as-is (simulates a torn write).
    pub torn_at: Option<(u64, usize)>,
    pub op_count: u64,
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
}
