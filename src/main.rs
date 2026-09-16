//! Interactive CLI for the recoverable-move library.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process;

use recoverable_move::disk::FileDisk;
use recoverable_move::gpt;
use recoverable_move::mover::{self, FileStore, MoveParams, NullStore};
use recoverable_move::traits::{Disk, MetaStore};

fn main() {
    let args: Vec<String> = env::args().collect();

    let path = if args.len() >= 2 {
        PathBuf::from(&args[1])
    } else {
        match prompt_line("Disk image: ") {
            Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
            _ => {
                eprintln!("no image given");
                process::exit(2);
            }
        }
    };

    if !path.exists() {
        eprintln!("no such file: {}", path.display());
        process::exit(2);
    }

    if let Err(e) = repl(&path) {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn repl(path: &Path) -> Result<(), String> {
    println!("recoverable-move — interactive GPT tool");
    println!("image: {}", path.display());
    println!();
    print_repl_help();
    println!();

    if let Err(e) = cmd_list(path) {
        eprintln!("warning: {e}");
    }

    loop {
        print!("\n> ");
        io::stdout().flush().ok();

        let line = match prompt_line("") {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let head = parts.next().unwrap_or("");

        let result = match head {
            "l" | "list" => cmd_list(path),
            "v" | "verify" => cmd_verify(path),
            "m" | "move" => cmd_move(path),
            "r" | "recover" => cmd_recover(path),
            #[cfg(debug_assertions)]
            "s" | "set" => cmd_set(path),
            "h" | "help" | "?" => {
                print_repl_help();
                Ok(())
            }
            "q" | "quit" | "exit" => break,
            other => {
                eprintln!("unknown command `{other}` (try `h`)");
                Ok(())
            }
        };

        if let Err(e) = result {
            eprintln!("error: {e}");
        }
    }

    Ok(())
}

fn print_repl_help() {
    println!("commands:");
    println!("  l, list     show partition table");
    println!("  v, verify   verify GPT CRCs (header + entry array)");
    println!("  m, move     move a partition (interactive)");
    println!("  r, recover  resume an interrupted move");
    #[cfg(debug_assertions)]
    println!("  s, set      rewrite a partition entry (debug builds only)");
    println!("  h, help     this message");
    println!("  q, quit     exit");
}

fn cmd_list(path: &Path) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let g = gpt::Gpt::read(&mut f)
        .map_err(|e| format!("read GPT from {}: {e}", path.display()))?;
    print_table(&g);
    Ok(())
}

fn cmd_verify(path: &Path) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let g = gpt::Gpt::read(&mut f)
        .map_err(|e| format!("read GPT from {}: {e}", path.display()))?;
    g.verify().map_err(|e| format!("CRC verification failed: {e}"))?;
    println!("CRC32 verification: OK");
    Ok(())
}

fn cmd_move(path: &Path) -> Result<(), String> {
    cmd_list(path)?;
    println!();

    let idx: u32 = prompt_required("Partition index: ")?;

    let mut probe = FileDisk::open(path, idx).map_err(|e| format!("{e}"))?;
    let current = probe.get_partition().map_err(|e| format!("{e}"))?;
    drop(probe);

    let old_start = current.start;
    let len = current.len;
    let old_end = old_start + len - 1;
    println!("partition {idx}: LBA {old_start}..={old_end} ({len} sectors)");

    let new_start: u64 = prompt_required("New start LBA: ")?;
    let displacement = new_start as i64 - old_start as i64;
    if displacement == 0 {
        return Err("displacement is zero (partition already there)".into());
    }
    let max_chunk = displacement.unsigned_abs();
    let new_end = new_start + len - 1;

    let subchunk: u64 = prompt_required("Subchunk (sectors): ")?;
    if subchunk == 0 || subchunk > max_chunk {
        return Err(format!(
            "subchunk must be in 1..={max_chunk} sectors (max is abs(displacement))"
        ));
    }

    let chunk: u64 = prompt_optional::<u64>(&format!(
        "Chunk (sectors, blank = {max_chunk}): "
    ))?
    .unwrap_or(max_chunk);
    if chunk == 0 || chunk > max_chunk {
        return Err(format!("chunk must be in 1..={max_chunk} sectors"));
    }
    if subchunk > chunk {
        return Err(format!("subchunk ({subchunk}) exceeds chunk ({chunk})"));
    }

    let store_input = prompt_line("Store path (blank = none): ")?;
    let store_path: Option<PathBuf> = if store_input.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(store_input.trim()))
    };

    let chunks_total = (len + chunk - 1) / chunk;
    let checkpoints = (chunk + subchunk - 1) / subchunk;

    println!();
    println!("about to move partition {idx}:");
    println!("  from LBA {old_start}..={old_end}");
    println!("  to   LBA {new_start}..={new_end}  (displacement {displacement:+})");
    println!("  chunk {chunk} sectors → {chunks_total} chunk(s)");
    println!("  subchunk {subchunk} sectors → ~{checkpoints} checkpoint(s) per chunk");

    match &store_path {
        Some(p) => println!("  metadata mirror: {}", p.display()),
        None => println!("  metadata mirror: (none)"),
    }
    println!();

    let mut store: Box<dyn MetaStore> = match store_path {
        Some(p) => Box::new(FileStore::new(p)),
        None => {
            eprintln!("warning: no metadata mirror.");
            eprintln!("         If the process is killed during the resize phase,");
            eprintln!("         the move cannot be recovered automatically.");
            eprintln!();
            if !confirm("continue without a metadata mirror?") {
                return Err("aborted by user".into());
            }
            Box::new(NullStore)
        }
    };

    if !confirm("proceed?") {
        return Err("aborted by user".into());
    }

    let mut disk = FileDisk::open(path, idx).map_err(|e| format!("{e}"))?;
    let params = MoveParams {
        old_start,
        len,
        new_start,
        chunk_size: chunk,
        subchunk_size: subchunk,
    };
    mover::run(&mut disk, Some(params), store.as_mut())
        .map_err(|e| format!("move failed: {e}"))?;

    println!();
    println!("move complete");
    println!();
    cmd_list(path)?;
    Ok(())
}

