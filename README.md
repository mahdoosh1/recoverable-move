# recoverable-move

Move a GPT partition when the old and new locations overlap, safely,
across power loss.

The library treats partition contents as opaque. It does not know or care
what filesystem lives inside. It only needs a `Disk` that can read and
write sectors and read/update a partition extent.

NOTE: originally vibe-coded; since refactored for readability, robustness,
and accurate docs.

## The problem

A move from `[A, A+L)` to `[A+X, A+X+L)` is trivial when the extents are
disjoint. When they overlap, a naïve sector-by-sector copy overwrites
source sectors it hasn't read yet. `X` is the displacement.

The classic fix is a memmove: chunks no larger than `|X|`, copied
highest-first when `X > 0` and lowest-first when `X < 0`, so a chunk's
destination never lands on another chunk's not-yet-read source. That's
correct for a single uninterrupted pass. It is not crash-safe.

## Crash safety

A crash at *any* instruction is recoverable, because:

- **A metadata sector** at the first sector of the enlarged extent
  records old LBA, new LBA, direction, chunk size, and progress. It
  carries a checksum, so a torn write is detected and the record is
  discarded.

- **A `MetaStore` mirror** is written *before* the sector. By the end
  of the move the metadata sector has been overwritten by moved data, so
  on-disk recovery alone cannot find the final resize. The mirror is how
  recovery survives that phase. The store is also written before the
  initial widen, so a crash there resumes cleanly.

- **Checkpoints** in the destination record in-chunk progress. Each
  subchunk ends in a one-sector checkpoint that is (a) zeroed before the
  chunk copy begins, (b) stamped with a sentinel when the copy reaches
  it, and (c) restored to real data after the chunk is fully copied.
  Recovery walks the checkpoints, finds the last sentinel, and resumes
  immediately after it.

## Layout

Union of old and new extents:

```
U = min(A, A+X)
E = max(A+L, A+X+L)
```

The enlarged partition is either `[U-1, E)` — metadata in free space
behind the union — or `[U, E+1)` — metadata at `U`, with the sector at
`E` doubling as a **shadow**. The mover picks between the two at start:
it widens backward when `U-1` is free, otherwise forward, failing if
neither sector is available.

The shadow's role depends on direction:

| Direction | Shadow holds            | Read/write rule                         | Resize             |
|-----------|-------------------------|-----------------------------------------|--------------------|
| rightward | original data of `U`    | chunk src = `U` reads from `E`          | leaves `E` alone   |
| leftward  | destination of `U`      | chunk dst = `U` writes to `E`           | copies `E → U`     |

In the leftward case the resize copy destroys the metadata sector, which
is exactly what we want.

## Phases

Discriminants are spaced by 4 so intermediate states can be inserted
later.

| Value | Phase      | Meaning                                             |
|-------|------------|-----------------------------------------------------|
| 4     | `Init`     | Metadata written; ready to begin.                   |
| 8     | `Zero`     | Zero the active chunk's checkpoint sectors.         |
| 12    | `Copy`     | Copy the active chunk, stamping sentinels.          |
| 16    | `Finalize` | Restore the sentinel sectors to real data.          |
| 20    | `Verify`   | Confirm no sentinels remain.                        |
| 24    | `Resize`   | Install final GPT extent; clean up the store.       |

## Checkpoints

One checkpoint per subchunk, at the last sector of the subchunk. From the
original design notes, `?` = original data, `Z` = zeroed, `S` = sentinel,
`D` = real data:

```
Initial:    D D ? Z ? ? ? Z
Progress:   D D D S D ? ? Z
Later:      D D D S D D D S
Final:      D D D D D D D D
```

The additional write cost per checkpoint is 2 writes (zero it, then
stamp it) against one for a plain copy. With one checkpoint every `N`
sectors the overhead is approximately:

```
2 / N
```

Example sizing, not a default: 32 GiB chunks, 128 MiB subchunks, 256
subchunks per chunk. That's `N = 262144`, so overhead ≈ 0.0008%.

## Usage

