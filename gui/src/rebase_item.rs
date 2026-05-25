use crate::{
    item::{DeserializeSnafu, ItemError, ItemView},
    theme::ActiveTheme,
};
use gpui::{
    div, uniform_list, AnyElement, App, AppContext, Context, Entity, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, UniformListScrollHandle, Window,
};
use serde_json::Value;
use stoat::{host::RebaseTodoOp, rebase::RebaseState};
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

const DATE_FORMAT: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]");

/// Direction parameter for [`RebaseItem::handle_move`]. Picked at
/// dispatch time from the four cursor / reorder rebase actions
/// (`RebaseNext` / `RebasePrev` / `RebaseMoveUp` / `RebaseMoveDown`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RebaseMoveDir {
    Next,
    Prev,
    SwapUp,
    SwapDown,
}

/// Pane-hosted view over an in-progress rebase plan.
///
/// Renders [`RebaseState::todo`] as a scrollable [`uniform_list`]
/// with one row per entry (op + short_sha + summary + author +
/// date). The row at [`RebaseState::selected`] is highlighted.
/// Mutations on the inner state (forthcoming
/// `RebaseNext` / `RebasePrev` / `RebaseMoveUp` / `RebaseMoveDown` /
/// `SetRebaseOp*` handlers) flow through `cx.notify()` on the inner
/// entity and trigger a re-render here via [`cx.observe`].
pub struct RebaseItem {
    state: Entity<RebaseState>,
    scroll_handle: UniformListScrollHandle,
    _state_subscription: Subscription,
}

impl RebaseItem {
    pub fn new(state: RebaseState, cx: &mut Context<'_, Self>) -> Self {
        let state = cx.new(|_| state);
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            scroll_handle: UniformListScrollHandle::new(),
            _state_subscription: subscription,
        }
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &Entity<RebaseState> {
        &self.state
    }

    /// Apply a cursor or reorder mutation to the inner [`RebaseState`].
    /// Notifies the inner entity so the outer item re-renders via the
    /// `cx.observe` subscription installed in [`RebaseItem::new`].
    pub(crate) fn handle_move(&self, dir: RebaseMoveDir, cx: &mut Context<'_, Self>) {
        self.state.update(cx, |state, cx| {
            let moved = match dir {
                RebaseMoveDir::Next => state.move_down(),
                RebaseMoveDir::Prev => state.move_up(),
                RebaseMoveDir::SwapUp => state.swap_up(),
                RebaseMoveDir::SwapDown => state.swap_down(),
            };
            if moved {
                cx.notify();
            }
        });
    }

    /// Set the operation on the cursor entry. No-op when the entry
    /// already carries `op`.
    pub(crate) fn handle_set_op(&self, op: RebaseTodoOp, cx: &mut Context<'_, Self>) {
        self.state.update(cx, |state, cx| {
            if state.set_op(op) {
                cx.notify();
            }
        });
    }

    /// Snapshot the inner [`RebaseState`] for hand-off to
    /// `ActiveRebase::new`. The state is cloned rather than taken so
    /// the item stays renderable up until its caller closes it.
    pub(crate) fn take_plan(&self, cx: &App) -> RebaseState {
        self.state.read(cx).clone()
    }

    fn render_row(&self, ix: usize, selected: bool, cx: &App) -> AnyElement {
        let row_text = self
            .state
            .read(cx)
            .todo
            .get(ix)
            .map(format_rebase_row)
            .unwrap_or_default();
        let color = cx.theme().statusbar_text;
        let mut row = div().px_2().text_color(color).child(row_text);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }

    fn render_rows(
        &self,
        range: std::ops::Range<usize>,
        selected: usize,
        cx: &App,
    ) -> Vec<AnyElement> {
        range
            .map(|ix| self.render_row(ix, ix == selected, cx))
            .collect()
    }
}

impl Render for RebaseItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let count = self.state.read(cx).todo.len();
        let selected = self.state.read(cx).selected;
        let handle = cx.weak_entity();

        let list = uniform_list("rebase-rows", count, move |range, _window, cx| {
            let Some(item) = handle.upgrade() else {
                return Vec::new();
            };
            item.read(cx).render_rows(range, selected, cx)
        })
        .track_scroll(self.scroll_handle.clone())
        .flex_grow();

        div().flex().flex_col().size_full().child(list)
    }
}

