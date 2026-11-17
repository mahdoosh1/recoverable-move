# recoverable-move

A Rust library and interactive command-line utility for crash-recoverable relocation of GPT partitions.

`recoverable-move` moves a partition to a different starting LBA while preserving its contents. It uses chunked copying, checkpoint sentinels, and an on-disk recovery record to resume interrupted operations.

**Warning:** This software directly modifies disk sectors and GPT partition metadata. Incorrect use, hardware failure, or unexpected interruption can result in data loss. Back up important data before using it.

## Features

- Moves GPT partitions in either direction.
- Supports overlapping source and destination extents.
- Copies data in configurable chunks and subchunks.
- Uses checkpoint sentinels to detect partially completed writes.
- Stores two copies of the recovery record on disk.
- Supports an optional file-based metadata mirror.
- Recovers interrupted operations using persisted progress.
- Uses CRC-32 checksums to validate recovery records.

## Requirements

- Rust 1.98 or later.
- Linux or another Unix platform supported by `std::os::unix::fs::FileExt`.
- A GPT-formatted disk image or block device.
- Read and write permissions for the target.

## Building

```bash
git clone https://github.com/mahdoosh1/recoverable-move.git
cd recoverable-move

cargo build --release
```

Run the tests:

```bash
cargo test
```

Run the program:

```bash
sudo ./target/release/recoverable-move /dev/sdX
```

You can also use a disk image:

```bash
./target/release/recoverable-move disk.img
```

If no path is supplied, the program prompts for one.

## Command-line usage

The program provides an interactive command interface.

| Command | Description |
|---|---|
| `help` | Display available commands |
| `list` | List GPT partitions |
| `move` | Move a partition |
| `recover` | Recover an interrupted move |
| `exit` | Exit the program |
| `quit` | Exit the program |

### Listing partitions

Use:

```text
> list
```

The partition table displays:

- Index
- Partition type GUID
- Starting LBA
- Ending LBA
- Size in sectors
- Partition label

Partition indices are one-based in the CLI.

### Moving a partition

Use:

```text
> move
```

The program asks for:

1. Partition index.
2. New starting LBA.
3. Chunk size in sectors.
4. Subchunk size in sectors.
5. Optional metadata mirror path.
6. Confirmation.

The default chunk size is the absolute displacement between the old and new starting LBAs.

The following constraints apply:

```text
1 <= chunk_size <= abs(displacement)
1 <= subchunk_size <= chunk_size
```

The number of chunks is:

```text
ceil(partition_length / chunk_size)
```

The program displays the source and destination extents and asks for confirmation before modifying the disk.

### Recovering an interrupted operation

Use:

```text
> recover
```

Recovery can use either:

- The recovery record stored on the disk.
- A metadata mirror file, if one was provided.

The program locates and validates the recovery record, determines the partition index, and resumes the operation from its recorded phase.

Recovery requires confirmation before writing to the disk.

## Library API

The library exposes the following modules:

```rust
pub mod consts;
pub mod errors;
pub mod gptman_interface;
pub mod mover;
pub mod record;
pub mod traits;
```

### Disk and Partition

The `Disk` trait abstracts sector-level disk operations and partition-table management.

```rust
pub trait Disk {
    fn sector_size(&self) -> usize;

    fn read_sector(
        &mut self,
        lba: u64,
        buf: &mut [u8],
    ) -> Result<(), Error>;

    fn write_sector(
        &mut self,
        lba: u64,
        buf: &[u8],
    ) -> Result<(), Error>;

    fn sync(&mut self) -> Result<(), Error>;

    fn list_partitions(&mut self) -> Result<Vec<Partition>, Error>;

    fn update_partition(
        &mut self,
        index: usize,
        partition: Partition,
    ) -> Result<(), Error>;

    fn is_range_free(
        &mut self,
        start_lba: u64,
        end_lba: u64,
    ) -> Result<bool, Error>;
}
```

A custom disk backend can implement this trait.

`Partition` represents a partition using:

- Type GUID.
- Starting LBA.
- Length in sectors.
- 72-byte UTF-16LE partition label.

Its constructor is:

```rust
Partition::new(
    start: u64,
    len: u64,
    guid: &[u8; 16],
    partition_label: &[u8; 72],
)
```

`Partition::end()` returns `start + len`, the exclusive end LBA.

### GptDisk

