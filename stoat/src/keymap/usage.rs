use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub struct UsageTracker {
    counts: HashMap<String, u32>,
    threshold: u32,
    invocations_since_save: u32,
}

impl UsageTracker {
    pub fn new(threshold: u32) -> Self {
        Self {
            counts: HashMap::new(),
            threshold,
            invocations_since_save: 0,
        }
    }

    pub fn record(&mut self, mode: &str, action: &str) {
        let key = format!("{mode}:{action}");
        *self.counts.entry(key).or_insert(0) += 1;
        self.invocations_since_save += 1;
    }

    pub fn should_hide(&self, mode: &str, action: &str) -> bool {
        let key = format!("{mode}:{action}");
        self.counts.get(&key).copied().unwrap_or(0) >= self.threshold
    }

    pub fn should_save(&self) -> bool {
        self.invocations_since_save >= 50
    }

    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("stoat/usage.json")
    }

    pub fn load(path: &Path) -> Self {
        let counts = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            counts,
            threshold: 10,
            invocations_since_save: 0,
        }
    }

    pub fn save(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&self.counts) {
            let _ = std::fs::write(path, json);
        }
        self.invocations_since_save = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_hide() {
        let mut tracker = UsageTracker::new(3);
        assert!(!tracker.should_hide("space", "OpenFileFinder"));

        tracker.record("space", "OpenFileFinder");
        tracker.record("space", "OpenFileFinder");
        assert!(!tracker.should_hide("space", "OpenFileFinder"));

        tracker.record("space", "OpenFileFinder");
        assert!(tracker.should_hide("space", "OpenFileFinder"));
    }

    #[test]
    fn different_modes_tracked_separately() {
        let mut tracker = UsageTracker::new(2);
        tracker.record("space", "Save");
        tracker.record("space", "Save");
        assert!(tracker.should_hide("space", "Save"));
        assert!(!tracker.should_hide("goto", "Save"));
    }

    #[test]
    fn save_threshold() {
        let mut tracker = UsageTracker::new(10);
        assert!(!tracker.should_save());
        for _ in 0..50 {
            tracker.record("m", "a");
        }
        assert!(tracker.should_save());
    }

    #[test]
    fn roundtrip_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");

        let mut tracker = UsageTracker::new(5);
        tracker.record("space", "OpenFileFinder");
        tracker.record("space", "OpenFileFinder");
        tracker.save(&path);

        let loaded = UsageTracker::load(&path);
        assert_eq!(loaded.counts.get("space:OpenFileFinder"), Some(&2));
        assert!(!loaded.should_hide("space", "OpenFileFinder"));
    }
}
