use gpui::{App, AppContext, Entity};
use std::num::NonZeroU64;
use stoat_rope_v3::{TokenMap, TokenSnapshot};
use text::{Buffer, BufferId, BufferSnapshot};

#[derive(Clone)]
pub struct Stoat {
    buffer: Entity<Buffer>,
    token_map: TokenMap,
}

impl Stoat {
    pub fn new(cx: &mut App) -> Self {
        let buffer_id = BufferId::from(NonZeroU64::new(1).unwrap());
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        let buffer_snapshot = buffer.read(cx).snapshot();
        let token_map = TokenMap::new(&buffer_snapshot);

        Self { buffer, token_map }
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.buffer.read(cx).snapshot()
    }

    pub fn token_snapshot(&self) -> TokenSnapshot {
        self.token_map.snapshot()
    }

    pub fn load_files(&mut self, paths: &[&std::path::Path], cx: &mut App) {
        // Load first file into buffer
        if let Some(first_path) = paths.first() {
            if let Ok(contents) = std::fs::read_to_string(first_path) {
                self.buffer.update(cx, |buffer, _| {
                    // Clear existing content and insert new
                    let len = buffer.len();
                    buffer.edit([(0..len, contents.as_str())]);
                });

                // Sync token map with new buffer content
                let buffer_snapshot = self.buffer.read(cx).snapshot();
                // For initial load, we can simulate an edit of the entire buffer
                let edit = text::Edit {
                    old: 0..0,
                    new: 0..buffer_snapshot.len(),
                };
                self.token_map.sync(&buffer_snapshot, &[edit]);
            }
        }
    }

    pub fn buffer_contents(&self, cx: &App) -> String {
        self.buffer.read(cx).text()
    }
}

pub struct EditorEngine;

impl EditorEngine {
    pub fn new() -> Self {
        EditorEngine
    }
}

pub mod cli {
    pub mod config {
        use clap::Parser;

        #[derive(Parser)]
        #[command(name = "stoat")]
        #[command(about = "A text editor", long_about = None)]
        pub struct Cli {
            #[command(subcommand)]
            pub command: Option<Command>,
        }

        #[derive(Parser)]
        pub enum Command {
            #[cfg(feature = "gui")]
            #[command(about = "Launch the graphical user interface")]
            Gui {
                #[arg(help = "Files to open")]
                paths: Vec<std::path::PathBuf>,

                #[arg(short, long, help = "Input sequence to execute")]
                input: Option<String>,
            },
        }
    }
}

pub mod log {
    pub fn init() -> Result<(), Box<dyn std::error::Error>> {
        // FIXME: Stub implementation - will set up proper logging later
        Ok(())
    }
}