```rust
use recoverable_move::{execute, traits::{Disk, FileStore, MetaStore}, MoveRequest};

let mut disk: MyDisk = MyDisk::open("/dev/sdX")?;
let mut store = FileStore::new("/var/lib/movepart/meta.bin");

let request = MoveRequest::new(
    2048,      // old_start
    1_048_576, // len
    1_000_000, // new_start
    67_108_864, // chunk sectors (≤ |displacement|)
    262_144,    // subchunk sectors
)?;

execute(&mut disk, Some(request), Some(&mut store))?;
```

To resume an interrupted move, pass `None` for the request:

```rust
use recoverable_move::traits::MetaStore;
let store_ref: Option<&mut dyn MetaStore> = Some(&mut store);
execute(&mut disk, None, store_ref)?;
```

If metadata is present it resumes; if not, `Error::NoTransaction` is
returned. Passing `Some(request)` after a partial move is ignored — the
stored metadata wins. The store is optional: with `None` the on-disk
metadata sector is the only record, which is enough except for a crash
inside the resize phase.

### Interactive CLI

The crate ships a `recoverable-move` binary for working on disk images:

```sh
cargo run -- /path/to/image.img
```

Commands: `list`, `verify` (GPT CRCs), `move`, `recover`, and `help`.
Release builds omit the unsafe `set` command, which rewrites a partition
entry without moving data.

### Implementing `Disk`

```rust
pub trait Disk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;
    fn get_partition(&mut self) -> Result<Extent, Error>;
    fn set_partition(&mut self, extent: Extent) -> Result<(), Error>;
    fn is_sector_free(&mut self, lba: u64) -> Result<bool, Error>;
    fn is_range_free(&mut self, start: u64, end: u64) -> Result<bool, Error>;
}
```

`get_partition` / `set_partition` operate on the moved partition only.
Free-space queries are used to choose where the metadata sector goes and
to reject a target that overlaps another partition. `FileDisk` in
[`src/disk.rs`](src/disk.rs) implements all of this on top of a file or
block device, keeping the primary and backup GPT in sync via
[`src/gpt.rs`](src/gpt.rs).

## MetaStore

The mirror strengthens recovery; it is not strictly required for most
phases. Two implementations ship:

- **`FileStore`** — durable, uses `fsync`. Production.
- **`MemStore`** — in-memory. Tests.

Without a store, the mover still recovers from the on-disk metadata
sector across every phase except a crash inside `Resize`, where the
metadata sector has been overwritten. A crash inside `Resize` without a
working store is unrecoverable by design. With a store, `start` writes
the record before widening the partition, so even a crash in that
window resumes cleanly.

## Invariants

- Chunk size ≤ `|X|`. This is what guarantees a chunk's destination
  never overlaps another chunk's not-yet-copied source.
- Rightward moves process chunks highest-index-first; leftward moves
  lowest-first.
- The active chunk's source remains intact until its destination is
  complete.
- Completed chunks are never recopied.
- Final geometry is installed only after all data has moved.
- Crashes may occur at any point inside any phase. Phases are milestones,
  not atomic steps.

## Limitations

- Offline only. The disk must not be mounted or otherwise written while
  a move is in flight.
- The caller must know where the `MetaStore` lives on recovery. The
  library does not scan the disk for it.
- `is_sentinel` treats any all-`0xA5` sector at a checkpoint position as
  a sentinel. A payload that happens to match costs one redundant
  rewrite, not correctness.
- The resize-to-cleanup window inside `resize` is only recoverable via
  the `MetaStore`. See above.
- A crash before the partition is widened is recoverable only with a
  `MetaStore`; without one there is no on-disk record yet.

## Testing

```sh
cargo test
cargo test --release
```

The suite includes an exhaustive crash-injection harness: for every
write the algorithm performs, in both directions and both shadow modes,
it interrupts the run (before and after the write), then runs recovery
and checks byte-for-byte that the moved data matches the original.

Crash tests use `MemStore` and a mock `Disk`, so they touch no
filesystem and run in under a second.