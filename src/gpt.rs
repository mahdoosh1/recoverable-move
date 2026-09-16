// gpt.rs — single-file GPT reader/writer with CRC32 verification, LBA editing,
// and primary+backup synchronization.
//
// Compile:  rustc test.rs
//
// Usage:
//   ./test <image>                                   # print + verify
//   ./test <image> set <idx> <start_lba> <end_lba>   # modify + write both copies

use std::convert::TryInto;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

// ===========================================================================
// Constants
// ===========================================================================

pub const EFI_PTAB_HEADER_ID: u64 = 0x5452415020494645; // "EFI PART"
pub const DEFAULT_SECTOR_SIZE: u64 = 512;
pub const GPT_HEADER_MIN_SIZE: u32 = 92;
pub const GPT_ENTRY_MIN_SIZE: u32 = 128;
pub const GPT_NAME_LEN: usize = 36;

// ===========================================================================
// CRC32 (IEEE 802.3 polynomial, as required by the UEFI spec)
// ===========================================================================

const CRC32_TABLE: [u32; 256] = generate_crc32_table();

const fn generate_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB88320 } else { crc >> 1 };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    !crc
}

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidSignature { found: u64 },
    HeaderTooSmall { found: u32 },
    EntryTooSmall { found: u32 },
    Overflow,
    CrcMismatch { what: &'static str, expected: u32, computed: u32 },
    PartitionNotFound { index: u32 },
    StartAfterEnd { start: u64, end: u64 },
    OutOfUsableRange { start: u64, end: u64, first: u64, last: u64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::InvalidSignature { found } =>
                write!(f, "invalid GPT signature: 0x{found:016X}"),
            Error::HeaderTooSmall { found } =>
                write!(f, "GPT header size too small: {found}"),
            Error::EntryTooSmall { found } =>
                write!(f, "GPT entry size too small: {found}"),
            Error::Overflow => write!(f, "size overflow while reading GPT"),
            Error::CrcMismatch { what, expected, computed } =>
                write!(f, "{what} CRC mismatch: stored 0x{expected:08X}, computed 0x{computed:08X}"),
            Error::PartitionNotFound { index } =>
                write!(f, "partition index {index} not found"),
            Error::StartAfterEnd { start, end } =>
                write!(f, "start LBA {start} is greater than end LBA {end}"),
            Error::OutOfUsableRange { start, end, first, last } =>
                write!(f, "LBA range {start}..{end} is outside usable range [{first}, {last}]"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self { Error::Io(e) => Some(e), _ => None }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self { Error::Io(e) }
}

pub type Result<T> = std::result::Result<T, Error>;

// ===========================================================================
// Data model
// ===========================================================================

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
    /// Slot index in the partition entry array (0-based).
    pub index: u32,
    pub type_guid: [u8; 16],
    pub type_name: Option<&'static str>,
    pub unique_guid: [u8; 16],
    pub name: String,
    pub starting_lba: u64,
    /// Inclusive last LBA.
    pub ending_lba: u64,
    pub attributes: u64,
}

impl Partition {
    pub fn size_in_lbas(&self) -> u64 {
        self.ending_lba
            .saturating_sub(self.starting_lba)
            .saturating_add(1)
    }
    pub fn size_in_bytes(&self, sector_size: u64) -> u64 {
        self.size_in_lbas().saturating_mul(sector_size)
    }
    pub fn type_guid_string(&self) -> String { format_guid(&self.type_guid) }
    pub fn unique_guid_string(&self) -> String { format_guid(&self.unique_guid) }
}

pub struct Gpt {
    pub header: GptHeader,
    pub partitions: Vec<Partition>,
    pub sector_size: u64,
    header_bytes: [u8; GPT_HEADER_MIN_SIZE as usize],
    entry_array: Vec<u8>,
}

impl Gpt {
    // ---------------------------------------------------------------------
    // Reading
    // ---------------------------------------------------------------------

    pub fn read<R: Read + Seek>(reader: &mut R) -> Result<Self> {
        Self::read_with_sector_size(reader, DEFAULT_SECTOR_SIZE)
    }

    pub fn read_with_sector_size<R: Read + Seek>(
        reader: &mut R,
        sector_size: u64,
    ) -> Result<Self> {
        // --- header at LBA 1 ---
        let mut header_bytes = [0u8; GPT_HEADER_MIN_SIZE as usize];
        read_exact_at(reader, sector_size, &mut header_bytes)?;

        let signature = u64::from_le_bytes(header_bytes[0..8].try_into().unwrap());
        if signature != EFI_PTAB_HEADER_ID {
            return Err(Error::InvalidSignature { found: signature });
        }

        let header = parse_header(&header_bytes);
        if header.header_size < GPT_HEADER_MIN_SIZE {
            return Err(Error::HeaderTooSmall { found: header.header_size });
        }
        if header.size_of_partition_entry < GPT_ENTRY_MIN_SIZE {
            return Err(Error::EntryTooSmall { found: header.size_of_partition_entry });
        }

        // --- entry array ---
        let entry_size = header.size_of_partition_entry as u64;
        let num_entries = header.number_of_partition_entries as u64;
        let array_bytes = entry_size.checked_mul(num_entries).ok_or(Error::Overflow)?;
        let array_offset = header
            .partition_entry_lba
            .checked_mul(sector_size)
            .ok_or(Error::Overflow)?;

        let mut entry_array = vec![0u8; array_bytes as usize];
        read_exact_at(reader, array_offset, &mut entry_array)?;

        let partitions = parse_partitions(&entry_array, &header);

        Ok(Gpt { header, partitions, sector_size, header_bytes, entry_array })
    }

    // ---------------------------------------------------------------------
    // LBA bounds
    // ---------------------------------------------------------------------

    pub fn first_usable_lba(&self) -> u64 { self.header.first_usable_lba }
    pub fn last_usable_lba(&self) -> u64 { self.header.last_usable_lba }

    pub fn usable_lba_count(&self) -> u64 {
        self.header
            .last_usable_lba
            .saturating_sub(self.header.first_usable_lba)
            .saturating_add(1)
    }

    pub fn disk_guid_string(&self) -> String { format_guid(&self.header.disk_guid) }

    pub fn find_partition(&self, index: u32) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.index == index)
    }

    // ---------------------------------------------------------------------
    // Verification
    // ---------------------------------------------------------------------

    pub fn verify(&self) -> Result<()> {
        let mut hdr = self.header_bytes;
        hdr[16..20].fill(0);
        let computed = crc32(&hdr[..self.header.header_size as usize]);
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

    // ---------------------------------------------------------------------
    // Modification
    // ---------------------------------------------------------------------

    /// Update the LBA range of the partition with the given slot index.
    ///
    /// Both the in-memory `Partition` and the raw entry-array bytes are
    /// updated. The new range must fall inside `[first_usable_lba, last_usable_lba]`.
    /// Call `write_to()` afterwards to persist the change (both GPTs).
    pub fn set_partition_lbas(&mut self, index: u32, start: u64, end: u64) -> Result<()> {
        if start > end {
            return Err(Error::StartAfterEnd { start, end });
        }
        let first = self.header.first_usable_lba;
        let last = self.header.last_usable_lba;
        if start < first || end > last {
            return Err(Error::OutOfUsableRange { start, end, first, last });
        }

        let pos = self
            .partitions
            .iter()
            .position(|p| p.index == index)
            .ok_or(Error::PartitionNotFound { index })?;
        self.partitions[pos].starting_lba = start;
        self.partitions[pos].ending_lba = end;

        let entry_size = self.header.size_of_partition_entry as usize;
        let off = (index as usize) * entry_size;
        self.entry_array[off + 32..off + 40].copy_from_slice(&start.to_le_bytes());
        self.entry_array[off + 40..off + 48].copy_from_slice(&end.to_le_bytes());

        Ok(())
    }

    // ---------------------------------------------------------------------
    // Write-back (primary + backup)
    // ---------------------------------------------------------------------

    /// Write both the primary and the backup GPT to `writer`, recomputing
    /// all CRCs. The backup is a mirror of the primary with three fields
    /// swapped: `my_lba`, `alternate_lba`, and `partition_entry_lba`.
    pub fn write_to<W: Write + Seek>(&mut self, writer: &mut W) -> Result<()> {
        // --- 1. Entry array CRC (same value goes into both headers) ---
        let array_crc = crc32(&self.entry_array);
        self.header.partition_entry_array_crc32 = array_crc;
        self.header_bytes[88..92].copy_from_slice(&array_crc.to_le_bytes());

        // --- 2. Primary entry array at its LBA ---
        let primary_array_offset = self.header.partition_entry_lba * self.sector_size;
        writer.seek(SeekFrom::Start(primary_array_offset))?;
        writer.write_all(&self.entry_array)?;

        // --- 3. Primary header CRC ---
        let mut hdr = self.header_bytes;
        hdr[16..20].fill(0);
        let hdr_crc = crc32(&hdr[..self.header.header_size as usize]);
        self.header.header_crc32 = hdr_crc;
        self.header_bytes[16..20].copy_from_slice(&hdr_crc.to_le_bytes());

        writer.seek(SeekFrom::Start(self.sector_size))?;
        writer.write_all(&self.header_bytes)?;

        // --- 4. Backup entry array + header ---
        self.write_backup(writer)?;

        writer.flush()?;
        Ok(())
    }

    /// Write the backup entry array and header, mirroring the primary.
    fn write_backup<W: Write + Seek>(&mut self, writer: &mut W) -> Result<()> {
        let entry_bytes = self.header.size_of_partition_entry as u64
            * self.header.number_of_partition_entries as u64;
        let entry_sectors = (entry_bytes + self.sector_size - 1) / self.sector_size;

        let backup_header_lba = self.header.alternate_lba;
        let backup_array_lba = backup_header_lba
            .checked_sub(entry_sectors)
            .ok_or(Error::Overflow)?;

        // Backup entry array (identical bytes to the primary's).
        let array_offset = backup_array_lba * self.sector_size;
        writer.seek(SeekFrom::Start(array_offset))?;
        writer.write_all(&self.entry_array)?;

        // Backup header: copy primary's bytes, then patch three LBA fields.
        let mut backup = self.header_bytes;

        // my_lba = alternate_lba
        backup[24..32].copy_from_slice(&backup_header_lba.to_le_bytes());
        // alternate_lba = primary's my_lba (usually 1)
        backup[32..40].copy_from_slice(&self.header.my_lba.to_le_bytes());
        // partition_entry_lba = backup_array_lba
        backup[72..80].copy_from_slice(&backup_array_lba.to_le_bytes());

        // Recompute header CRC with the CRC field zeroed.
        backup[16..20].fill(0);
        let crc = crc32(&backup[..self.header.header_size as usize]);
        backup[16..20].copy_from_slice(&crc.to_le_bytes());

        let header_offset = backup_header_lba * self.sector_size;
        writer.seek(SeekFrom::Start(header_offset))?;
        writer.write_all(&backup)?;

        Ok(())
    }
}

