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

use std::path::Path;
use std::str::FromStr;
use std::{env, path::PathBuf, process};
use std::io::{self, Write};

use recoverable_move::errors::Error;
use recoverable_move::gptman_interface::{self, GptDisk};
use recoverable_move::mover::{recover_record, continue_};
use recoverable_move::record::Record;
use recoverable_move::traits::{Disk, FileWriter, Partition};

#[inline]
fn copyright_text() {
    println!("recoverable-move");
    println!("Copyright (C) 2026 mahdoosh1");
    println!();
    println!("This program comes with ABSOLUTELY NO WARRANTY.");
    println!("This is free software, and you are welcome to redistribute it");
    println!("under certain conditions.");
    println!();
    println!("See LICENSE for the license terms.");
}

fn ask_the_user_for(message: &str) -> Result<String, String> {
    if !message.is_empty() {
        print!("{message}");
        io::stdout().flush().map_err(|e| e.to_string())?;
    }
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => Err("end of input".into()),
        Ok(_) => Ok(line.trim_end_matches(['\n', '\r']).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn error_and_exit(text: String, exit_code: i32) -> ! {
    eprintln!("{}", text);
    process::exit(exit_code)
}

fn main() {
    copyright_text();
    let mut args = env::args_os().skip(1);
    let path;
    match args.next() {
        Some(arg) => path = PathBuf::from(arg),
        None => {
            match ask_the_user_for("Disk image/device: ") {
                Ok(s) if !s.is_empty() => {
                    path = PathBuf::from(s)
                },
                _ => {
                    error_and_exit(format!("no disk image/device given"), 2);
                }
            }
        }
    }

    if args.next().is_some() {
        error_and_exit(format!("usage: recoverable-move [DISK_IMAGE_OR_DEVICE]"), 2);
    }
    if !path.exists() {
        error_and_exit(format!("no such file: {}", path.display()), 2);
    }
    match run(path) {
        Ok(_) => {},
        Err(error) => error_and_exit(format!("error: {}", error.to_string()), 1)
    }
}

fn print_help() {
    println!(
        "\
Commands:
  help       Show this help message
  list       List partitions
  move       Move a partition
  recover    Recover an interrupted operation
  exit       Exit the program
  quit       Exit the program"
    );
}

fn run(path: impl AsRef<Path>) -> Result<(), Error> {
    let mut disk = gptman_interface::create_disk(&path)?;
    let mut partitions = disk.list_partitions()?;
    print_help();
    print_partitions(&partitions);
    loop {
        partitions = disk.list_partitions()?;
        let command = ask_the_user_for("> ");
        if command.is_err() {
            println!("{}", command.unwrap_err());
            break
        }
        match command.ok().unwrap_or_default().as_str() {
            "help"    => print_help(),
            "list"    => print_partitions(&partitions),
            "move"    => move_partition(&path)?,
            "recover" => recover(&path)?,
            "exit" | "quit" => break,
            _ => println!("invalid command"),
        }
    }
    Ok(())
}

fn ask_the_user_for_integer<T: FromStr<Err = std::num::ParseIntError>>(text: &'static str) -> Result<T, Error> {
    ask_the_user_for(text)
        .map_err(|text| Error::invalid(text))?
        .parse()
        .map_err(|error: std::num::ParseIntError| Error::invalid(error.to_string()))
}
fn ask_the_user_for_string(text: &'static str) -> Result<String, Error> {
    ask_the_user_for(text)
        .map_err(|text| Error::invalid(text))
}

fn move_partition(path: impl AsRef<Path>) -> Result<(), Error>{
    let index: usize = ask_the_user_for_integer("Partition index: ")?;
    let mut disk = GptDisk::open(path)?;
    let current = *disk.list_partitions()?.get_mut(index - 1).ok_or(Error::invalid("no partition at this index"))?;
    let old_start = current.start();
    let len = current.len();
    let old_end = current.end();
    println!("partition {index}: LBA {old_start}..={old_end} ({len} sectors)");
    let new_start: u64 = ask_the_user_for_integer("New start LBA: ")?;
    let displacement = (i128::from(new_start) - i128::from(old_start)) as i64;
    if displacement == 0 {
        return Err(Error::invalid("displacement is zero (partition already there)"));
    }
    let displacement = displacement;
    let max_chunk = displacement.unsigned_abs() as u64;
    let new_end = new_start
        .checked_add(len - 1)
        .ok_or_else(|| Error::invalid("new partition end overflows".to_string()))?;
    println!("Default chunk (sectors) = {max_chunk}");
    let chunk = ask_the_user_for_integer::<u64>("Chunk (sectors, blank = default): ")
        .unwrap_or(max_chunk);
    if chunk == 0 || chunk > max_chunk {
        return Err(Error::invalid(format!("chunk must be in 1..={max_chunk} sectors")));
    }
    let subchunk: u64 = ask_the_user_for_integer("Subchunk (sectors): ")?;
    if subchunk == 0 || subchunk > chunk {
        return Err(Error::invalid(format!("subchunk must be in 1..={chunk} sectors")));
    }
    let store_path = ask_the_user_for_string("Metadata mirror path (blank = none): ")?;
    let store_path = if store_path.is_empty() {
        None
    } else {
        Some(PathBuf::from(store_path))
    };

    let chunks = len.div_ceil(chunk);
    println!("\nabout to move partition {index}:");
    println!("  from LBA {old_start}..={old_end}");
    println!("  to   LBA {new_start}..={new_end} (displacement {displacement:+})");
    println!("  chunk    {chunk} sectors → {chunks} chunk(s)");
    println!("  subchunk {subchunk} sectors");
    println!(
        "  metadata mirror: {}",
        store_path
            .as_ref()
            .map_or_else(|| "(none)".into(), |p| p.display().to_string())
    );
    if store_path.is_none() {
        eprintln!("warning: no metadata mirror; recovery relies on the on-disk record");
    }
    if ask_the_user_for_string("Proceed? This operation writes to the disk")? != "y" {
        return Err(Error::invalid("aborted by user"));
    }
    let record = Record::new(disk.sector_size() as u32, current, displacement, chunk, subchunk)?;
    let file = match store_path {
        Some(path) => Some(FileWriter::new(path)?),
        None => None
    };
    continue_(&mut disk, index - 1, record, file)?;
    println!("\nmove complete\n");
    Ok(())
}

fn recover(path: impl AsRef<Path>) -> Result<(), Error>{
    let mut disk = GptDisk::open(path)?;
    let mirror_path = ask_the_user_for_string("metadata mirror path (blank = recover from the disk): ")?;
    let mirror;
    if mirror_path.is_empty() {
        mirror = None
    } else {
        let mut data = vec![0u8; disk.sector_size()];
        FileWriter::new(mirror_path)?.read(&mut data)?;
        mirror = Some(Record::decode(&data)?)
    }
    println!("Attempting Partition recovery...");
    if ask_the_user_for_string("Proceed? This operation reads from the disk")? != "y" {
        return Err(Error::invalid("aborted by user"));
    }
    let (partition_index, record) = recover_record(&mut disk, mirror)?;
    let new_mirror: Option<String>;
    if mirror.is_none() {
        eprintln!("warning: no metadata mirror; re-recovery will have to rely on the on-disk record");
        let mirror_path = ask_the_user_for_string("Type the path to metadata mirror or blank to ignore the warning")?;
        if !mirror_path.is_empty() {
            new_mirror = Some(mirror_path.into());
        } else {
            new_mirror = None;
        }
    } else {
        let answer = ask_the_user_for_string("Metadata mirror path (blank = reuse the existing): ")?;
        if !answer.is_empty() {
            new_mirror = Some(answer.into());
        } else {
            new_mirror = None;
        }
    }
    println!("Continuing from where left off...");
    if ask_the_user_for_string("Proceed? This operation writes to the disk")? != "y" {
        return Err(Error::invalid("aborted by user"));
    }
    let file = match new_mirror {
        Some(path) => Some(FileWriter::new(path)?),
        None => None
    };
    continue_(&mut disk, partition_index, record, file)?;
    Ok(())
}

fn print_partitions(partitions: &[Partition]) {
    println!(
        "{:<4} {:<36} {:>12} {:>12} {:>12} {}",
        "Idx", "GUID", "Start", "End", "Size", "Label"
    );

    for (i, partition) in partitions.iter().enumerate() {
        let guid = uuid::Uuid::from_slice_le(partition.guid()).unwrap();

        println!(
            "{:<4} {:<36} {:>12} {:>12} {:>12} {}",
            i + 1,
            guid,
            partition.start(),
            partition.end(),
            partition.len(),
            partition.label_string()
        );
    }
}