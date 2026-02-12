//! Shell environment capture and direnv integration.
//!
//! GUI apps launched from macOS Dock/Spotlight inherit launchd's minimal
//! environment (bare PATH: `/usr/bin:/bin`), which means tools like
//! `rust-analyzer` installed via rustup/nix/homebrew can't be found.
//!
//! [`ProjectEnvironment`] captures the user's full shell environment for
//! a project directory, layers direnv on top, and provides it for LSP
//! process spawning.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Captured project environment for spawning LSP processes.
///
/// Wraps a shell-captured environment (with optional direnv overlay)
/// that reflects the user's actual PATH and tool configuration.
#[derive(Clone)]
pub struct ProjectEnvironment {
    vars: Arc<HashMap<String, String>>,
}

impl ProjectEnvironment {
    /// Capture environment for the given project directory.
    ///
    /// Spawns the user's login shell to get the full environment, then
    /// layers direnv on top if available. Falls back to the current
    /// process environment if shell capture fails.
    pub async fn capture(project_dir: &Path) -> Self {
        let base = match capture_shell_env(project_dir).await {
            Ok(env) => {
                tracing::debug!(
                    "Captured shell environment ({} vars, PATH={})",
                    env.len(),
                    env.get("PATH")
                        .map(|p| &p[..p.len().min(80)])
                        .unwrap_or("(unset)")
                );
                env
            },
            Err(e) => {
                tracing::warn!("Shell env capture failed, using process env: {e}");
                std::env::vars().collect()
            },
        };

        let direnv_diff = load_direnv(&base, project_dir).await;
        if !direnv_diff.is_empty() {
            tracing::debug!("direnv applied {} variable changes", direnv_diff.len());
        }

        let merged = merge_direnv(base, &direnv_diff);
        Self {
            vars: Arc::new(merged),
        }
    }

    pub fn vars(&self) -> &HashMap<String, String> {
        &self.vars
    }

    pub fn path(&self) -> Option<&str> {
        self.vars.get("PATH").map(|s| s.as_str())
    }
}

/// Spawn the user's login shell to capture the full environment as JSON.
///
/// Runs `$SHELL -l -c 'stoat cmd printenv'` in the project directory,
/// which outputs all environment variables as a JSON object.
async fn capture_shell_env(project_dir: &Path) -> anyhow::Result<HashMap<String, String>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let stoat_bin = std::env::current_exe()?;
    let stoat_path = stoat_bin
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("stoat binary path is not valid UTF-8"))?;

    // Quote the path in case it contains spaces
    let inner_cmd = format!("'{stoat_path}' cmd printenv");

    tracing::debug!("Capturing shell env: {} -l -c {:?}", shell, inner_cmd);

    let output = smol::process::Command::new(&shell)
        .args(["-l", "-c", &inner_cmd])
        .current_dir(project_dir)
        .stdout(smol::process::Stdio::piped())
        .stderr(smol::process::Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Shell exited with {}: {}",
            output.status,
            stderr.lines().next().unwrap_or("(no output)")
        );
    }

    let stdout = String::from_utf8(output.stdout)?;
    let env: HashMap<String, String> = serde_json::from_str(&stdout)?;
    Ok(env)
}

/// Run `direnv export json` to get environment variable changes.
///
/// Returns a map of variable name -> `Some(value)` for set/changed vars,
/// `None` for removed vars. Returns empty map on any failure.
async fn load_direnv(
    base_env: &HashMap<String, String>,
    project_dir: &Path,
) -> HashMap<String, Option<String>> {
    let direnv_path = match find_in_env_path("direnv", base_env) {
        Some(p) => p,
        None => return HashMap::new(),
    };

    let result = smol::process::Command::new(&direnv_path)
        .args(["export", "json"])
        .env_clear()
        .envs(base_env)
        .env("TERM", "dumb")
        .current_dir(project_dir)
        .stdout(smol::process::Stdio::piped())
        .stderr(smol::process::Stdio::piped())
        .output()
        .await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("direnv failed to run: {e}");
            return HashMap::new();
        },
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            "direnv exited with {}: {}",
            output.status,
            stderr.lines().next().unwrap_or("(no output)")
        );
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return HashMap::new();
    }

    match serde_json::from_str(trimmed) {
        Ok(diff) => diff,
        Err(e) => {
            tracing::warn!("direnv output is not valid JSON: {e}");
            HashMap::new()
        },
    }
}

/// Apply direnv diff to a base environment.
///
/// `Some(v)` entries overwrite/insert, `None` entries remove.
fn merge_direnv(
    mut base: HashMap<String, String>,
    diff: &HashMap<String, Option<String>>,
) -> HashMap<String, String> {
    for (key, value) in diff {
        match value {
            Some(v) => {
                base.insert(key.clone(), v.clone());
            },
            None => {
                base.remove(key);
            },
        }
    }
    base
}

/// Find an executable in the PATH from the given environment.
fn find_in_env_path(name: &str, env: &HashMap<String, String>) -> Option<PathBuf> {
    let path_var = env.get("PATH")?;
    which::which_in(name, Some(path_var), ".").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_direnv_inserts_and_removes() {
        let mut base = HashMap::new();
        base.insert("A".into(), "1".into());
        base.insert("B".into(), "2".into());
        base.insert("C".into(), "3".into());

        let mut diff = HashMap::new();
        diff.insert("B".into(), Some("updated".into()));
        diff.insert("C".into(), None);
        diff.insert("D".into(), Some("new".into()));

        let result = merge_direnv(base, &diff);

        assert_eq!(result.get("A").unwrap(), "1");
        assert_eq!(result.get("B").unwrap(), "updated");
        assert!(!result.contains_key("C"));
        assert_eq!(result.get("D").unwrap(), "new");
    }

    #[test]
    fn merge_direnv_empty_diff_is_identity() {
        let mut base = HashMap::new();
        base.insert("X".into(), "val".into());

        let result = merge_direnv(base.clone(), &HashMap::new());
        assert_eq!(result, base);
    }
}
