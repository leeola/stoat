//! Settings loader + file watcher for `.claude/settings.json`.
//!
//! Merges the four standard Claude Code settings files with increasing
//! precedence:
//!
//! 1. User:    `~/.claude/settings.json`
//! 2. Project: `<cwd>/.claude/settings.json`
//! 3. Local:   `<cwd>/.claude/settings.local.json`
//! 4. Managed: platform-specific enterprise override
//!
//! Later sources win per-field via a shallow merge. File watching is
//! opt-in via [`SettingsManager::spawn_watcher`]; one-shot loading via
//! [`load_merged_settings`] does not need a background task.

use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::warn;

/// Shape of a `.claude/settings.json` file. Fields are all optional
/// because individual settings files frequently cover only a subset.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Preserve any fields this crate doesn't model explicitly so
    /// subsequent serialisation round-trips cleanly.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub other: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,
}

impl ClaudeCodeSettings {
    /// Merge `other` into `self`, with `other`'s fields winning on
    /// overlap. Used to build the effective settings across the four
    /// source files.
    pub fn merge(&mut self, other: ClaudeCodeSettings) {
        if other.permissions.is_some() {
            self.permissions = other.permissions;
        }
        if let Some(env) = other.env {
            self.env
                .get_or_insert_with(HashMap::new)
                .extend(env.into_iter());
        }
        if other.model.is_some() {
            self.model = other.model;
        }
        self.other.extend(other.other);
    }
}

/// Paths consulted when loading settings. Exposed so
/// [`SettingsManager::spawn_watcher`] can register them with `notify`.
#[derive(Debug, Clone)]
pub struct SettingsPaths {
    pub user: Option<PathBuf>,
    pub project: PathBuf,
    pub local: PathBuf,
    pub managed: Option<PathBuf>,
}

impl SettingsPaths {
    pub fn for_cwd(cwd: &Path) -> Self {
        let user = home_dir().map(|h| h.join(".claude").join("settings.json"));
        let project = cwd.join(".claude").join("settings.json");
        let local = cwd.join(".claude").join("settings.local.json");
        let managed = managed_settings_path();
        Self {
            user,
            project,
            local,
            managed,
        }
    }

    pub fn all(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = Vec::new();
        if let Some(u) = &self.user {
            v.push(u);
        }
        v.push(&self.project);
        v.push(&self.local);
        if let Some(m) = &self.managed {
            v.push(m);
        }
        v
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn managed_settings_path() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(
            "/Library/Application Support/ClaudeCode/managed-settings.json",
        ))
    } else if cfg!(target_os = "linux") {
        Some(PathBuf::from("/etc/claude-code/managed-settings.json"))
    } else if cfg!(target_os = "windows") {
        Some(PathBuf::from(
            r"C:\Program Files\ClaudeCode\managed-settings.json",
        ))
    } else {
        None
    }
}

/// One-shot: read and merge the four settings files for the given cwd.
/// Missing files are skipped silently; malformed files are logged at
/// warn level and also skipped.
pub fn load_merged_settings(cwd: &Path) -> ClaudeCodeSettings {
    let paths = SettingsPaths::for_cwd(cwd);
    let mut merged = ClaudeCodeSettings::default();
    for path in paths.all() {
        if let Some(loaded) = load_one(path) {
            merged.merge(loaded);
        }
    }
    merged
}

/// Load only the managed (enterprise) settings file. Returns `None`
/// when the file is absent or unreadable.
pub fn load_managed_settings() -> Option<ClaudeCodeSettings> {
    let path = managed_settings_path()?;
    load_one(&path)
}

fn load_one(path: &Path) -> Option<ClaudeCodeSettings> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<ClaudeCodeSettings>(&bytes) {
        Ok(settings) => Some(settings),
        Err(err) => {
            warn!(
                "ClaudeCodeSettings: failed to parse {}: {}",
                path.display(),
                err
            );
            None
        },
    }
}

