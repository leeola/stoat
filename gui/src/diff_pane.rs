use crate::{
    buffer::Buffer,
    diff_map::DiffMap,
    display_map::DisplayMap,
    editor::{Editor, EditorMode},
    multi_buffer::MultiBuffer,
};
use gpui::{AppContext, Context, Entity};
use stoat::display_map::BlockProperties;
use stoat_scheduler::Executor;

/// Build a single side-by-side pane: a singleton [`MultiBuffer`] over `buffer`
/// padded with `fillers` spacer blocks. The singleton (no excerpts) keeps the
/// editor's tree-sitter syntax overlay active. Shared by the review (two-pane)
/// and conflict (three-source-pane) views so both present identical editor
/// panes differing only in count.
pub(crate) fn build_pane_editor<T: 'static>(
    buffer: Entity<Buffer>,
    fillers: Vec<BlockProperties>,
    executor: Executor,
    cx: &mut Context<'_, T>,
) -> (Entity<MultiBuffer>, Entity<Editor>) {
    let multi_buffer = {
        let buffer = buffer.clone();
        cx.new(|cx| MultiBuffer::singleton(buffer, cx))
    };
    let display_map = {
        let buffer = buffer.clone();
        cx.new(|cx| DisplayMap::new(buffer, executor, cx))
    };
    let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));

    if !fillers.is_empty() {
        display_map.update(cx, |dm, cx| dm.insert_blocks(fillers, cx));
    }

    let editor = cx.new(|cx| {
        Editor::new(
            multi_buffer.clone(),
            display_map,
            diff_map,
            EditorMode::full(),
            cx,
        )
    });
    (multi_buffer, editor)
}

/// Link every editor in `editors` into one scroll-sync group: a scroll or
/// autoscroll in any pane mirrors to all the others. Each pair is registered in
/// both directions (all-to-all) so the group stays aligned regardless of which
/// pane the user scrolls.
pub(crate) fn link_scroll_group<T: 'static>(editors: &[&Entity<Editor>], cx: &mut Context<'_, T>) {
    for (i, editor) in editors.iter().enumerate() {
        for (j, partner) in editors.iter().enumerate() {
            if i == j {
                continue;
            }
            let partner = partner.downgrade();
            editor.update(cx, |ed, _| ed.link_scroll(partner));
        }
    }
}
