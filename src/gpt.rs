//! GPT reader/writer with CRC32 verification and primary + backup
//! synchronization.
//!
//! Only the fields needed by the mover are modelled. Unknown type GUIDs
//! are preserved verbatim; known ones get a human-readable name.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub const EFI_PTAB_HEADER_ID: u64 = 0x5452415020494645; // "EFI PART"
pub const DEFAULT_SECTOR_SIZE: u64 = 512;
pub const GPT_HEADER_MIN_SIZE: u32 = 92;
pub const GPT_ENTRY_MIN_SIZE: u32 = 128;
pub const GPT_NAME_LEN: usize = 36;
/// Generous upper bound on the entry array; real tables are a few KiB.
pub const MAX_ENTRY_ARRAY_BYTES: u64 = 4 * 1024 * 1024;

// --- CRC32 -------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = generate_crc32_table();

const fn generate_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[index];
    }
    !crc
}

// --- Errors ------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidSignature { found: u64 },
    HeaderTooSmall { found: u32 },
    HeaderTooLarge { found: u32, sector_size: u64 },
    EntryTooSmall { found: u32 },
    EntryArrayTooLarge { found: u64 },
    Overflow,
    CrcMismatch {
        what: &'static str,
        expected: u32,
        computed: u32,
    },
    PartitionNotFound { index: u32 },
    StartAfterEnd { start: u64, end: u64 },
    OutOfUsableRange {
        start: u64,
        end: u64,
        first: u64,
        last: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::InvalidSignature { found } => {
                write!(f, "invalid GPT signature: 0x{found:016X}")
            }
            Self::HeaderTooSmall { found } => {
                write!(f, "GPT header size too small: {found}")
            }
            Self::HeaderTooLarge { found, sector_size } => {
                write!(f, "GPT header size {found} exceeds sector size {sector_size}")
            }
            Self::EntryTooSmall { found } => {
                write!(f, "GPT entry size too small: {found}")
            }
            Self::EntryArrayTooLarge { found } => {
                write!(f, "GPT entry array too large: {found} bytes")
            }
            Self::Overflow => f.write_str("size overflow while reading GPT"),
            Self::CrcMismatch { what, expected, computed } => write!(
                f,
                "{what} CRC mismatch: stored 0x{expected:08X}, computed 0x{computed:08X}"
            ),
            Self::PartitionNotFound { index } => {
                write!(f, "partition index {index} not found")
            }
            Self::StartAfterEnd { start, end } => {
                write!(f, "start LBA {start} greater than end LBA {end}")
            }
            Self::OutOfUsableRange { start, end, first, last } => write!(
                f,
                "LBA range {start}..{end} is outside usable range [{first}, {last}]"
            ),
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

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// --- Data model --------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GptHeader {
    pub revision: u32,
    pub header_size: u32,
    pub header_crc32: u32,
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: [u8; 16],
    pub partition_entry_lba: u64,
    pub number_of_partition_entries: u32,
    pub size_of_partition_entry: u32,
    pub partition_entry_array_crc32: u32,
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub index: u32,
    pub type_guid: [u8; 16],
    pub type_name: Option<&'static str>,
    pub unique_guid: [u8; 16],
    pub name: String,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub attributes: u64,
}

impl Partition {
    pub const fn size_in_lbas(&self) -> u64 {
        self.ending_lba
            .saturating_sub(self.starting_lba)
            .saturating_add(1)
    }

    pub const fn size_in_bytes(&self, sector_size: u64) -> u64 {
        self.size_in_lbas().saturating_mul(sector_size)
    }
}

pub struct Gpt {
    pub header: GptHeader,
    pub partitions: Vec<Partition>,
    pub sector_size: u64,
    header_bytes: [u8; GPT_HEADER_MIN_SIZE as usize],
    entry_array: Vec<u8>,
}

impl fmt::Debug for Gpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gpt")
            .field("header", &self.header)
            .field("partitions", &self.partitions)
            .field("sector_size", &self.sector_size)
            .finish_non_exhaustive()
    }
}

// --- Reading -----------------------------------------------------------

impl Gpt {
    pub fn read<R: Read + Seek>(mut reader: R) -> Result<Self> {
        Self::read_with_sector_size(&mut reader, DEFAULT_SECTOR_SIZE)
    }

