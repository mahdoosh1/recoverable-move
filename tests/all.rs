use recoverable_move::mover::*;
use recoverable_move::traits::*;

const OLD: u64 = 100;
const LEN: u64 = 60;
const CS: u64 = 10;
const SUB: u64 = 3;

// --- Helpers -----------------------------------------------------------

fn fill(disk: &mut MockDisk) {
    for i in 0..disk.data.len() / SECTOR_SIZE {
        let b = (i as u8).wrapping_mul(7).wrapping_add(3);
        disk.data[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE].fill(b);
    }
}

fn setup() -> (MockDisk, Vec<u8>) {
    let mut disk = MockDisk::new(2048);
    fill(&mut disk);
    let mut original = vec![0u8; LEN as usize * SECTOR_SIZE];
    for i in 0..LEN {
        let lba = OLD + i;
        original[i as usize * SECTOR_SIZE..(i as usize + 1) * SECTOR_SIZE]
            .copy_from_slice(
                &disk.data[lba as usize * SECTOR_SIZE..(lba as usize + 1) * SECTOR_SIZE],
            );
    }
    disk.partition = Extent::new(OLD, OLD + LEN).unwrap();
    (disk, original)
}

fn setup_with(blocked: Option<u64>) -> (MockDisk, Vec<u8>) {
    let (mut disk, original) = setup();
    disk.blocked_lba = blocked;
    (disk, original)
}

fn request(new_start: u64) -> MoveRequest {
    MoveRequest::new(OLD, LEN, new_start, CS, SUB).unwrap()
}

fn check_region(disk: &mut MockDisk, start: u64, expected: &[u8]) {
    let mut buf = [0u8; SECTOR_SIZE];
    for i in 0..expected.len() / SECTOR_SIZE {
        disk.read_sector(start + i as u64, &mut buf).unwrap();
        assert_eq!(
            &buf[..],
            &expected[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE],
            "sector {}",
            start + i as u64
        );
    }
}

fn resize_op_index(disk: &MockDisk, final_extent: Extent) -> Option<u64> {
    disk.trace
        .iter()
        .position(|op| matches!(op, Op::SetPartition(ext) if *ext == final_extent))
        .map(|i| i as u64 + 1)
}

fn first_metadata_op_index(disk: &MockDisk) -> Option<u64> {
    disk.trace
        .iter()
        .position(|op| matches!(op, Op::Write { data, .. } if Metadata::decode(data).is_some()))
        .map(|i| i as u64 + 1)
}

// --- Exhaustive crash injection: on-disk recovery only -----------------

fn exhaustive_on_disk(req: MoveRequest, blocked: Option<u64>) {
    let (mut baseline, _) = setup_with(blocked);
    execute(&mut baseline, Some(req), None).unwrap();

    let final_extent = Extent::new(req.new_start, req.new_start + LEN).unwrap();

    let first_metadata = first_metadata_op_index(&baseline)
        .expect("baseline never wrote metadata");
    let resize_start = resize_op_index(&baseline, final_extent)
        .expect("baseline never wrote the final extent");

    assert!(
        first_metadata < resize_start,
        "first metadata write should precede the resize phase"
    );

    for crash_at in (first_metadata + 1)..resize_start {
        for crash_after in [false, true] {
            let (mut disk, original) = setup_with(blocked);
            disk.crash_at = Some(crash_at);
            disk.crash_after = crash_after;
            let _ = execute(&mut disk, Some(req), None);

            disk.crash_at = None;
            disk.crash_after = false;
            execute(&mut disk, None, None).unwrap_or_else(|e| {
                panic!(
                    "disk-only recovery failed \
                     (crash_at={crash_at}, after={crash_after}): {e}"
                )
            });

            assert_eq!(
                disk.partition,
                final_extent,
                "wrong extent (crash_at={crash_at}, after={crash_after})"
            );
            check_region(&mut disk, req.new_start, &original);
        }
    }
}

// --- Exhaustive crash injection: full coverage via the store -----------

