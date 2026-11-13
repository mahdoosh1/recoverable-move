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


//! Interactive CLI for the recoverable-move library.
//!
//! The program accepts an optional disk image/device path, then offers commands
//! to list and verify GPT metadata, move a partition, or recover an interrupted
//! move. Operations that modify storage require explicit confirmation.

use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

use gptman::GPT;
use recoverable_move::traits::{FileStore, RecordStore};
use recoverable_move::{execute, Disk, GptDisk, MoveRequest, Verify};

/// Parse the optional disk path and start the command loop.
fn main() {
    let mut args = env::args_os().skip(1);
    let path = match args.next() {
        Some(arg) => PathBuf::from(arg),
        None => match prompt_line("Disk image/device: ") {
            Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
            _ => {
                eprintln!("no disk image/device given");
                process::exit(2);
            }
        },
    };

    if args.next().is_some() {
        eprintln!("usage: recoverable-move [DISK_IMAGE_OR_DEVICE]");
        process::exit(2);
    }
    if !path.exists() {
        eprintln!("no such file: {}", path.display());
        process::exit(2);
    }
    if let Err(error) = repl(&path) {
        eprintln!("error: {error}");
        process::exit(1);
    }
}

/// Run the interactive command prompt for the selected disk.
fn repl(path: &Path) -> Result<(), String> {

    println!("recoverable-move");
    println!("Copyright (C) 2026 mahdoosh1");
    println!("");
    println!("This program comes with ABSOLUTELY NO WARRANTY.");
    println!("This is free software, and you are welcome to redistribute it");
    println!("under certain conditions.");
    println!("")
    println!("See COPYING for the license terms.");

    println!("disk: {}", path.display());
    print_help();
    if let Err(error) = cmd_list(path) {
        eprintln!("warning: {error}");
    }

    loop {
        print!("\n> ");
        io::stdout().flush().ok();
        let line = match prompt_line("") {
            Ok(line) => line,
            Err(_) => break,
        };
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or("");
        let result = match command {
            "l" | "list" => cmd_list(path),
            "v" | "verify" => cmd_verify(path),
            "m" | "move" => cmd_move(path),
            "r" | "recover" => cmd_recover(path),
            "h" | "help" | "?" => {
                print_help();
                Ok(())
            }
            "q" | "quit" | "exit" => break,
            other => {
                eprintln!("unknown command `{other}` (try `help`)");
                Ok(())
            }
        };
        if let Err(error) = result {
            eprintln!("error: {error}");
        }
    }
    Ok(())
}

/// Print available command names and their purpose.
fn print_help() {
    println!("\ncommands:");
    println!("  l, list       show the GPT partition table");
    println!("  v, verify     verify the GPT header and entry-array CRC32 values");
    println!("  m, move       move a partition (interactive)");
    println!("  r, recover    resume an interrupted move");
    println!("  h, help       show this help");
    println!("  q, quit       exit");
}

/// Open a disk read-only and parse its GPT using gptman.
fn read_gpt(path: &Path) -> Result<GPT, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    GPT::find_from(&mut file).map_err(|e| format!("read GPT from {}: {e}", path.display()))
}

/// Display disk metadata and every used GPT partition entry.
fn cmd_list(path: &Path) -> Result<(), String> {
    let gpt = read_gpt(path)?;
    let h = &gpt.header;
    println!("Disk GUID : {}", format_guid(&h.disk_guid));
    println!("Sector sz : {} bytes", gpt.sector_size);
    println!(
        "Usable LBA: {}..={} ({} sectors)",
        h.first_usable_lba,
        h.last_usable_lba,
        h.last_usable_lba - h.first_usable_lba + 1
    );
    println!();
    println!("Idx  Type GUID                            Start           End          Size  Name");
    println!("{}", "-".repeat(112));
    for (index, partition) in gpt.iter() {
        if !partition.is_used() {
            continue;
        }
        let size = partition
            .ending_lba
            .checked_sub(partition.starting_lba)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| format!("partition {index} has invalid LBA bounds"))?;
        println!(
            "{index:<4} {} {:>14} {:>14} {:>10}  {}",
            format_guid(&partition.partition_type_guid),
            partition.starting_lba,
            partition.ending_lba,
            size,
            partition.partition_name.as_str()
        );
    }
    Ok(())
}

/// Recompute and compare the GPT header and partition-array CRC32 values.
fn cmd_verify(path: &Path) -> Result<(), String> {
    let gpt = read_gpt(path)?;
    let entries: Vec<_> = gpt.iter().map(|(_, entry)| entry.clone()).collect();
    let header_crc = gpt.header.generate_crc32_checksum();
    if header_crc != gpt.header.crc32_checksum {
        return Err(format!(
            "header CRC mismatch: stored {:#010x}, computed {header_crc:#010x}",
            gpt.header.crc32_checksum
        ));
    }
    let entries_crc = gpt
        .header
        .generate_partition_entry_array_crc32(&entries);
    if entries_crc != gpt.header.partition_entry_array_crc32 {
        return Err(format!(
            "partition-array CRC mismatch: stored {:#010x}, computed {entries_crc:#010x}",
            gpt.header.partition_entry_array_crc32
        ));
    }
    println!("GPT CRC32 verification: OK");
    Ok(())
}