// ===========================================================================
// Parsing helpers
// ===========================================================================

fn read_exact_at<R: Read + Seek>(r: &mut R, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    r.seek(SeekFrom::Start(offset))?;
    r.read_exact(buf)
}

fn parse_header(buf: &[u8; 92]) -> GptHeader {
    GptHeader {
        revision: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        header_size: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        header_crc32: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
        my_lba: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
        alternate_lba: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
        first_usable_lba: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
        last_usable_lba: u64::from_le_bytes(buf[48..56].try_into().unwrap()),
        disk_guid: buf[56..72].try_into().unwrap(),
        partition_entry_lba: u64::from_le_bytes(buf[72..80].try_into().unwrap()),
        number_of_partition_entries: u32::from_le_bytes(buf[80..84].try_into().unwrap()),
        size_of_partition_entry: u32::from_le_bytes(buf[84..88].try_into().unwrap()),
        partition_entry_array_crc32: u32::from_le_bytes(buf[88..92].try_into().unwrap()),
    }
}

fn parse_partitions(entry_array: &[u8], header: &GptHeader) -> Vec<Partition> {
    let entry_size = header.size_of_partition_entry as usize;
    let num = header.number_of_partition_entries as usize;
    let mut out = Vec::new();

    for i in 0..num {
        let off = i * entry_size;
        let e = &entry_array[off..off + GPT_ENTRY_MIN_SIZE as usize];

        let type_guid: [u8; 16] = e[0..16].try_into().unwrap();
        if type_guid.iter().all(|&b| b == 0) {
            continue; // unused slot
        }

        let unique_guid: [u8; 16] = e[16..32].try_into().unwrap();
        let starting_lba = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let ending_lba   = u64::from_le_bytes(e[40..48].try_into().unwrap());
        let attributes   = u64::from_le_bytes(e[48..56].try_into().unwrap());

        // UTF-16LE name, NUL-terminated.
        let mut units = [0u16; GPT_NAME_LEN];
        for (j, slot) in units.iter_mut().enumerate() {
            let lo = e[56 + j * 2];
            let hi = e[57 + j * 2];
            *slot = u16::from_le_bytes([lo, hi]);
        }
        let len = units.iter().position(|&c| c == 0).unwrap_or(GPT_NAME_LEN);
        let name = String::from_utf16_lossy(&units[..len]);

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

// ===========================================================================
// GUID helpers
// ===========================================================================

pub fn format_guid(raw: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let d2 = u16::from_le_bytes([raw[4], raw[5]]);
    let d3 = u16::from_le_bytes([raw[6], raw[7]]);
    let d = &raw[8..16];
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        d1, d2, d3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
    )
}

pub fn partition_type_name(guid: &[u8; 16]) -> Option<&'static str> {
    KNOWN_TYPES.iter().find(|(g, _)| *g == guid).map(|(_, n)| *n)
}

