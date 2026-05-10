//! App globals: types stored via [`gpui::App::set_global`] so any
//! entity can read them with `cx.global::<T>()` without entity-to-
//! entity plumbing. Cross-cutting state like the language registry,
//! settings, and the active theme lives here.
//!
//! Production wiring calls [`install_production_globals`] during
//! startup; tests register their own values via
//! [`crate::test::TestHarness::set_global`] or directly through
//! `cx.set_global(...)`.

use gpui::App;

/// Register the production set of app globals on `cx`. The
/// concrete globals (settings, theme, language registry) are added
/// by the "Foundation: app globals, settings, theme" parent items.
pub fn install_production_globals(_cx: &mut App) {}
