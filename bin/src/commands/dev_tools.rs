use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use stoat::dev_tools::git;

pub fn run_git_list() -> Result<(), Box<dyn std::error::Error>> {
    let fixtures_dir = fixtures_dir();
    let scenarios = git::list_scenarios(&fixtures_dir);
    if scenarios.is_empty() {
        eprintln!("No scenarios found in {}", fixtures_dir.display());
    } else {
        for name in &scenarios {
            println!("{name}");
        }
    }
    Ok(())
}

pub fn run_git_open(
    scenario: &str,
    base_temp_dir: Option<&Path>,
    persist: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture_dir = fixtures_dir().join(scenario);
    if !fixture_dir.is_dir() {
        return Err(format!("scenario not found: {}", fixture_dir.display()).into());
    }

    let repo = git::create_test_repo(&fixture_dir, base_temp_dir, persist)?;

    if persist {
        let _ = writeln!(std::io::stderr(), "Fixture dir: {}", repo.dir.display());
    }

    let canonical_dir = fs::canonicalize(&repo.dir)?;
    std::env::set_current_dir(&canonical_dir)?;
    let paths = repo.changed_files.clone();

    #[cfg(debug_assertions)]
    {
        stoat::app::run_with_paths(None, None, None, paths)
    }
    #[cfg(not(debug_assertions))]
    {
        stoat::app::run_with_paths(None, None, paths)
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stoat/fixtures/git")
}
