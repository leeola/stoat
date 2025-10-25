use std::process::Command;

fn main() {
    // Capture git commit hash at build time
    let commit_hash = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Check if working tree is dirty (has uncommitted changes)
    let is_dirty = Command::new("git")
        .args(["diff", "--quiet"])
        .status()
        .map(|status| !status.success())
        .unwrap_or(false)
        || Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .status()
            .map(|status| !status.success())
            .unwrap_or(false);

    // Set environment variables for use in the binary
    println!("cargo:rustc-env=STOAT_COMMIT_HASH={commit_hash}");
    println!("cargo:rustc-env=STOAT_COMMIT_DIRTY={is_dirty}");

    // Re-run build script if .git/HEAD changes (branch switch)
    println!("cargo:rerun-if-changed=../.git/HEAD");
    // Re-run if git index changes (new commits)
    println!("cargo:rerun-if-changed=../.git/index");
}
