use crate::env::EnvHost;
use std::{collections::HashMap, sync::Mutex};

/// Test [`EnvHost`] backed by an in-memory map. Tests `set` /
/// `unset` to seed values; readers see exactly what was seeded
/// regardless of the host environment.
pub struct FakeEnv {
    vars: Mutex<HashMap<String, String>>,
}

impl Default for FakeEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeEnv {
    pub fn new() -> Self {
        Self {
            vars: Mutex::new(HashMap::new()),
        }
    }

    pub fn set(&self, name: impl Into<String>, value: impl Into<String>) {
        self.vars
            .lock()
            .expect("FakeEnv lock poisoned")
            .insert(name.into(), value.into());
    }

    pub fn unset(&self, name: &str) {
        self.vars
            .lock()
            .expect("FakeEnv lock poisoned")
            .remove(name);
    }
}

impl EnvHost for FakeEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars
            .lock()
            .expect("FakeEnv lock poisoned")
            .get(name)
            .cloned()
    }

    fn vars(&self) -> Vec<(String, String)> {
        self.vars
            .lock()
            .expect("FakeEnv lock poisoned")
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_var_roundtrip() {
        let env = FakeEnv::new();
        assert_eq!(env.var("X"), None);
        env.set("X", "hello");
        assert_eq!(env.var("X"), Some("hello".to_string()));
    }

    #[test]
    fn unset_removes_value() {
        let env = FakeEnv::new();
        env.set("X", "v");
        env.unset("X");
        assert_eq!(env.var("X"), None);
    }

    #[test]
    fn distinct_keys_independent() {
        let env = FakeEnv::new();
        env.set("A", "alpha");
        env.set("B", "beta");
        assert_eq!(env.var("A"), Some("alpha".to_string()));
        assert_eq!(env.var("B"), Some("beta".to_string()));
    }

    #[test]
    fn vars_returns_all_seeded_pairs() {
        let env = FakeEnv::new();
        env.set("A", "alpha");
        env.set("B", "beta");
        let mut vars = env.vars();
        vars.sort();
        assert_eq!(
            vars,
            vec![
                ("A".to_string(), "alpha".to_string()),
                ("B".to_string(), "beta".to_string()),
            ]
        );
    }
}
