//! Crash-recoverable relocation of an overlapping GPT partition.
//!
//! A partition is moved from one LBA range to another. When the ranges
//! overlap, a naive copy overwrites source sectors before they are read.
//! This crate relocates the data safely and survives power loss at any
//! point.
//!
//! # Layout
//!
//! Let the source be `[A, A + L)` and the target `[A + X, A + X + L)`.
//! During the move the partition occupies an *extended range* — the union
//! of the two, plus one sector for metadata:
//!
//! ```text
//! union_lower = min(A, A + X)
//! union_upper = max(A + L, A + X + L)
//!
//! widen_forward = false:  [union_lower - 1, union_upper)
//! widen_forward = true:   [union_lower, union_upper + 1)
//! ```
//!
//! The metadata sector sits at `partition_start`. When the range was
//! widened forward, `partition_start == union_lower` is also the first
//! data sector of the union, so its contents are parked at the *shadow*
//! (`partition_end - 1`) for the duration of the move. When widened
//! backward, `partition_start` is a fresh sector and no shadow is used.
//!
//! # Entry points
//!
//! - [`mover::execute`] — start a new move or resume an interrupted one.
//!   With a [`traits::MetaStore`] the record is persisted before the
//!   partition is widened, so every crash point is resumable.
//! - [`mover::MoveRequest`] — parameters for a new move.
//! - [`traits::Disk`] — raw sector I/O plus partition-table access.
//! - [`traits::MetaStore`] — durable out-of-band record of move progress.
//! - [`disk::FileDisk`] — a [`traits::Disk`] backed by a file or block device.

pub mod disk;
pub mod gpt;
pub mod mover;
pub mod traits;

pub use mover::{execute, Extent, MoveRequest};
pub use traits::{Disk, MetaStore};
