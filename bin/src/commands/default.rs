use clap::Parser;
use ratatui::DefaultTerminal;
use stoat::Stoat;

#[derive(Parser)]
#[command(name = "stoat", about = "A modal text editor")]
pub struct Args {
    #[arg(help = "File to open")]
    pub file: Option<std::path::PathBuf>,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _args = Args::parse();
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run_app(terminal: &mut DefaultTerminal) -> Result<(), Box<dyn std::error::Error>> {
    let mut stoat = Stoat::new();
    while stoat.draw().await? {
        terminal.draw(|f| stoat.render(f))?;
    }
    Ok(())
}
