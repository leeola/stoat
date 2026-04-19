use clap::Subcommand;
use std::{env, io, process::Command};
use stoat::dump::{self, DumpEntry, DumpError};
use tempfile::Builder as TempBuilder;

#[derive(Subcommand, Debug)]
pub enum DumpCommand {
    /// List available dumps, newest first.
    Ls,
    /// Extract a dump into a fresh tempdir and open stoat rooted there.
    /// The tempdir is removed when stoat exits.
    Open {
        /// Full dump id (`<timestamp>_<name>`) or the name suffix alone.
        id: String,
    },
    /// Remove a specific dump.
    Rm {
        /// Full dump id or the name suffix alone.
        id: String,
    },
    /// Remove every dump older than the given number of whole days.
    Clean {
        #[arg(long, default_value_t = 30)]
        older_than: u64,
    },
}

pub fn run(sub: DumpCommand) -> Result<(), Box<dyn std::error::Error>> {
    match sub {
        DumpCommand::Ls => ls(),
        DumpCommand::Open { id } => open(&id),
        DumpCommand::Rm { id } => rm(&id),
        DumpCommand::Clean { older_than } => clean(older_than),
    }
}

fn ls() -> Result<(), Box<dyn std::error::Error>> {
    let entries = dump::list()?;
    if entries.is_empty() {
        println!("No dumps found under $XDG_DATA_HOME/stoat/dumps/.");
        return Ok(());
    }
    let id_width = entries
        .iter()
        .map(|e| e.id.as_str().len())
        .max()
        .unwrap_or(0);
    println!(
        "{:<id_width$}  {:<20}  {:>9}",
        "ID",
        "DATE (UTC)",
        "SIZE",
        id_width = id_width.max(2),
    );
    for entry in &entries {
        println!(
            "{:<id_width$}  {:<20}  {:>9}",
            entry.id.as_str(),
            format_date(entry),
            human_size(entry.size_bytes),
            id_width = id_width.max(2),
        );
    }
    Ok(())
}

fn format_date(entry: &DumpEntry) -> String {
    entry
        .id
        .created_at()
        .and_then(|t| {
            let fmt =
                time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
            t.format(&fmt).ok()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn open(query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let entry = dump::resolve(query).map_err(describe)?;
    let tempdir = TempBuilder::new().prefix("stoat-dump-").tempdir()?;
    dump::extract(&entry.id, tempdir.path()).map_err(describe)?;

    let current_exe = env::current_exe()?;
    let status = Command::new(&current_exe)
        .current_dir(tempdir.path())
        .env(
            "STOAT_DUMP_LOAD",
            tempdir.path().join(".stoat").join("dump.ron"),
        )
        .status()?;

    // TempDir drops here -> cleaned up.
    if !status.success() {
        return Err(io::Error::other(format!("stoat exited with status {status}")).into());
    }
    Ok(())
}

fn rm(query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let entry = dump::resolve(query).map_err(describe)?;
    dump::remove(&entry.id).map_err(describe)?;
    println!("Removed {}", entry.id);
    Ok(())
}

fn clean(older_than_days: u64) -> Result<(), Box<dyn std::error::Error>> {
    let removed = dump::clean_older_than(older_than_days).map_err(describe)?;
    if removed.is_empty() {
        println!("No dumps older than {older_than_days} days.");
    } else {
        for id in &removed {
            println!("Removed {id}");
        }
        println!("{} dumps removed.", removed.len());
    }
    Ok(())
}

/// Preserve the structured message of a [`DumpError`] across the
/// `Box<dyn Error>` boundary. Without this the top-level formatter on
/// [`DumpError`] is hidden behind `Debug`.
fn describe(err: DumpError) -> io::Error {
    io::Error::other(err.to_string())
}
