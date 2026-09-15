# recoverable-move

Move a GPT partition when the old and new locations overlap, safely,
across power loss.

The library treats partition contents as opaque. It does not know or care
what filesystem lives inside. It only needs a `Disk` that can read and
write sectors and read/update a partition extent.

NOTE: Vibe-coded with deepseek, i will debloat it sometime

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

- **A `MetaStore` mirror** is written *before* the sector. This is
  load-bearing: by the end of the move the metadata sector has been
  overwritten by moved data, so on-disk recovery alone cannot find the
  final resize. The mirror is how recovery survives that phase.

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
`E` doubling as a **shadow**. `uses_shadow` in `MoveParams` picks between
the two; the caller decides based on whether `U-1` is free.

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
use recoverable_move::{
    run, Disk, Error, FileStore, Gpt, MoveParams, SECTOR_SIZE,
};
use std::path::PathBuf;

let mut disk = MyDisk::open("/dev/sdX")?;
let mut store = FileStore::new(PathBuf::from("/var/lib/movepart/meta.bin"));

let params = MoveParams {
    old_start: 2048,
    len: 1_048_576,
    new_start: 1_000_000,
    chunk_size: 32 * 1024 * 1024 * 1024 / SECTOR_SIZE as u64,
    subchunk_size: 128 * 1024 * 1024 / SECTOR_SIZE as u64,
    uses_shadow: false,
};

run(&mut disk, Some(params), &mut store)?;
```

To resume an interrupted move, call with `params = None`:

```rust
run(&mut disk, None, &mut store)?;
```

If metadata is present it resumes; if not, `Error::NoTransaction` is
returned. Passing `Some(params)` after a partial move is ignored — the
stored metadata wins.

### Implementing `Disk`

```rust
pub trait Disk {
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<(), Error>;
    fn sync(&mut self) -> Result<(), Error>;
    fn get_partition(&mut self) -> Result<Gpt, Error>;
    fn set_partition(&mut self, gpt: Gpt) -> Result<(), Error>;
}
```

`get_partition` / `set_partition` operate on the moved partition only.
The caller is responsible for whatever the platform requires to keep the
primary and backup GPT consistent; the library treats the extent as one
logical object.

## MetaStore

The metadata mirror is part of the protocol, not an optional aid. Three
implementations ship:

- **`FileStore`** — durable, uses `fsync`. Production.
- **`MemStore`** — in-memory. Tests.
- **`NullStore`** — discards writes. Only if the resize phase is known
  not to be interruptible (e.g. battery-backed write cache).

A crash inside `Resize` without a working store is unrecoverable by
design.

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
- The resize-to-cleanup window inside `do_resize` is only recoverable
  via the `MetaStore`. See above.

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