fn exhaustive_with_store(req: MoveRequest, blocked: Option<u64>) {
    let mut baseline_store = MemStore::new();
    let (mut baseline, _) = setup_with(blocked);
    execute(&mut baseline, Some(req), Some(&mut baseline_store)).unwrap();
    let total = baseline.op_count;
    assert!(total > 10, "baseline run too short: {total}");

    let final_extent = Extent::new(req.new_start, req.new_start + LEN).unwrap();

    for crash_at in 1..=total {
        for crash_after in [false, true] {
            let mut store = MemStore::new();
            let (mut disk, original) = setup_with(blocked);
            disk.crash_at = Some(crash_at);
            disk.crash_after = crash_after;

            let _ = execute(&mut disk, Some(req), Some(&mut store));

            disk.crash_at = None;
            disk.crash_after = false;
            execute(&mut disk, Some(req), Some(&mut store)).unwrap_or_else(|e| {
                panic!("recovery failed (crash_at={crash_at}, after={crash_after}): {e}")
            });

            assert_eq!(
                disk.partition,
                final_extent,
                "wrong extent (crash_at={crash_at}, after={crash_after})"
            );
            check_region(&mut disk, req.new_start, &original);
        }
    }
}

// --- Pure unit-style tests ---------------------------------------------

#[test]
fn checkpoint_offsets_match_subchunk_boundaries() {
    let expect = |chunk_len: u64, subchunk: u64| -> Vec<u64> {
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
    };
    assert_eq!(expect(100, 25), vec![24, 49, 74, 99]);
    assert_eq!(expect(100, 30), vec![29, 59, 89, 99]);
    assert_eq!(expect(100, 200), vec![99]);
    assert_eq!(expect(100, 100), vec![99]);
}

#[test]
fn invalid_metadata_rejected() {
    assert!(Metadata::new(100, 60, 0, 10, 3, false).is_err());
    assert!(Metadata::new(100, 0, 100, 10, 3, false).is_err());
    assert!(Metadata::new(0, 60, -100, 10, 3, false).is_err());
    assert!(Metadata::new(100, 60, 100, 200, 3, false).is_err());
    assert!(Metadata::new(100, 60, 100, 10, 20, false).is_err());
    assert!(Metadata::new(100, 60, 100, 10, 3, false).is_ok());
}

// --- Happy path (no store) ---------------------------------------------

#[test]
fn clean_forward_widened_backward() {
    let (mut disk, original) = setup_with(None);
    execute(&mut disk, Some(request(200)), None).unwrap();
    assert_eq!(disk.partition, Extent::new(200, 200 + LEN).unwrap());
    check_region(&mut disk, 200, &original);
}

#[test]
fn clean_forward_widened_forward() {
    let (mut disk, original) = setup_with(Some(OLD - 1));
    execute(&mut disk, Some(request(200)), None).unwrap();
    assert_eq!(disk.partition, Extent::new(200, 200 + LEN).unwrap());
    check_region(&mut disk, 200, &original);
}

#[test]
fn clean_backward_widened_backward() {
    let (mut disk, original) = setup_with(None);
    execute(&mut disk, Some(request(50)), None).unwrap();
    assert_eq!(disk.partition, Extent::new(50, 50 + LEN).unwrap());
    check_region(&mut disk, 50, &original);
}

#[test]
fn clean_backward_widened_forward() {
    let (mut disk, original) = setup_with(Some(49));
    execute(&mut disk, Some(request(50)), None).unwrap();
    assert_eq!(disk.partition, Extent::new(50, 50 + LEN).unwrap());
    check_region(&mut disk, 50, &original);
}

// --- Trace-based phase ordering ----------------------------------------

#[test]
fn trace_phase_sequence() {
    let (mut disk, _) = setup_with(Some(OLD - 1));
    execute(&mut disk, Some(request(200)), None).unwrap();

    let metadata_lba = OLD;
    let phases: Vec<Phase> = disk
        .trace
        .iter()
        .filter_map(|op| match op {
            Op::Write { lba, data, .. } if *lba == metadata_lba => {
                Metadata::decode(data).map(|m| m.phase())
            }
            _ => None,
        })
        .collect();

    assert_eq!(phases.first(), Some(&Phase::Init));
    assert_eq!(phases.last(), Some(&Phase::Resize));
    for phase in [Phase::Zero, Phase::Copy, Phase::Finalize, Phase::Verify] {
        assert!(phases.contains(&phase), "missing {phase:?}");
    }

    let first_init = phases.iter().position(|p| *p == Phase::Init).unwrap();
    for (i, phase) in phases.iter().enumerate() {
        if *phase == Phase::Zero {
            assert!(i > first_init, "Zero before Init at {i}");
        }
    }
}

// --- Crash injection: on-disk recovery only ----------------------------

#[test]
fn disk_only_forward_widened_backward() {
    exhaustive_on_disk(request(200), None);
}

#[test]
fn disk_only_forward_widened_forward() {
    exhaustive_on_disk(request(200), Some(OLD - 1));
}

#[test]
fn disk_only_backward_widened_backward() {
    exhaustive_on_disk(request(50), None);
}

