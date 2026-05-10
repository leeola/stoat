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

use crate::settings::Settings;
use gpui::App;

/// All app globals registered at startup. Grows additively as new
/// global types are introduced; new fields are added by sibling
/// items in this parent (theme, language registry).
pub struct Globals {
    pub settings: Settings,
}

/// Register the production set of app globals on `cx`.
pub fn install_production_globals(cx: &mut App, globals: Globals) {
    cx.set_global(globals.settings);
}