    pub fn read_with_sector_size<R: Read + Seek>(
        mut reader: R,
        sector_size: u64,
    ) -> Result<Self> {
        let mut header_bytes = [0u8; GPT_HEADER_MIN_SIZE as usize];
        read_exact_at(&mut reader, sector_size, &mut header_bytes)?;

        let signature = read_le::<8>(&header_bytes, 0);
        if u64::from_le_bytes(signature) != EFI_PTAB_HEADER_ID {
            return Err(Error::InvalidSignature {
                found: u64::from_le_bytes(signature),
            });
        }

        let header = parse_header(&header_bytes);
        if header.header_size < GPT_HEADER_MIN_SIZE {
            return Err(Error::HeaderTooSmall { found: header.header_size });
        }
        if header.header_size as u64 > sector_size {
            return Err(Error::HeaderTooLarge {
                found: header.header_size,
                sector_size,
            });
        }
        if header.size_of_partition_entry < GPT_ENTRY_MIN_SIZE {
            return Err(Error::EntryTooSmall {
                found: header.size_of_partition_entry,
            });
        }
        if header.first_usable_lba > header.last_usable_lba {
            return Err(Error::OutOfUsableRange {
                start: header.first_usable_lba,
                end: header.last_usable_lba,
                first: header.first_usable_lba,
                last: header.last_usable_lba,
            });
        }

        let entry_size = header.size_of_partition_entry as u64;
        let num_entries = header.number_of_partition_entries as u64;
        let array_bytes = entry_size
            .checked_mul(num_entries)
            .ok_or(Error::Overflow)?;
        let array_offset = header
            .partition_entry_lba
            .checked_mul(sector_size)
            .ok_or(Error::Overflow)?;
        array_offset
            .checked_add(array_bytes)
            .ok_or(Error::Overflow)?;
        if array_bytes > MAX_ENTRY_ARRAY_BYTES {
            return Err(Error::EntryArrayTooLarge { found: array_bytes });
        }

        let mut entry_array = vec![0u8; array_bytes as usize];
        read_exact_at(&mut reader, array_offset, &mut entry_array)?;
        let partitions = parse_partitions(&entry_array, &header);

        Ok(Self { header, partitions, sector_size, header_bytes, entry_array })
    }

    pub const fn first_usable_lba(&self) -> u64 {
        self.header.first_usable_lba
    }

    pub const fn last_usable_lba(&self) -> u64 {
        self.header.last_usable_lba
    }

    pub const fn usable_lba_count(&self) -> u64 {
        self.header
            .last_usable_lba
            .saturating_sub(self.header.first_usable_lba)
            .saturating_add(1)
    }

    pub fn find_partition(&self, index: u32) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.index == index)
    }

    pub fn verify(&self) -> Result<()> {
        let mut header = self.header_bytes;
        header[16..20].fill(0);
        let computed = crc32(&header[..self.header.header_size as usize]);
        if computed != self.header.header_crc32 {
            return Err(Error::CrcMismatch {
                what: "GPT header",
                expected: self.header.header_crc32,
                computed,
            });
        }
        let computed = crc32(&self.entry_array);
        if computed != self.header.partition_entry_array_crc32 {
            return Err(Error::CrcMismatch {
                what: "partition entry array",
                expected: self.header.partition_entry_array_crc32,
                computed,
            });
        }
        Ok(())
    }
}

// --- Modification ------------------------------------------------------

impl Gpt {
    /// Update the LBA range of the partition with the given slot index.
    ///
    /// Both the in-memory `Partition` and the raw entry-array bytes are
    /// updated. The new range must fall inside
    /// `[first_usable_lba, last_usable_lba]`. Call [`Gpt::write_to`]
    /// afterwards to persist the change to both GPT copies.
    pub fn set_partition_lbas(
        &mut self,
        index: u32,
        start: u64,
        end: u64,
    ) -> Result<()> {
        if start > end {
            return Err(Error::StartAfterEnd { start, end });
        }
        let first = self.header.first_usable_lba;
        let last = self.header.last_usable_lba;
        if start < first || end > last {
            return Err(Error::OutOfUsableRange { start, end, first, last });
        }
        let position = self
            .partitions
            .iter()
            .position(|p| p.index == index)
            .ok_or(Error::PartitionNotFound { index })?;
        self.partitions[position].starting_lba = start;
        self.partitions[position].ending_lba = end;

        let entry_size = self.header.size_of_partition_entry as usize;
        let offset = (index as usize) * entry_size;
        self.entry_array[offset + 32..offset + 40]
            .copy_from_slice(&start.to_le_bytes());
        self.entry_array[offset + 40..offset + 48]
            .copy_from_slice(&end.to_le_bytes());
        Ok(())
    }

