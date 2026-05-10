use gpui::{App, Entity};
use serde::Serialize;

/// Serialize an entity's state via [`serde_json`] and assert against the
/// named insta snapshot. Comparable to the TUI test harness's
/// `assert_snapshot`, but captures structured state instead of a
/// rendered character grid.
pub fn assert_entity_snapshot<T>(name: &str, entity: &Entity<T>, cx: &App)
where
    T: 'static + Serialize,
{
    let value = serde_json::to_value(entity.read(cx)).expect("entity serializes");
    insta::assert_json_snapshot!(name, value);
}