fn cmd_recover(path: &Path) -> Result<(), String> {
    println!("Looking for an interrupted move on this image.");
    println!();
    cmd_list(path)?;
    println!();

    let idx: u32 = prompt_required("Partition index: ")?;

    let store_input = prompt_line(
        "Store path (blank = rely on on-disk metadata only): ",
    )?;
    let mut store: Box<dyn MetaStore> = if store_input.trim().is_empty() {
        Box::new(NullStore)
    } else {
        Box::new(FileStore::new(PathBuf::from(store_input.trim())))
    };

    let mut disk = FileDisk::open(path, idx).map_err(|e| format!("{e}"))?;

    match mover::run(&mut disk, None, store.as_mut()) {
        Ok(()) => {
            println!();
            println!("recovery complete");
            println!();
            cmd_list(path)?;
            Ok(())
        }
        Err(mover::Error::NoTransaction) => Err(format!(
            "no interrupted move found on partition {idx} \
             (checked the on-disk metadata sector and the store)"
        )),
        Err(e) => Err(format!("recovery failed: {e}")),
    }
}

#[cfg(debug_assertions)]
fn cmd_set(path: &Path) -> Result<(), String> {
    cmd_list(path)?;
    println!();
    eprintln!("warning: `set` rewrites the GPT entry without touching the data.");
    eprintln!("         The filesystem will not be where the table says it is.");
    eprintln!();
    if !confirm("continue?") {
        return Err("aborted by user".into());
    }

    let idx: u32 = prompt_required("Partition index: ")?;
    let start: u64 = prompt_required("New start LBA: ")?;
    let end: u64 = prompt_required("New end LBA: ")?;

    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut g = gpt::Gpt::read(&mut f)
        .map_err(|e| format!("read GPT: {e}"))?;
    g.set_partition_lbas(idx, start, end).map_err(|e| format!("{e}"))?;
    g.write_to(&mut f).map_err(|e| format!("write GPT: {e}"))?;

    println!("partition {idx}: LBA {start}..={end}");
    cmd_list(path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt helpers
// ---------------------------------------------------------------------------

fn prompt_line(msg: &str) -> Result<String, String> {
    if !msg.is_empty() {
        print!("{msg}");
        io::stdout().flush().map_err(|e| e.to_string())?;
    }
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) => Err("EOF".into()),
        Ok(_) => Ok(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn prompt_required<T: std::str::FromStr>(msg: &str) -> Result<T, String> {
    loop {
        let s = prompt_line(msg)?;
        let s = s.trim();
        if s.is_empty() {
            eprintln!("  required");
            continue;
        }
        match s.parse::<T>() {
            Ok(v) => return Ok(v),
            Err(_) => {
                eprintln!("  not a valid value, try again");
                continue;
            }
        }
    }
}

fn prompt_optional<T: std::str::FromStr>(msg: &str) -> Result<Option<T>, String> {
    let s = prompt_line(msg)?;
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<T>()
        .map(Some)
        .map_err(|_| "not a valid value".into())
}

fn confirm(msg: &str) -> bool {
    print!("{msg} [y/N] ");
    let _ = io::stdout().flush();
    match prompt_line("") {
        Ok(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Table printing
// ---------------------------------------------------------------------------

fn print_table(g: &gpt::Gpt) {
    let width = terminal_width();

    println!("Disk GUID : {}", g.disk_guid_string());
    println!("Sector sz : {} bytes", g.sector_size);
    println!(
        "Usable LBA: {}..={} ({} sectors)",
        g.first_usable_lba(),
        g.last_usable_lba(),
        g.usable_lba_count()
    );
    match g.verify() {
        Ok(()) => println!("CRC32     : OK"),
        Err(e) => println!("CRC32     : FAILED ({e})"),
    }
    println!();

    const IDX_W: usize = 4;
    const START_W: usize = 11;
    const END_W: usize = 11;
    const SIZE_W: usize = 10;
    const SPACERS: usize = 5;

    const TYPE_MIN: usize = 20;
    const TYPE_MAX: usize = 34;
    const NAME_MIN: usize = 12;

    let fixed = IDX_W + START_W + END_W + SIZE_W + SPACERS;
    let flexible = width.saturating_sub(fixed);

    let (include_type, type_w, name_w) = if flexible >= TYPE_MIN + NAME_MIN {
        let t = TYPE_MAX.min(flexible - NAME_MIN);
        (true, t, flexible - t)
    } else {
        (false, 0, flexible.max(NAME_MIN.min(width)))
    };

    let mut header = String::new();
    header.push_str(&fit("Idx", IDX_W));
    header.push(' ');
    if include_type {
        header.push_str(&fit("Type", type_w));
        header.push(' ');
    }
    header.push_str(&fit("Start", START_W));
    header.push(' ');
    header.push_str(&fit("End", END_W));
    header.push(' ');
    header.push_str(&fit("Size", SIZE_W));
    header.push(' ');
    header.push_str(&fit("Name", name_w));
    println!("{header}");
    println!("{}", "-".repeat(header.chars().count().min(width)));

    for p in &g.partitions {
        let mut row = String::new();
        row.push_str(&fit(&p.index.to_string(), IDX_W));
        row.push(' ');
        if include_type {
            row.push_str(&fit(p.type_name.unwrap_or("Unknown"), type_w));
            row.push(' ');
        }
        row.push_str(&fit(&p.starting_lba.to_string(), START_W));
        row.push(' ');
        row.push_str(&fit(&p.ending_lba.to_string(), END_W));
        row.push(' ');
        row.push_str(&fit(&p.size_in_lbas().to_string(), SIZE_W));
        row.push(' ');
        row.push_str(&fit(&p.name, name_w));
        println!("{row}");
    }
}

fn fit(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= w {
        let mut out = String::with_capacity(w);
        out.push_str(s);
        for _ in count..w {
            out.push(' ');
        }
        out
    } else {
        let mut out = String::with_capacity(w);
        for c in s.chars().take(w - 1) {
            out.push(c);
        }
        out.push('…');
        out
    }
}

fn terminal_width() -> usize {
    if let Ok(s) = env::var("COLUMNS") {
        if let Ok(n) = s.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    80
}