/// Apply the `env` map from a settings blob to `std::env`. The caller
/// must ensure this runs before threads observe the environment; in
/// Rust 2024 `std::env::set_var` is marked `unsafe` for that reason.
///
/// # Safety
///
/// Mutating `std::env` is not thread-safe. Call this only from a
/// single-threaded startup path.
pub unsafe fn apply_environment_settings(settings: &ClaudeCodeSettings) {
    if let Some(env) = &settings.env {
        for (key, value) in env {
            // SAFETY: documented on the outer fn.
            unsafe {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Resolve a permission-mode string (from settings or user input) into
/// the CLI's canonical camelCase value. Accepts the CLI-facing aliases
/// (`auto`, `default`, `acceptedits`, `dontask`, `plan`,
/// `bypasspermissions`, `bypass`).
pub fn resolve_permission_mode(input: Option<&str>) -> Result<&'static str, String> {
    let Some(raw) = input else {
        return Ok("default");
    };
    let normalized = raw.trim().to_lowercase();
    match normalized.as_str() {
        "" => Err("Invalid permissions.defaultMode: expected a non-empty string.".into()),
        "auto" => Ok("auto"),
        "default" => Ok("default"),
        "acceptedits" | "accept-edits" | "acceptEdits" | "accept_edits" => Ok("acceptEdits"),
        "dontask" | "dont-ask" | "dontAsk" | "dont_ask" => Ok("dontAsk"),
        "plan" => Ok("plan"),
        "bypasspermissions" | "bypass-permissions" | "bypass" => Ok("bypassPermissions"),
        other => Err(format!("Invalid permissions.defaultMode: {other}.")),
    }
}

/// File watcher that debounces change events and re-merges settings
/// whenever any tracked file changes.
pub struct SettingsManager {
    cwd: PathBuf,
    current: ClaudeCodeSettings,
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    changes: mpsc::UnboundedReceiver<ClaudeCodeSettings>,
}

impl SettingsManager {
    /// Spawn a watcher for the given cwd. The returned manager yields
    /// updated snapshots on `changes()` whenever any of the four
    /// settings files change, with a 100ms debounce.
    pub fn spawn_watcher(cwd: PathBuf) -> io::Result<Self> {
        let paths = SettingsPaths::for_cwd(&cwd);
        let current = load_merged_settings(&cwd);
        let (tx, rx) = mpsc::unbounded_channel::<ClaudeCodeSettings>();
        let watch_cwd = cwd.clone();

        let mut debouncer = new_debouncer(
            Duration::from_millis(100),
            move |res: DebounceEventResult| {
                if matches!(&res, Ok(events) if events.iter().any(is_content_change)) {
                    let merged = load_merged_settings(&watch_cwd);
                    let _ = tx.send(merged);
                }
            },
        )
        .map_err(|e| io::Error::other(format!("failed to construct settings watcher: {e}")))?;

        for path in paths.all() {
            // Watch the parent directory rather than the file itself
            // so editors that write via rename (e.g. vim's backup-and-
            // rename) still trigger events.
            if let Some(parent) = path.parent()
                && parent.exists()
            {
                let _ = debouncer
                    .watcher()
                    .watch(parent, RecursiveMode::NonRecursive);
            }
        }

        Ok(Self {
            cwd,
            current,
            _debouncer: debouncer,
            changes: rx,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Snapshot of the latest merged settings.
    pub fn current(&self) -> ClaudeCodeSettings {
        self.current.clone()
    }

    /// Receive the next change notification. Returns `None` when the
    /// watcher task has exited (debouncer dropped).
    pub async fn next_change(&mut self) -> Option<ClaudeCodeSettings> {
        let next = self.changes.recv().await?;
        self.current = next.clone();
        Some(next)
    }
}

/// `notify-debouncer-mini` collapses filesystem events into a single
/// `DebouncedEvent` and doesn't expose the underlying kind. Treat
/// every debounced event we receive as a content change.
fn is_content_change(event: &notify_debouncer_mini::DebouncedEvent) -> bool {
    matches!(
        event.kind,
        notify_debouncer_mini::DebouncedEventKind::Any
            | notify_debouncer_mini::DebouncedEventKind::AnyContinuous
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_later_fields() {
        let mut a = ClaudeCodeSettings {
            model: Some("m1".into()),
            ..Default::default()
        };
        let b = ClaudeCodeSettings {
            model: Some("m2".into()),
            ..Default::default()
        };
        a.merge(b);
        assert_eq!(a.model.as_deref(), Some("m2"));
    }

    #[test]
    fn merge_unions_env_keys() {
        let mut a = ClaudeCodeSettings {
            env: Some(HashMap::from([("X".into(), "1".into())])),
            ..Default::default()
        };
        let b = ClaudeCodeSettings {
            env: Some(HashMap::from([("Y".into(), "2".into())])),
            ..Default::default()
        };
        a.merge(b);
        let env = a.env.unwrap();
        assert_eq!(env.get("X").map(String::as_str), Some("1"));
        assert_eq!(env.get("Y").map(String::as_str), Some("2"));
    }

    #[test]
    fn load_merged_reads_files_and_merges() {
        let temp = tempfile::tempdir().unwrap();
        let claude_dir = temp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{"model": "sonnet", "permissions": {"defaultMode": "default"}}"#,
        )
        .unwrap();
        std::fs::write(
            claude_dir.join("settings.local.json"),
            r#"{"model": "opus"}"#,
        )
        .unwrap();
        let merged = load_merged_settings(temp.path());
        assert_eq!(merged.model.as_deref(), Some("opus"));
        assert_eq!(
            merged
                .permissions
                .as_ref()
                .and_then(|p| p.default_mode.as_deref()),
            Some("default")
        );
    }

    #[test]
    fn resolve_permission_mode_accepts_aliases() {
        assert_eq!(
            resolve_permission_mode(Some("acceptedits")).unwrap(),
            "acceptEdits"
        );
        assert_eq!(
            resolve_permission_mode(Some("bypass")).unwrap(),
            "bypassPermissions"
        );
        assert_eq!(resolve_permission_mode(Some("DONTASK")).unwrap(), "dontAsk");
        assert_eq!(resolve_permission_mode(None).unwrap(), "default");
        assert!(resolve_permission_mode(Some("garbage")).is_err());
    }
}