impl ItemView for RebaseItem {
    fn tab_label(&self, _cx: &App) -> SharedString {
        SharedString::from("Rebase")
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "RebaseItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }

    fn item_kind(&self) -> crate::item::ItemKind {
        crate::item::ItemKind::Rebase
    }
}

fn format_rebase_row(entry: &stoat::rebase::RebaseEntry) -> String {
    let op_label = match entry.op {
        RebaseTodoOp::Pick => "pick  ",
        RebaseTodoOp::Squash => "squash",
        RebaseTodoOp::Fixup => "fixup ",
        RebaseTodoOp::Drop => "drop  ",
        RebaseTodoOp::Reword => "reword",
        RebaseTodoOp::Edit => "edit  ",
    };
    let author_first = entry
        .commit
        .author_name
        .split_whitespace()
        .next()
        .unwrap_or("");
    let date = OffsetDateTime::from_unix_timestamp(entry.commit.time)
        .ok()
        .and_then(|dt| dt.format(&DATE_FORMAT).ok())
        .unwrap_or_default();
    format!(
        "{}  {}  {}  {}  {}",
        op_label, entry.commit.short_sha, entry.commit.summary, author_first, date,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::path::PathBuf;
    use stoat::{host::CommitInfo, rebase::RebaseEntry};

    fn mk_entry(op: RebaseTodoOp, sha: &str, summary: &str) -> RebaseEntry {
        RebaseEntry {
            op,
            commit: CommitInfo {
                sha: sha.to_string(),
                short_sha: sha.chars().take(7).collect(),
                summary: summary.to_string(),
                author_name: "Alice Example".to_string(),
                author_email: "alice@example.invalid".to_string(),
                time: 1_700_000_000,
                parent_count: 1,
            },
        }
    }

    fn mk_state(entries: Vec<RebaseEntry>) -> RebaseState {
        RebaseState::new(PathBuf::from("/tmp/repo"), "onto1".to_string(), entries)
    }

    #[test]
    fn tab_label_returns_rebase() {
        let cx = TestAppContext::single();
        let item = cx.update(|cx| {
            let state = mk_state(vec![mk_entry(RebaseTodoOp::Pick, "abc1234", "first")]);
            cx.new(|cx| RebaseItem::new(state, cx))
        });
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Rebase"));
        });
    }

    #[test]
    fn is_dirty_is_false_initially() {
        let cx = TestAppContext::single();
        let item = cx.update(|cx| {
            let state = mk_state(vec![mk_entry(RebaseTodoOp::Pick, "abc1234", "first")]);
            cx.new(|cx| RebaseItem::new(state, cx))
        });
        item.read_with(&cx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn state_accessor_exposes_underlying_entity() {
        let cx = TestAppContext::single();
        let item = cx.update(|cx| {
            let state = mk_state(vec![
                mk_entry(RebaseTodoOp::Pick, "abc1234", "first"),
                mk_entry(RebaseTodoOp::Drop, "def5678", "second"),
            ]);
            cx.new(|cx| RebaseItem::new(state, cx))
        });
        item.read_with(&cx, |item, app| {
            let state = item.state().read(app);
            assert_eq!(state.todo.len(), 2);
            assert_eq!(state.onto, "onto1");
            assert_eq!(state.selected, 0);
        });
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        let item = cx.update(|cx| {
            let state = mk_state(vec![mk_entry(RebaseTodoOp::Pick, "abc1234", "first")]);
            cx.new(|cx| RebaseItem::new(state, cx))
        });
        let err = item.update(&mut cx, |_, cx| {
            RebaseItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }

    #[test]
    fn format_rebase_row_emits_pick_with_short_sha_and_metadata() {
        let row = format_rebase_row(&mk_entry(RebaseTodoOp::Pick, "abc1234", "first commit"));
        assert_eq!(row, "pick    abc1234  first commit  Alice  2023-11-14");
    }

    #[test]
    fn format_rebase_row_uses_correct_label_per_op() {
        assert!(
            format_rebase_row(&mk_entry(RebaseTodoOp::Squash, "s", "x")).starts_with("squash  ")
        );
        assert!(format_rebase_row(&mk_entry(RebaseTodoOp::Fixup, "s", "x")).starts_with("fixup   "));
        assert!(format_rebase_row(&mk_entry(RebaseTodoOp::Drop, "s", "x")).starts_with("drop    "));
        assert!(
            format_rebase_row(&mk_entry(RebaseTodoOp::Reword, "s", "x")).starts_with("reword  ")
        );
        assert!(format_rebase_row(&mk_entry(RebaseTodoOp::Edit, "s", "x")).starts_with("edit    "));
    }

    #[test]
    fn format_rebase_row_uses_first_word_of_author() {
        let mut entry = mk_entry(RebaseTodoOp::Pick, "abc1234", "first");
        entry.commit.author_name = "Multi Word Name".to_string();
        let row = format_rebase_row(&entry);
        assert!(row.contains("  Multi  "));
        assert!(!row.contains("Word"));
    }

    #[test]
    fn format_rebase_row_handles_empty_summary() {
        let mut entry = mk_entry(RebaseTodoOp::Pick, "abc1234", "");
        entry.commit.summary = String::new();
        let row = format_rebase_row(&entry);
        assert_eq!(row, "pick    abc1234    Alice  2023-11-14");
    }
}