/// Collect move parameters, confirm the destructive operation, and execute it.
fn cmd_move(path: &Path) -> Result<(), String> {
    cmd_list(path)?;
    println!();
    let index: u32 = prompt_required("Partition index: ")?;
    let mut disk = GptDisk::open(path, index).map_err(|e| e.to_string())?;
    let current = disk.partition().map_err(|e| e.to_string())?;
    let old_start = current.start;
    let len = current.len;
    let old_end = old_start
        .checked_add(len - 1)
        .ok_or_else(|| "current partition end overflows".to_string())?;
    println!("partition {index}: LBA {old_start}..={old_end} ({len} sectors)");

    let new_start: u64 = prompt_required("New start LBA: ")?;
    let displacement = i128::from(new_start) - i128::from(old_start);
    if displacement == 0 {
        return Err("displacement is zero (partition already there)".into());
    }
    let max_chunk = displacement.unsigned_abs() as u64;
    let new_end = new_start
        .checked_add(len - 1)
        .ok_or_else(|| "new partition end overflows".to_string())?;

    let subchunk: u64 = prompt_required("Subchunk (sectors): ")?;
    if subchunk == 0 || subchunk > max_chunk {
        return Err(format!("subchunk must be in 1..={max_chunk} sectors"));
    }
    let chunk = prompt_optional::<u64>(&format!("Chunk (sectors, blank = {max_chunk}): "))?
        .unwrap_or(max_chunk);
    if chunk == 0 || chunk > max_chunk {
        return Err(format!("chunk must be in 1..={max_chunk} sectors"));
    }
    if subchunk > chunk {
        return Err(format!("subchunk ({subchunk}) exceeds chunk ({chunk})"));
    }
    let store_path = prompt_line("Metadata mirror path (blank = none): ")?;
    let store_path = if store_path.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(store_path.trim()))
    };

    let chunks = len.div_ceil(chunk);
    println!("\nabout to move partition {index}:");
    println!("  from LBA {old_start}..={old_end}");
    println!("  to   LBA {new_start}..={new_end} (displacement {displacement:+})");
    println!("  chunk    {chunk} sectors → {chunks} chunk(s)");
    println!("  subchunk {subchunk} sectors");
    println!(
        "  metadata mirror: {}",
        store_path.as_ref().map_or_else(|| "(none)".into(), |p| p.display().to_string())
    );
    if store_path.is_none() {
        eprintln!("warning: no metadata mirror; recovery relies on the on-disk record");
    }
    if !confirm("Proceed? This operation writes to the disk") {
        return Err("aborted by user".into());
    }

    let mut store = store_path.map(FileStore::new);
    let store_ref: Option<&mut dyn RecordStore> =
        store.as_mut().map(|s| s as &mut dyn RecordStore);
    let request = MoveRequest {
        old_start,
        new_start,
        len,
        chunk_size: chunk,
        subchunk_size: subchunk,
        verify: Verify::Full,
    };
    execute(&mut disk, store_ref, Some(request)).map_err(|e| format!("move failed: {e}"))?;
    println!("\nmove complete\n");
    cmd_list(path)
}

/// Resume a move from the selected partition's on-disk journal.
fn cmd_recover(path: &Path) -> Result<(), String> {
    println!("Looking for an interrupted move.\n");
    cmd_list(path)?;
    println!();
    let index: u32 = prompt_required("Partition index: ")?;
    let store_input = prompt_line("Metadata mirror path (blank = on-disk record only): ")?;
    let mut store = if store_input.trim().is_empty() {
        None
    } else {
        Some(FileStore::new(PathBuf::from(store_input.trim())))
    };
    let store_ref: Option<&mut dyn RecordStore> =
        store.as_mut().map(|s| s as &mut dyn RecordStore);
    let mut disk = GptDisk::open(path, index).map_err(|e| e.to_string())?;
    execute(&mut disk, store_ref, None).map_err(|e| format!("recovery failed: {e}"))?;
    println!("\nrecovery complete\n");
    cmd_list(path)
}

/// Print a prompt and read one line, returning an error on EOF or I/O failure.
fn prompt_line(message: &str) -> Result<String, String> {
    if !message.is_empty() {
        print!("{message}");
        io::stdout().flush().map_err(|e| e.to_string())?;
    }
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => Err("end of input".into()),
        Ok(_) => Ok(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Repeatedly prompt until the input parses as `T`.
fn prompt_required<T: std::str::FromStr>(message: &str) -> Result<T, String> {
    loop {
        let input = prompt_line(message)?;
        match input.trim().parse::<T>() {
            Ok(value) => return Ok(value),
            Err(_) => eprintln!("  enter a valid value"),
        }
    }
}

/// Parse an optional value; blank input returns `None`.
fn prompt_optional<T: std::str::FromStr>(message: &str) -> Result<Option<T>, String> {
    let input = prompt_line(message)?;
    if input.trim().is_empty() {
        return Ok(None);
    }
    input.trim().parse::<T>().map(Some).map_err(|_| "invalid value".into())
}

/// Ask for explicit yes/no confirmation; all answers except yes are false.
fn confirm(message: &str) -> bool {
    print!("{message} [y/N] ");
    let _ = io::stdout().flush();
    matches!(
        prompt_line("").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )
}

/// Format a GPT GUID using the conventional mixed-endian textual layout.
fn format_guid(guid: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid[3], guid[2], guid[1], guid[0], guid[5], guid[4], guid[7], guid[6],
        guid[8], guid[9], guid[10], guid[11], guid[12], guid[13], guid[14], guid[15]
    )
}
