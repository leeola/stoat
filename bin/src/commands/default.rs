use clap::Parser;

#[derive(Parser)]
#[command(name = "stoat", about = "A modal text editor")]
pub struct Args {
    #[arg(help = "File to open")]
    pub file: Option<std::path::PathBuf>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _args = Args::parse();
    Ok(())
}