pub const KNOWN_TYPES: &[(&[u8; 16], &str)] = &[
    // C12A7328-F81F-11D2-BA4B-00A0C93EC93B
    (&[0x28,0x73,0x2A,0xC1,0x1F,0xF8,0xD2,0x11,0xBA,0x4B,0x00,0xA0,0xC9,0x3E,0xC9,0x3B], "EFI System Partition"),
    // EBD0A0A2-B9E5-4433-87C0-68B6B72699C7
    (&[0xA2,0xA0,0xD0,0xEB,0xE5,0xB9,0x33,0x44,0x87,0xC0,0x68,0xB6,0xB7,0x26,0x99,0xC7], "Microsoft Basic Data"),
    // 0FC63DAF-8483-4772-8E79-3D69D8477DE4
    (&[0xAF,0x3D,0xC6,0x0F,0x83,0x84,0x72,0x47,0x8E,0x79,0x3D,0x69,0xD8,0x47,0x7D,0xE4], "Linux Filesystem"),
    // 0657FD6D-A4AB-43C4-84E5-0933C84B4F4F
    (&[0x6D,0xFD,0x57,0x06,0xAB,0xA4,0xC4,0x43,0x84,0xE5,0x09,0x33,0xC8,0x4B,0x4F,0x4F], "Linux Swap"),
    // E6D6D379-F507-44C2-A23C-238F2A3DF928
    (&[0x79,0xD3,0xD6,0xE6,0x07,0xF5,0xC2,0x44,0xA2,0x3C,0x23,0x8F,0x2A,0x3D,0xF9,0x28], "Linux LVM"),
    // A19D880F-05FC-4D3B-A006-743F0F84911E
    (&[0x0F,0x88,0x9D,0xA1,0xFC,0x05,0x3B,0x4D,0xA0,0x06,0x74,0x3F,0x0F,0x84,0x91,0x1E], "Linux RAID"),
    // 21686148-6449-6E6F-744E-656564454649
    (&[0x48,0x61,0x68,0x21,0x49,0x64,0x6F,0x6E,0x74,0x4E,0x65,0x65,0x64,0x45,0x46,0x49], "BIOS Boot Partition"),
    // DE94BBA4-06D1-4D40-A16A-BFD50179D6AC
    (&[0xA4,0xBB,0x94,0xDE,0xD1,0x06,0x40,0x4D,0xA1,0x6A,0xBF,0xD5,0x01,0x79,0xD6,0xAC], "Windows Recovery Environment"),
    // E3C9E316-0B5C-4DB8-817D-F92DF00215AE
    (&[0x16,0xE3,0xC9,0xE3,0x5C,0x0B,0xB8,0x4D,0x81,0x7D,0xF9,0x2D,0xF0,0x02,0x15,0xAE], "Microsoft Reserved"),
];