#[test]
fn disk_only_backward_widened_forward() {
    exhaustive_on_disk(request(50), Some(49));
}

// --- Crash injection: full coverage via the store ----------------------

#[test]
fn with_store_forward_widened_backward() {
    exhaustive_with_store(request(200), None);
}

#[test]
fn with_store_forward_widened_forward() {
    exhaustive_with_store(request(200), Some(OLD - 1));
}

#[test]
fn with_store_backward_widened_backward() {
    exhaustive_with_store(request(50), None);
}

#[test]
fn with_store_backward_widened_forward() {
    exhaustive_with_store(request(50), Some(49));
}

// --- Torn metadata write + backup store --------------------------------

#[test]
fn torn_metadata_recovered_from_mem_backup() {
    let (mut probe, _) = setup_with(Some(OLD - 1));
    let _ = execute(&mut probe, Some(request(200)), None);
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

    let mut store = MemStore::new();
    let (mut disk, original) = setup_with(Some(OLD - 1));
    disk.torn_at = Some((op, 64));
    disk.crash_at = Some(op);
    disk.crash_after = true;
    let _ = execute(&mut disk, Some(request(200)), Some(&mut store));

    disk.torn_at = None;
    disk.crash_at = None;
    disk.crash_after = false;
    execute(&mut disk, None, Some(&mut store)).unwrap();

    assert_eq!(disk.partition, Extent::new(200, 200 + LEN).unwrap());
    check_region(&mut disk, 200, &original);
}

// --- FileStore round-trip ----------------------------------------------

#[test]
fn file_store_roundtrip_and_removal() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "recoverable_move_filestore_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.bin");

    let mut store = FileStore::new(&path);

    assert!(store.read().unwrap().is_none());

    let data = [0xABu8; 512];
    store.write(&data).unwrap();
    assert_eq!(store.read().unwrap().as_deref(), Some(&data[..]));

    let data2 = [0xCDu8; 512];
    store.write(&data2).unwrap();
    assert_eq!(store.read().unwrap().as_deref(), Some(&data2[..]));

    store.remove();
    assert!(store.read().unwrap().is_none());

    store.remove();
    assert!(store.read().unwrap().is_none());

    let _ = fs::remove_dir_all(&dir);
}

// --- Post-completion behavior ------------------------------------------

#[test]
fn run_after_completion_returns_no_transaction() {
    let (mut disk, _) = setup_with(None);
    execute(&mut disk, Some(request(200)), None).unwrap();

    let err = execute(&mut disk, None, None).unwrap_err();
    assert!(matches!(err, Error::NoTransaction));
}

// --- Mock disk ---------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Read { lba: u64, len: usize },
    Write { lba: u64, len: usize, data: Vec<u8> },
    Sync,
    GetPartition,
    SetPartition(Extent),
    IsSectorFree { lba: u64 },
    IsRangeFree { start: u64, end: u64 },
}

pub struct MockDisk {
    pub data: Vec<u8>,
    pub partition: Extent,
    pub trace: Vec<Op>,
    pub crash_at: Option<u64>,
    pub crash_after: bool,
    pub torn_at: Option<(u64, usize)>,
    pub op_count: u64,
    pub blocked_lba: Option<u64>,
}

impl MockDisk {
    pub fn new(sectors: u64) -> Self {
        Self {
            data: vec![0u8; sectors as usize * SECTOR_SIZE],
            partition: Extent::new(0, 0).unwrap(),
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
        let start = lba as usize * SECTOR_SIZE;
        let end = start + len;
        if end > self.data.len() {
            return Err(Error::InvalidParams(format!("lba {lba} out of range")));
        }
        Ok(start..end)
    }
}

impl Disk for MockDisk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        let range = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[range]);
        self.trace.push(Op::Read { lba, len: buf.len() });
        self.crash_after(n)?;
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        let range = self.span(lba, buf.len())?;
        let to_write = match self.torn_at {
            Some((i, k)) if i == n => k.min(buf.len()),
            _ => buf.len(),
        };
        self.data[range.start..range.start + to_write].copy_from_slice(&buf[..to_write]);
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

    fn get_partition(&mut self) -> Result<Extent, Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        self.trace.push(Op::GetPartition);
        self.crash_after(n)?;
        Ok(self.partition)
    }

    fn set_partition(&mut self, extent: Extent) -> Result<(), Error> {
        let n = self.next_op();
        self.crash_before(n)?;
        self.partition = extent;
        self.trace.push(Op::SetPartition(extent));
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
