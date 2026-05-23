use crate::buffer::{Buffer, BufferEvent};
use gpui::{Context, Entity, Subscription, WeakEntity};
use std::sync::Arc;
use stoat_language::{Language, SyntaxMap};

/// Owns the multi-layer parse tree for one [`Buffer`] entity and
/// keeps it in sync with the buffer's text. Constructed by
/// [`crate::editor::Editor::install_syntax_map_updater`] when the
/// editor opens a file whose extension maps to a registered
/// [`Language`].
///
/// The updater runs one parse at construction so the initial paint
/// has syntax styling, then subscribes to the buffer's
/// [`BufferEvent::Edited`] / [`BufferEvent::Reloaded`] stream and
/// reparses each time. Parses run on the foreground -- moving to a
/// background executor lands later when the parse cost is profiled.
pub struct SyntaxMapUpdater {
    buffer: WeakEntity<Buffer>,
    language: Arc<Language>,
    version: u64,
    _subscription: Subscription,
}

impl SyntaxMapUpdater {
    pub fn new(
        buffer: Entity<Buffer>,
        language: Arc<Language>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let weak_buffer = buffer.downgrade();
        let subscription = cx.subscribe(&buffer, |this, _buf, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited | BufferEvent::Reloaded) {
                this.reparse(cx);
            }
        });
        let mut updater = Self {
            buffer: weak_buffer,
            language,
            version: 0,
            _subscription: subscription,
        };
        updater.reparse(cx);
        updater
    }

    fn reparse(&mut self, cx: &mut Context<'_, Self>) {
        let _span = tracing::trace_span!("syntax.reparse").entered();
        let Some(buffer) = self.buffer.upgrade() else {
            return;
        };
        let rope = buffer.read(cx).read(|b| b.rope().clone());
        self.version = self.version.wrapping_add(1);
        let mut map = SyntaxMap::new();
        let _ = map.reparse(&rope, self.language.clone(), self.version);
        buffer.update(cx, |b, cx| b.set_syntax_map(Some(map), cx));
    }

    pub fn language(&self) -> &Arc<Language> {
        &self.language
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use stoat::buffer::BufferId;
    use stoat_language::LanguageRegistry;

    fn new_buffer(cx: &mut TestAppContext, text: &str) -> Entity<Buffer> {
        cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)))
    }

    fn rust_language() -> Arc<Language> {
        let registry = LanguageRegistry::standard();
        let lang = registry
            .languages()
            .iter()
            .find(|l| l.name == "rust")
            .expect("rust language registered")
            .clone();
        let styles = stoat::display_map::syntax_theme::SyntaxStyles::from_theme(
            &stoat::theme::Theme::empty(),
        );
        let map =
            stoat_language::HighlightMap::new(lang.highlight_capture_names(), styles.theme_keys());
        lang.set_highlight_map(map);
        lang
    }

    #[test]
    fn new_seeds_initial_parse() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "fn main() {}");
        let language = rust_language();
        let _updater =
            cx.update(|cx| cx.new(|cx| SyntaxMapUpdater::new(buffer.clone(), language, cx)));
        cx.run_until_parked();
        let has_map = buffer.read_with(&cx, |b, _| b.syntax_map().is_some());
        assert!(has_map, "initial parse populates syntax_map");
    }

    #[test]
    fn initial_parse_has_at_least_one_layer() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "fn main() {}");
        let language = rust_language();
        let _updater =
            cx.update(|cx| cx.new(|cx| SyntaxMapUpdater::new(buffer.clone(), language, cx)));
        cx.run_until_parked();
        let layer_count = buffer.read_with(&cx, |b, _| {
            b.syntax_map()
                .map(|m| m.snapshot().layer_count())
                .unwrap_or(0)
        });
        assert!(layer_count >= 1, "expected >= 1 layer, got {layer_count}");
    }

    #[test]
    fn edit_reparses_and_republishes() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "fn a() {}");
        let language = rust_language();
        let _updater =
            cx.update(|cx| cx.new(|cx| SyntaxMapUpdater::new(buffer.clone(), language, cx)));
        cx.run_until_parked();
        buffer.update(&mut cx, |b, cx| b.edit(3..4, "main", cx));
        cx.run_until_parked();
        let text_after = buffer.read_with(&cx, |b, _| b.text());
        assert_eq!(text_after, "fn main() {}");
        let layer_count = buffer.read_with(&cx, |b, _| {
            b.syntax_map()
                .map(|m| m.snapshot().layer_count())
                .unwrap_or(0)
        });
        assert!(layer_count >= 1);
    }

    #[test]
    fn reload_reparses() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "fn a() {}");
        let language = rust_language();
        let _updater =
            cx.update(|cx| cx.new(|cx| SyntaxMapUpdater::new(buffer.clone(), language, cx)));
        cx.run_until_parked();
        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();
        let has_map = buffer.read_with(&cx, |b, _| b.syntax_map().is_some());
        assert!(has_map);
    }

    #[test]
    fn dropping_buffer_makes_updater_inert() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "fn a() {}");
        let language = rust_language();
        let updater =
            cx.update(|cx| cx.new(|cx| SyntaxMapUpdater::new(buffer.clone(), language, cx)));
        drop(buffer);
        cx.run_until_parked();
        updater.update(&mut cx, |u, cx| u.reparse(cx));
    }
}
