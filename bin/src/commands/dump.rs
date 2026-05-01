use clap::Subcommand;
use snafu::{whatever, ResultExt, Whatever};
use std::{env, process::Command, sync::Arc};
use stoat::{
    dump::{self, DumpEntry},
    host::LocalFs,
};
use stoat_scheduler::TestScheduler;
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

pub fn run(sub: DumpCommand) -> Result<(), Whatever> {
    match sub {
        DumpCommand::Ls => ls(),
        DumpCommand::Open { id } => open(&id),
        DumpCommand::Rm { id } => rm(&id),
        DumpCommand::Clean { older_than } => clean(older_than),
    }
}

fn ls() -> Result<(), Whatever> {
    let entries = dump::list(&LocalFs).whatever_context("list workspace dumps")?;
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

fn open(query: &str) -> Result<(), Whatever> {
    let entry = dump::resolve(query, &LocalFs).whatever_context("resolve dump id")?;
    let tempdir = TempBuilder::new()
        .prefix("stoat-dump-")
        .tempdir()
        .whatever_context("create tempdir for dump extract")?;
    dump::extract(&entry.id, tempdir.path(), &LocalFs).whatever_context("extract dump")?;

    let current_exe = env::current_exe().whatever_context("locate current stoat binary")?;
    let status = Command::new(&current_exe)
        .current_dir(tempdir.path())
        .env(
            "STOAT_DUMP_LOAD",
            tempdir.path().join(".stoat").join("dump.ron"),
        )
        .status()
        .whatever_context("spawn nested stoat process")?;

    // TempDir drops here -> cleaned up.
    if !status.success() {
        whatever!("stoat exited with status {status}");
    }
    Ok(())
}

fn rm(query: &str) -> Result<(), Whatever> {
    let entry = dump::resolve(query, &LocalFs).whatever_context("resolve dump id")?;
    dump::remove(&entry.id, &LocalFs).whatever_context("remove dump")?;
    println!("Removed {}", entry.id);
    Ok(())
}

fn clean(older_than_days: u64) -> Result<(), Whatever> {
    // FIXME: Replace TestScheduler with a production scheduler
    let scheduler = Arc::new(TestScheduler::new());
    let executor = scheduler.executor();
    let removed = dump::clean_older_than(older_than_days, &LocalFs, &executor)
        .whatever_context("clean stale dumps")?;
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