    /// Write both the primary and backup GPT to `writer`, recomputing all
    /// CRCs. The backup is a mirror of the primary with three fields
    /// swapped: `my_lba`, `alternate_lba`, and `partition_entry_lba`.
    pub fn write_to<W: Write + Seek>(&mut self, writer: &mut W) -> Result<()> {
        let array_crc = crc32(&self.entry_array);
        self.header.partition_entry_array_crc32 = array_crc;
        self.header_bytes[88..92].copy_from_slice(&array_crc.to_le_bytes());

        let primary_array_offset = self.header.partition_entry_lba * self.sector_size;
        writer.seek(SeekFrom::Start(primary_array_offset))?;
        writer.write_all(&self.entry_array)?;

        let mut header = self.header_bytes;
        header[16..20].fill(0);
        let header_crc = crc32(&header[..self.header.header_size as usize]);
        self.header.header_crc32 = header_crc;
        self.header_bytes[16..20].copy_from_slice(&header_crc.to_le_bytes());
        writer.seek(SeekFrom::Start(self.sector_size))?;
        writer.write_all(&self.header_bytes)?;

        self.write_backup(writer)?;
        writer.flush()?;
        Ok(())
    }

    fn write_backup<W: Write + Seek>(&mut self, writer: &mut W) -> Result<()> {
        let entry_bytes = self.header.size_of_partition_entry as u64
            * self.header.number_of_partition_entries as u64;
        let entry_sectors = entry_bytes.div_ceil(self.sector_size);
        let backup_header_lba = self.header.alternate_lba;
        let backup_array_lba = backup_header_lba
            .checked_sub(entry_sectors)
            .ok_or(Error::Overflow)?;

        let array_offset = backup_array_lba * self.sector_size;
        writer.seek(SeekFrom::Start(array_offset))?;
        writer.write_all(&self.entry_array)?;

        let mut backup = self.header_bytes;
        backup[24..32].copy_from_slice(&backup_header_lba.to_le_bytes());
        backup[32..40].copy_from_slice(&self.header.my_lba.to_le_bytes());
        backup[72..80].copy_from_slice(&backup_array_lba.to_le_bytes());

        backup[16..20].fill(0);
        let crc = crc32(&backup[..self.header.header_size as usize]);
        backup[16..20].copy_from_slice(&crc.to_le_bytes());

        let header_offset = backup_header_lba * self.sector_size;
        writer.seek(SeekFrom::Start(header_offset))?;
        writer.write_all(&backup)?;
        Ok(())
    }
}

// --- Parsing helpers ---------------------------------------------------

fn read_le<const N: usize>(buf: &[u8], offset: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[offset..offset + N]);
    out
}

fn read_exact_at<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    reader.seek(SeekFrom::Start(offset))?;
    reader.read_exact(buf)
}

fn parse_header(buf: &[u8; GPT_HEADER_MIN_SIZE as usize]) -> GptHeader {
    GptHeader {
        revision: u32::from_le_bytes(read_le::<4>(buf, 8)),
        header_size: u32::from_le_bytes(read_le::<4>(buf, 12)),
        header_crc32: u32::from_le_bytes(read_le::<4>(buf, 16)),
        my_lba: u64::from_le_bytes(read_le::<8>(buf, 24)),
        alternate_lba: u64::from_le_bytes(read_le::<8>(buf, 32)),
        first_usable_lba: u64::from_le_bytes(read_le::<8>(buf, 40)),
        last_usable_lba: u64::from_le_bytes(read_le::<8>(buf, 48)),
        disk_guid: read_le::<16>(buf, 56),
        partition_entry_lba: u64::from_le_bytes(read_le::<8>(buf, 72)),
        number_of_partition_entries: u32::from_le_bytes(read_le::<4>(buf, 80)),
        size_of_partition_entry: u32::from_le_bytes(read_le::<4>(buf, 84)),
        partition_entry_array_crc32: u32::from_le_bytes(read_le::<4>(buf, 88)),
    }
}

