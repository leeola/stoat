/// Process environment lookup.
///
/// Production code reads env vars through this trait so tests can
/// install [`crate::FakeEnv`] without leaking real environment
/// state. UTF-8-only by design: non-UTF-8 values are reported as
/// absent. Callers needing `OsString` semantics (e.g. exotic paths)
/// can extend the trait when a real consumer surfaces.
pub trait EnvHost: Send + Sync {
    /// Returns the value of the variable named `name`, or `None` if
    /// the variable is unset, empty, or holds non-UTF-8 bytes.
    fn var(&self, name: &str) -> Option<String>;

    /// Returns every environment variable as a UTF-8 name/value pair.
    /// Variables whose name or value holds non-UTF-8 bytes are omitted,
    /// matching the UTF-8-only contract of [`EnvHost::var`]. Iteration
    /// order is unspecified.
    fn vars(&self) -> Vec<(String, String)>;
}

/// Production [`EnvHost`] backed by [`std::env::var`].
pub struct LocalEnv;

impl EnvHost for LocalEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn vars(&self) -> Vec<(String, String)> {
        std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .collect()
    }
}
