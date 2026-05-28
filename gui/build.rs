#![allow(clippy::disallowed_types)]

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let hash = capture("git", &["rev-parse", "--short", "HEAD"]);
    let dirty_suffix = if is_dirty() { "-dirty" } else { "" };
    let date = capture("date", &["-u", "+%Y-%m-%d"]);

    println!("cargo:rustc-env=STOAT_BUILD_INFO={hash}{dirty_suffix} {date}");
}

fn capture(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        },
        _ => "unknown".to_string(),
    }
}

fn is_dirty() -> bool {
    match Command::new("git").args(["status", "--porcelain"]).output() {
        Ok(output) if output.status.success() => !output.stdout.is_empty(),
        _ => false,
    }
}
