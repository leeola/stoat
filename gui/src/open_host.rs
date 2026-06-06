//! URL-open host: a testable seam over gpui's URL opener.
//!
//! Mirrors the `ClipboardHost` pattern -- a trait with a production impl
//! and a test fake -- so a clicked link routes through
//! [`OpenHostGlobal`](crate::globals::OpenHostGlobal) and tests can assert
//! the URL without launching a browser. Lives in this crate rather than
//! `stoat::host` because the real impl calls [`gpui::App::open_url`], which
//! the gpui-free `stoat` crate cannot reach.

use gpui::App;
#[cfg(test)]
use std::sync::Mutex;

/// Opens a URL in the user's browser. The production impl delegates to the
/// platform opener; the test fake records the requested URLs instead.
pub(crate) trait OpenHost: Send + Sync {
    fn open_url(&self, url: &str, cx: &App);
}

/// Production [`OpenHost`] that delegates to [`gpui::App::open_url`].
pub(crate) struct GpuiOpenHost;

impl OpenHost for GpuiOpenHost {
    fn open_url(&self, url: &str, cx: &App) {
        cx.open_url(url);
    }
}

#[cfg(test)]
pub(crate) struct FakeOpenHost {
    urls: Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeOpenHost {
    pub(crate) fn new() -> Self {
        Self {
            urls: Mutex::new(Vec::new()),
        }
    }

    /// The URLs passed to [`OpenHost::open_url`], in call order.
    pub(crate) fn opened(&self) -> Vec<String> {
        self.urls.lock().expect("poisoned").clone()
    }
}

#[cfg(test)]
impl OpenHost for FakeOpenHost {
    fn open_url(&self, url: &str, _cx: &App) {
        self.urls.lock().expect("poisoned").push(url.to_owned());
    }
}
