use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let hash = capture("git", &["rev-parse", "--short", "HEAD"]);
    let title = capture("git", &["log", "-1", "--pretty=%s"]);
    let dirty = capture_dirty();
    let date = capture("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]);

    println!("cargo:rustc-env=STOAT_GIT_HASH={hash}");
    println!("cargo:rustc-env=STOAT_GIT_TITLE={title}");
    println!("cargo:rustc-env=STOAT_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=STOAT_BUILD_DATE={date}");
}

fn capture(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        },
        _ => "unknown".to_string(),
    }
}

fn capture_dirty() -> &'static str {
    match Command::new("git").args(["status", "--porcelain"]).output() {
        Ok(output) if output.status.success() => {
            if output.stdout.is_empty() {
                "clean"
            } else {
                "dirty"
            }
        },
        _ => "unknown",
    }
}
