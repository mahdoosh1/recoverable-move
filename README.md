# recoverable-move

Crash-recoverable relocation of an overlapping GPT partition. The library is
written in Rust; the binary provides an interactive GPT command-line interface.

> [!WARNING]
> **Destructive storage operations**
>
> This program writes raw sectors and GPT metadata. Back up the disk,
> unmount the target partition, ensure nothing is accessing it, and
> test using a disposable disk image before using a real device.

> [!WARNING]
> **NOTE**
>
> THIS PROJECT IS VIBE CODED BY DEEPSEEK AND CHATGPT AND, ME (mahdoosh1),
> THE AUTHOR HAVE READ IT AND VERIFIED IT.

## Build and test

```sh
cargo test
cargo run -- path/to/disk.img
```

Run the binary as root/with suitable permissions when accessing a block device.
If no path is supplied, the CLI prompts for one.

## CLI commands

- `list`: list used GPT entries and disk geometry.
- `verify`: recompute GPT header and partition-entry-array CRC32 checksums.
- `move`: interactively move a selected partition.
- `recover`: resume a previously interrupted move.
- `help`, `quit`: show help or exit.

The selected partition number is GPT's one-based entry number. The CLI uses full
sector verification for newly initiated moves. A metadata mirror file is optional;
the primary recovery record is stored in two sectors adjacent to the moved extent.

## Library API

The main entry point is `recoverable_move::execute`. Supply a mutable `Disk`, an
optional `RecordStore`, and `Some(MoveRequest)` to start a move. Supply `None` as
the request to recover an interrupted move. `GptDisk` is the provided gptman-backed
implementation of `Disk`; custom storage backends can implement the same trait.

All addresses and lengths are in sectors. A move must overlap its source
(`0 < abs(new_start - old_start) < len`). Non-overlapping copies are deliberately
out of scope. `chunk_size` is capped at the absolute displacement;
`subchunk_size` is capped at the chunk size.

### Recovery model

The mover temporarily marks and widens the GPT partition to include both source
and destination extents. It writes two copies of a versioned CRC-32 record in
reserved sectors outside the data union. Chunks are copied in an order that
prevents overwriting uncopied source data. Checkpoint sentinels allow a partially
completed chunk to resume. Each journal phase is persisted after its work is
synced, making recovery repeatable. On completion the GPT extent and original
type GUID are restored and the journal sectors are cleared.

The optional `RecordStore` is a secondary copy used if both on-disk record copies
are unreadable. `FileStore` stores it in a synced file; `MemStore` is intended for
tests and does not survive process termination.

## Source layout

- `geometry.rs`: checked source/target/union extent calculations.
- `record.rs`: journal format, phase tags, and CRC-32 validation.
- `traits.rs`: storage interfaces and record-store implementations.
- `mover.rs`: crash-recoverable copy state machine.
- `gpt_disk.rs`: gptman-backed file/block-device implementation.
- `main.rs`: interactive CLI.
- `tests/crash.rs`: crash, torn-write, and corruption fault-injection tests.

API documentation and examples can be generated with `cargo doc --open`.

## License

recoverable-move is licensed under the
GNU General Public License, version 2 or (at your option) any later version.

See [LICENSE](LICENSE) for the complete license text.

Copyright (C) 2026 mahdoosh1