fn parse_partitions(entry_array: &[u8], header: &GptHeader) -> Vec<Partition> {
    let entry_size = header.size_of_partition_entry as usize;
    let count = header.number_of_partition_entries as usize;
    let mut out = Vec::new();

    for i in 0..count {
        let offset = i * entry_size;
        let entry = &entry_array[offset..offset + GPT_ENTRY_MIN_SIZE as usize];

        let type_guid: [u8; 16] = read_le(entry, 0);
        if type_guid.iter().all(|&b| b == 0) {
            continue; // unused slot
        }

        let unique_guid: [u8; 16] = read_le(entry, 16);
        let starting_lba = u64::from_le_bytes(read_le(entry, 32));
        let ending_lba = u64::from_le_bytes(read_le(entry, 40));
        let attributes = u64::from_le_bytes(read_le(entry, 48));
        let name = parse_name(entry);

        out.push(Partition {
            index: i as u32,
            type_guid,
            type_name: partition_type_name(&type_guid),
            unique_guid,
            name,
            starting_lba,
            ending_lba,
            attributes,
        });
    }
    out
}

fn parse_name(entry: &[u8]) -> String {
    let mut units = [0u16; GPT_NAME_LEN];
    for (i, slot) in units.iter_mut().enumerate() {
        let lo = entry[56 + i * 2];
        let hi = entry[57 + i * 2];
        *slot = u16::from_le_bytes([lo, hi]);
    }
    let end = units.iter().position(|&c| c == 0).unwrap_or(GPT_NAME_LEN);
    String::from_utf16_lossy(&units[..end])
}

// --- GUID helpers ------------------------------------------------------

pub fn format_guid(raw: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let d2 = u16::from_le_bytes([raw[4], raw[5]]);
    let d3 = u16::from_le_bytes([raw[6], raw[7]]);
    format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]
    )
}

pub fn partition_type_name(guid: &[u8; 16]) -> Option<&'static str> {
    KNOWN_TYPES
        .iter()
        .find(|(known, _)| *known == guid)
        .map(|(_, name)| *name)
}

pub const KNOWN_TYPES: &[(&[u8; 16], &str)] = &[
    (
        &[
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ],
        "EFI System Partition",
    ),
    (
        &[
            0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
            0x99, 0xC7,
        ],
        "Microsoft Basic Data",
    ),
    (
        &[
            0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47,
            0x7D, 0xE4,
        ],
        "Linux Filesystem",
    ),
    (
        &[
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B,
            0x4F, 0x4F,
        ],
        "Linux Swap",
    ),
    (
        &[
            0x79, 0xD3, 0xD6, 0xE6, 0x07, 0xF5, 0xC2, 0x44, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D,
            0xF9, 0x28,
        ],
        "Linux LVM",
    ),
    (
        &[
            0x0F, 0x88, 0x9D, 0xA1, 0xFC, 0x05, 0x3B, 0x4D, 0xA0, 0x06, 0x74, 0x3F, 0x0F, 0x84,
            0x91, 0x1E,
        ],
        "Linux RAID",
    ),
    (
        &[
            0x48, 0x61, 0x68, 0x21, 0x49, 0x64, 0x6F, 0x6E, 0x74, 0x4E, 0x65, 0x65, 0x64, 0x45,
            0x46, 0x49,
        ],
        "BIOS Boot Partition",
    ),
    (
        &[
            0xA4, 0xBB, 0x94, 0xDE, 0xD1, 0x06, 0x40, 0x4D, 0xA1, 0x6A, 0xBF, 0xD5, 0x01, 0x79,
            0xD6, 0xAC,
        ],
        "Windows Recovery Environment",
    ),
    (
        &[
            0x16, 0xE3, 0xC9, 0xE3, 0x5C, 0x0B, 0xB8, 0x4D, 0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02,
            0x15, 0xAE,
        ],
        "Microsoft Reserved",
    ),
];
