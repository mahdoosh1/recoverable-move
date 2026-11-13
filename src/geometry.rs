// Copyright (C) 2026 mahdoosh1
//
// This file is part of recoverable-move.
//
// recoverable-move is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 2 of the License, or
// (at your option) any later version.
//
// recoverable-move is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with recoverable-move. If not, see <https://www.gnu.org/licenses/>.

//! Checked arithmetic and address calculations for an overlapping partition move.
//! All extents use half-open LBA ranges: the start is included and the end excluded.
//!
//! ```
//! use recoverable_move::{Geometry, Placement};
//!
//! let movement = Geometry::new(100, 60, 12).unwrap();
//! assert_eq!(movement.target(), (112, 172));
//! assert_eq!(movement.union_lower(), 100);
//! assert_eq!(movement.union_upper(), 172);
//! assert_eq!(movement.record_range(Placement::After).unwrap(), (172, 174));
//! ```

use crate::Error;

/// Where the two record sectors sit relative to the union of source and target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// `[union_lower - 2, union_lower)`; the widened partition starts there.
    Before,
    /// `[union_upper, union_upper + 2)`; the widened partition ends there.
    After,
}

impl Placement {
    /// Decode the stable on-disk representation (`0` = before, `1` = after).
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Before,
            1 => Self::After,
            _ => return None,
        })
    }

    /// Return the stable on-disk representation of this placement.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Before => 0,
            Self::After => 1,
        }
    }
}

/// Source and target of one move, with the derived union bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// First LBA of the original partition.
    old_start: u64,
    /// Partition length in sectors.
    len: u64,
    /// Signed difference between target and original start LBAs.
    displacement: i64,
}

impl Geometry {
    /// Construct geometry after validating nonzero length, overlap, and arithmetic.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`](crate::Error::Invalid) for invalid or overflowing
    /// extents and [`Error::Unsupported`](crate::Error::Unsupported) for disjoint moves.
    pub fn new(old_start: u64, len: u64, displacement: i64) -> Result<Self, Error> {
        if len == 0 {
            return Err(Error::invalid("partition length is zero"));
        }
        if displacement == 0 {
            return Err(Error::invalid("displacement is zero"));
        }
        if displacement.unsigned_abs() >= len {
            return Err(Error::unsupported(
                "source and target do not overlap; a plain copy is enough",
            ));
        }

        let _source_end = old_start
            .checked_add(len)
            .ok_or_else(|| Error::invalid("source end overflows"))?;
        let target_start = old_start
            .checked_add_signed(displacement)
            .ok_or_else(|| Error::invalid("target start overflows"))?;
        let _target_end = target_start
            .checked_add(len)
            .ok_or_else(|| Error::invalid("target end overflows"))?;

        Ok(Self {
            old_start,
            len,
            displacement,
        })
    }

    /// Return the original partition start LBA.
    pub const fn old_start(self) -> u64 {
        self.old_start
    }

    /// Return the partition length in sectors.
    #[allow(clippy::len_without_is_empty)]
    pub const fn len(self) -> u64 {
        self.len
    }

    /// Return the signed movement distance in sectors.
    pub const fn displacement(self) -> i64 {
        self.displacement
    }

    /// Return the target partition start LBA.
    pub const fn target_start(self) -> u64 {
        self.old_start().wrapping_add(self.displacement() as u64)
    }

    /// Return the lower LBA of the union of source and target extents.
    pub fn union_lower(self) -> u64 {
        self.old_start().min(self.target_start())
    }

    /// Return the exclusive upper LBA of the source/target union.
    pub fn union_upper(self) -> u64 {
        self.old_start().max(self.target_start()) + self.len()
    }

    /// Whether the target starts at a higher LBA than the source.
    pub const fn is_forward(self) -> bool {
        self.displacement() > 0
    }

    /// Return the two-sector record range for `placement` as `(start, end)`.
    /// The returned range is half-open.
    pub fn record_range(self, placement: Placement) -> Result<(u64, u64), Error> {
        match placement {
            Placement::Before => {
                let start = self
                    .union_lower()
                    .checked_sub(2)
                    .ok_or_else(|| Error::invalid("no room for the record before the union"))?;
                Ok((start, self.union_lower()))
            }
            Placement::After => Ok((self.union_upper(), self.union_upper() + 2)),
        }
    }

    /// Return the widened partition extent used while the move is in flight.
    /// The returned `(start, end)` range is half-open.
    pub fn widened(self, placement: Placement) -> Result<(u64, u64), Error> {
        let (start, end) = self.record_range(placement)?;
        match placement {
            Placement::Before => Ok((start, self.union_upper())),
            Placement::After => Ok((self.union_lower(), end)),
        }
    }

    /// Return the final target extent as a half-open `(start, end)` range.
    pub const fn target(self) -> (u64, u64) {
        (self.target_start(), self.target_start() + self.len())
    }

    /// Return the number of chunks needed for `len` sectors at `chunk_size`.
    /// The caller must provide a nonzero chunk size.
    pub fn chunks_total(self, chunk_size: u64) -> u64 {
        self.len().div_ceil(chunk_size)
    }
}
