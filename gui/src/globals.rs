//! App globals: types stored via [`gpui::App::set_global`] so any
//! entity can read them with `cx.global::<T>()` without entity-to-
//! entity plumbing. Cross-cutting state like the language registry,
//! settings, and the active theme lives here.
//!
//! Production wiring calls [`install_production_globals`] during
//! startup with a fully-constructed [`Globals`] aggregate; tests
//! register their own values via
//! [`crate::test::TestHarness::set_global`] or directly through
//! `cx.set_global(...)`.

use crate::{settings::Settings, theme::Theme};
use gpui::{App, Global};

/// App-global wrapper around [`stoat_language::LanguageRegistry`].
pub struct LanguageRegistry(pub stoat_language::LanguageRegistry);

impl Global for LanguageRegistry {}

impl LanguageRegistry {
    /// Standard registry with the production grammars wired in.
    pub fn standard() -> Self {
        Self(stoat_language::LanguageRegistry::standard())
    }
}

/// All app globals registered at startup. Grows additively as new
/// global types are introduced; new fields are added by sibling
/// items in this parent (host-trait globals).
pub struct Globals {
    pub settings: Settings,
    pub theme: Theme,
    pub language_registry: LanguageRegistry,
}

/// Register the production set of app globals on `cx`.
pub fn install_production_globals(cx: &mut App, globals: Globals) {
    cx.set_global(globals.settings);
    cx.set_global(globals.theme);
    cx.set_global(globals.language_registry);
}
