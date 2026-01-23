use clap::Parser;
use std::path::PathBuf;
use stoat::{Action, Key, Stoat};

#[derive(Parser)]
#[command(name = "stoat", about = "A modal text editor")]
pub struct Args {
    #[arg(help = "File to open")]
    pub file: Option<PathBuf>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut stoat = Stoat::new();

    stoat.keymap(Key::char('q'), Action::Exit, |_| true);
    stoat.keymap(Key::esc(), Action::Exit, |_| true);
    stoat.keymap(Key::char('j'), Action::ScrollDown(1), |_| true);
    stoat.keymap(Key::char('k'), Action::ScrollUp(1), |_| true);
    stoat.keymap(Key::char('d').ctrl(), Action::PageDown, |_| true);
    stoat.keymap(Key::char('u').ctrl(), Action::PageUp, |_| true);

    if let Some(path) = args.file {
        stoat.open_file(path)?;
    } else if let Some(files) = stoat::git::modified_files(&std::env::current_dir()?) {
        for file in files {
            stoat.open_file(file)?;
        }
    }

    Ok(stoat::run(stoat)?)
}