`GptDisk` implements `Disk` using the `gptman` crate.

Open a disk image or device:

```rust
use recoverable_move::gptman_interface::GptDisk;

let mut disk = GptDisk::open("/dev/sdX")?;
```

Alternatively:

```rust
use recoverable_move::gptman_interface::create_disk;

let mut disk = create_disk("disk.img")?;
```

### Record

`Record` contains the parameters and progress of a move.

Create a new record:

```rust
use recoverable_move::record::Record;

let record = Record::new(
    sector_size,
    partition,
    displacement,
    chunk_size,
    subchunk_size,
)?;
```

The record tracks:

- Original partition information.
- Displacement.
- Chunk and subchunk sizes.
- Current phase.
- Number of completed chunks.
- Recovery-record location.

Useful methods include:

```rust
record.encode()?;
Record::decode(&buffer)?;
record.verify()?;
record.chunks_total();
record.partition_after();
```

`encode()` serializes a record into a sector-sized byte buffer. `decode()` validates and reconstructs a record.

### Moving and recovering

The main operations are exposed through `mover`.

```rust
use recoverable_move::mover::{continue_, recover_record};
```

Start a move:

```rust
continue_(
    &mut disk,
    partition_index,
    record,
    optional_file_writer,
)?;
```

Recover an interrupted move:

```rust
let (partition_index, record) = recover_record(
    &mut disk,
    optional_mirror_record,
)?;

continue_(
    &mut disk,
    partition_index,
    record,
    optional_file_writer,
)?;
```

`partition_index` is zero-based in the library API.

The optional file writer is a `FileWriter` from `traits`. The optional mirror record passed to `recover_record()` is a decoded `Record`.

### FileWriter

`FileWriter` provides file-based storage for encoded recovery records.

```rust
use recoverable_move::traits::FileWriter;

let mut writer = FileWriter::new("metadata.bin")?;
```

It provides:

```rust
writer.read(&mut buffer)?;
writer.write(&buffer)?;
```

The file is opened for reading and writing and is created if it does not exist.

## Recovery mechanism

The mover uses a state machine. Each phase is persisted before proceeding to the next phase.

| Phase | Purpose |
|---|---|
| `Widen` | Reserve space for the expanded partition and recovery record |
| `Zero` | Zero destination checkpoint sectors |
| `Copy` | Copy chunk data while writing checkpoint sentinels |
| `Finalize` | Replace destination sentinels with original source data |
| `NextChunk` | Mark the current chunk complete |
| `Resize` | Restore the final partition extent and clear recovery records |

### Recovery record

The recovery record has:

- Magic identifier: `MOVEPART`.
- Format version: `1`.
- Sector-size information.
- Move parameters and progress.
- CRC-32 checksum.

The checksum uses CRC-32/ISO-HDLC.

Two copies are written to adjacent sectors. The record location is selected from available space near the expanded partition.

### Checkpoint sentinels

During copying, checkpoint sectors are temporarily written with a known sentinel pattern.

When recovering a chunk, the mover examines these sectors to determine how much copying was completed. This allows it to resume without blindly assuming that an interrupted write completed successfully.

### Widening

Before moving data, the original partition is temporarily widened to cover the source and destination extents. Its partition type GUID is replaced with a marker GUID.

This makes the operation discoverable during recovery.

A power failure during the initial `Widen` phase can occur before the partition-table update. In that case, the original partition and its data remain intact, but the operation may need to be started again rather than recovered.

### Finalization

After all chunks are completed, the partition is updated to its final extent and original type GUID. The two recovery-record sectors are then cleared.

## Testing

The crash-recovery tests are in `tests/crash.rs`.

Run them separately:

```bash
cargo test --test crash
```

The tests cover:

- Moves in both directions.
- Recovery after interruptions at operation boundaries.
- Torn sector writes.
- Alternative recovery-record placement.
- Invalid recovery records.
- Data preservation.

Run the complete test suite:

```bash
cargo test
```

## Dependencies

- [gptman](https://crates.io/crates/gptman) — GPT partition-table handling.
- [crc](https://crates.io/crates/crc) — CRC checksums.
- [uuid](https://crates.io/crates/uuid) — UUID handling.

## License

GNU General Public License v2.0 or, at your option, any later version.

See [LICENSE](LICENSE) for details.

Copyright (C) 2026 mahdoosh1
