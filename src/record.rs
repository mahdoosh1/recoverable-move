//! Versioned, checksummed on-disk journal records used for crash recovery.
//!
//! ```
//! use recoverable_move::{Geometry, Placement, Record, Phase};
//!
//! let geometry = Geometry::new(100, 60, 12).unwrap();
//! let record = Record::new(geometry, Placement::After, 6, 2, [7; 16]).unwrap();
//! let bytes = record.encode(512);
//! let decoded = Record::decode(&bytes).unwrap();
//! assert_eq!(decoded.phase, Phase::Zero);
//! assert_eq!(decoded.geometry, geometry);
//! ```

use crate::{Error, Geometry, Placement};

/// Eight-byte signature identifying a move record.
pub const MAGIC: [u8; 8] = *b"MOVEPART";
/// Current binary record format version.
pub const VERSION: u16 = 1;
/// Two adjacent sectors, written in order and synced between the writes.
/// Number of adjacent sectors reserved for redundant record copies.
pub const RECORD_SECTORS: u64 = 2;
/// Byte offset of the CRC field in the encoded record header.
const CRC_OFFSET: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
/// Journal state machine phase. Values are stable on-disk tags.
pub enum Phase {
    /// Clear destination checkpoint sectors before copying a chunk.
    Zero = 0,
    /// Copy non-checkpoint sectors from source to destination.
    Copy = 4,
    /// Restore real source data into checkpoint sectors and verify.
    Finalize = 8,
    /// Shrink to the final target extent and remove the journal.
    Resize = 12,
}

impl Phase {
    /// Decode a phase tag; unknown values return `None`.
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Zero,
            4 => Self::Copy,
            8 => Self::Finalize,
            12 => Self::Resize,
            _ => return None,
        })
    }

    /// Return this phase as its stable on-disk tag.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The record of an in-flight move, one per record sector.
///
/// Both sectors hold the same bytes. The lower LBA is written first, so when a
/// crash leaves them disagreeing the lower one describes the state that was
/// actually reached and the higher one is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Current state-machine phase.
    pub phase: Phase,
    /// Location of the two journal sectors relative to the moved extent.
    pub placement: Placement,
    /// Original and target partition geometry.
    pub geometry: Geometry,
    /// Maximum number of sectors copied in one chunk.
    pub chunk_size: u64,
    /// Interval between checkpoint sectors.
    pub subchunk_size: u64,
    /// Number of fully completed chunks.
    pub chunks_done: u64,
    /// Partition type GUID restored after the move completes.
    pub original_type_guid: [u8; 16],
}

impl Record {
    /// Construct and validate a fresh journal record.
    pub fn new(
        geometry: Geometry,
        placement: Placement,
        chunk_size: u64,
        subchunk_size: u64,
        original_type_guid: [u8; 16],
    ) -> Result<Self, Error> {
        let record = Self {
            phase: Phase::Zero,
            placement,
            geometry,
            chunk_size,
            subchunk_size,
            chunks_done: 0,
            original_type_guid,
        };
        record.validate()?;
        Ok(record)
    }

    /// Check chunk parameters and progress bounds before encoding or use.
    fn validate(&self) -> Result<(), Error> {
        if self.chunk_size == 0 || self.chunk_size > self.geometry.displacement().unsigned_abs() {
            return Err(Error::corrupt("chunk size is out of range"));
        }
        if self.subchunk_size == 0 || self.subchunk_size > self.chunk_size {
            return Err(Error::corrupt("subchunk size is out of range"));
        }
        if self.chunks_done > self.geometry.chunks_total(self.chunk_size) {
            return Err(Error::corrupt("chunk progress exceeds the chunk count"));
        }
        Ok(())
    }

    /// Return the total number of chunks represented by this record.
    pub fn chunks_total(&self) -> u64 {
        self.geometry.chunks_total(self.chunk_size)
    }

    /// Encode this record into one sector-sized byte buffer, including CRC-32.
    /// `sector_size` must be at least 84 bytes and fit in a `u32`.
    pub fn encode(&self, sector_size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; sector_size];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..10].copy_from_slice(&VERSION.to_le_bytes());
        buf[10..14].copy_from_slice(&(sector_size as u32).to_le_bytes());
        buf[14] = self.phase.as_u8();
        buf[15] = self.placement.as_u8();
        buf[16..24].copy_from_slice(&self.chunk_size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.subchunk_size.to_le_bytes());
        buf[32..40].copy_from_slice(&self.chunks_done.to_le_bytes());
        buf[40..48].copy_from_slice(&self.geometry.old_start().to_le_bytes());
        buf[48..56].copy_from_slice(&self.geometry.len().to_le_bytes());
        buf[56..64].copy_from_slice(&self.geometry.displacement().to_le_bytes());
        buf[64..80].copy_from_slice(&self.original_type_guid);
        let crc = crc32(&buf[..CRC_OFFSET]);
        buf[CRC_OFFSET..CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decode a record. Returns `None` if the signature, version, size, CRC,
    /// enum tags, geometry, or progress fields are invalid.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CRC_OFFSET + 4 || buf[0..8] != MAGIC {
            return None;
        }
        if u16::from_le_bytes(buf[8..10].try_into().ok()?) != VERSION {
            return None;
        }
        if u32::from_le_bytes(buf[10..14].try_into().ok()?) as usize != buf.len() {
            return None;
        }
        let stored = u32::from_le_bytes(buf[CRC_OFFSET..CRC_OFFSET + 4].try_into().ok()?);
        if stored != crc32(&buf[..CRC_OFFSET]) {
            return None;
        }

        let record = Self {
            phase: Phase::from_u8(buf[14])?,
            placement: Placement::from_u8(buf[15])?,
            chunk_size: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            subchunk_size: u64::from_le_bytes(buf[24..32].try_into().ok()?),
            chunks_done: u64::from_le_bytes(buf[32..40].try_into().ok()?),
            geometry: Geometry::new(
                u64::from_le_bytes(buf[40..48].try_into().ok()?),
                u64::from_le_bytes(buf[48..56].try_into().ok()?),
                i64::from_le_bytes(buf[56..64].try_into().ok()?),
            )
            .ok()?,
            original_type_guid: buf[64..80].try_into().ok()?,
        };
        record.validate().ok()?;
        Some(record)
    }
}

/// CRC-32/ISO-HDLC, the same polynomial/initialization used by GPT.
/// Compute CRC-32/ISO-HDLC over the record header bytes.
fn crc32(data: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(data)
}
