//! Copyright (C) 2026 mahdoosh1
//!
//! This file is part of recoverable-move.
//!
//! recoverable-move is free software: you can redistribute it and/or modify
//! it under the terms of the GNU General Public License as published by
//! the Free Software Foundation, either version 2 of the License, or
//! (at your option) any later version.
//!
//! recoverable-move is distributed in the hope that it will be useful,
//! but WITHOUT ANY WARRANTY; without even the implied warranty of
//! MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
//! See the GNU General Public License for more details.
//!
//! You should have received a copy of the GNU General Public License
//! along with recoverable-move. If not, see <https://www.gnu.org/licenses/>.

//! Crash-recoverable relocation of an overlapping GPT partition.
//!
//! A partition occupies `[A, A + L)` and is moved to `[A + X, A + X + L)` with
//! `0 < |X| < L`. Data is copied in chunks of at most `|X|` sectors, so a
//! chunk's destination never overlaps its own source, and chunks are ordered
//! so a chunk's destination never overwrites a source that has not been copied
//! yet.
//!
//! While a move is in flight the partition is widened to cover the union of the
//! two extents plus two record sectors placed immediately outside the union.
//! The two record sectors are written through a mirror and never overlap copied
//! data, so recovery needs nothing but the disk and finds the move by looking at
//! the partition's type GUID.
//!
//! Recovery redoes the step named by the record. Every step is idempotent, and a
//! step that would make an earlier step unsafe is only entered after the record
//! says the earlier step is done.
//!
//! # Safety
//!
//! This crate writes directly to the selected disk and its GPT. Back up important
//! data, unmount the partition, ensure no process is using it, and test against a
//! disposable disk image before using a physical device. The implementation assumes
//! the `Disk` implementation honours sector writes and durability barriers.
//!
//! # Basic use
//!
//! Implement [`Disk`] for the storage device, then pass a [`MoveRequest`] to
//! [`execute`]. For a file or block device, use [`GptDisk`].
//!
//! ```no_run
//! use recoverable_move::{execute, GptDisk, MoveRequest, Verify};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut disk = GptDisk::open("disk.img", 1)?; // GPT entries are one-based.
//! let request = MoveRequest {
//!     old_start: 2048,
//!     new_start: 3072,
//!     len: 8192,
//!     chunk_size: 1024,
//!     subchunk_size: 128,
//!     verify: Verify::Full,
//! };
//! execute(&mut disk, None, Some(request))?;
//! # Ok(())
//! # }
//! ```

use std::fmt;

pub mod geometry;
pub mod gpt_disk;
pub mod mover;
pub mod record;
pub mod traits;

pub use geometry::{Geometry, Placement};
pub use gpt_disk::GptDisk;
pub use mover::{MoveRequest, Verify, execute};
pub use record::{Phase, Record};
pub use traits::{Disk, Partition, RecordStore};

/// Partition type GUID temporarily assigned to a partition while a move is active.
///
/// The marker lets [`execute`] distinguish an interrupted move from an ordinary
/// partition and trigger on-disk record recovery.
pub const MARKER_TYPE_GUID: [u8; 16] = [
    0x52, 0x4D, 0x56, 0x2D, 0x52, 0x45, 0x43, 0x4F, 0x56, 0x45, 0x52, 0x41, 0x42, 0x4C, 0x45, 0x01,
];

/// Errors returned by geometry validation, disk I/O, record handling, or recovery.
#[derive(Debug)]
pub enum Error {
    /// An underlying storage or filesystem operation failed.
    Io(std::io::Error),
    /// The caller asked for something that cannot be done.
    Invalid(String),
    /// The on-disk state contradicts the record or itself.
    Corrupt(String),
    /// Out of scope, for example a move that does not overlap.
    Unsupported(String),
    /// No move record exists and no new move request was supplied.
    NoRecord,
    /// Copied data did not match the still-intact source data.
    Verify(String),
    /// The partition table could not be read or written.
    Table(String),
}

impl Error {
    /// Construct an invalid-request error.
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }

    /// Construct an inconsistent-on-disk-state error.
    pub(crate) fn corrupt(msg: impl Into<String>) -> Self {
        Self::Corrupt(msg.into())
    }

    /// Construct an unsupported-operation error.
    pub(crate) fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }

    /// Construct a partition-table error.
    pub(crate) fn table(msg: impl Into<String>) -> Self {
        Self::Table(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Invalid(s) => write!(f, "invalid request: {s}"),
            Self::Corrupt(s) => write!(f, "inconsistent state: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::NoRecord => f.write_str("no interrupted move found"),
            Self::Verify(s) => write!(f, "verification failed: {s}"),
            Self::Table(s) => write!(f, "partition table: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
