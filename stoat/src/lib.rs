pub struct Stoat {
    buffer: String,
}

impl Stoat {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    pub fn buffer_contents(&self) -> &str {
        &self.buffer
    }

    pub fn load_files(&mut self, paths: &[&std::path::Path]) {
        // FIXME: Stub implementation - will load file contents later
        if let Some(first_path) = paths.first() {
            if let Ok(contents) = std::fs::read_to_string(first_path) {
                self.buffer = contents;
            }
        }
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
