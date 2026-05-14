use gpui::{Context, EventEmitter};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, RwLock},
};
use stoat::buffer::{BufferId, SharedBuffer, TextBuffer};
use stoat_language::SyntaxMap;
use stoat_text::Anchor;

/// Entity-shaped wrapper around [`SharedBuffer`]. Mutations go through
/// the wrapper's methods so the entity emits [`BufferEvent`]s on the
/// gpui foreground; subscribers re-render in response.
pub struct Buffer {
    inner: SharedBuffer,
    syntax_map: Option<SyntaxMap>,
    marks: HashMap<char, Anchor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferEvent {
    Edited,
    LanguageChanged,
    DiagnosticsUpdated,
    Saved,
    Reloaded,
}

impl EventEmitter<BufferEvent> for Buffer {}

impl Buffer {
    pub fn from_shared(inner: SharedBuffer) -> Self {
        Self {
            inner,
            syntax_map: None,
            marks: HashMap::new(),
        }
    }

    pub fn with_text(buffer_id: BufferId, text: &str) -> Self {
        Self {
            inner: Arc::new(RwLock::new(TextBuffer::with_text(buffer_id, text))),
            syntax_map: None,
            marks: HashMap::new(),
        }
    }

    pub fn shared(&self) -> &SharedBuffer {
        &self.inner
    }

    pub fn read<R>(&self, f: impl FnOnce(&TextBuffer) -> R) -> R {
        let guard = self.inner.read().expect("buffer lock poisoned");
        f(&guard)
    }

    pub fn text(&self) -> String {
        self.read(|b| b.rope().to_string())
    }

    pub fn is_dirty(&self) -> bool {
        self.read(|b| b.dirty)
    }

    pub fn edit(&self, range: Range<usize>, text: &str, cx: &mut Context<'_, Self>) {
        self.inner
            .write()
            .expect("buffer lock poisoned")
            .edit(range, text);
        cx.emit(BufferEvent::Edited);
        cx.notify();
    }

    pub fn save(&self, cx: &mut Context<'_, Self>) {
        self.inner.write().expect("buffer lock poisoned").dirty = false;
        cx.emit(BufferEvent::Saved);
        cx.notify();
    }

    pub fn reload(&self, cx: &mut Context<'_, Self>) {
        cx.emit(BufferEvent::Reloaded);
        cx.notify();
    }

    pub fn language_changed(&self, cx: &mut Context<'_, Self>) {
        cx.emit(BufferEvent::LanguageChanged);
        cx.notify();
    }

    /// Returns the buffer's multi-layer parse tree, if one has been
    /// installed by the parsing pipeline. Tree-sitter motion handlers
    /// no-op when this returns `None`.
    pub fn syntax_map(&self) -> Option<&SyntaxMap> {
        self.syntax_map.as_ref()
    }

    /// Install (or clear) the buffer's multi-layer parse tree. Emits
    /// [`BufferEvent::LanguageChanged`] so editors re-render any
    /// syntax-driven decoration.
    pub fn set_syntax_map(&mut self, map: Option<SyntaxMap>, cx: &mut Context<'_, Self>) {
        self.syntax_map = map;
        cx.emit(BufferEvent::LanguageChanged);
        cx.notify();
    }

    /// Store the cursor anchor `anchor` under mark name `ch`.
    /// Overwrites any prior mark with the same name. Marks are not
    /// rendered, so the call does not emit [`BufferEvent`].
    pub fn set_mark(&mut self, ch: char, anchor: Anchor) {
        self.marks.insert(ch, anchor);
    }

    /// Returns the anchor stored under mark name `ch`, or `None`
    /// when no mark has been set under that name.
    pub fn get_mark(&self, ch: char) -> Option<Anchor> {
        self.marks.get(&ch).copied()
    }

    pub fn diagnostics_updated(&self, cx: &mut Context<'_, Self>) {
        cx.emit(BufferEvent::DiagnosticsUpdated);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use std::sync::Mutex;

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            buffer: &Entity<Buffer>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<BufferEvent>>>) {
            let events: Arc<Mutex<Vec<BufferEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let buffer = buffer.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&buffer, move |_, _, event: &BufferEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    Recorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    fn new_buffer(cx: &mut TestAppContext, text: &str) -> Entity<Buffer> {
        cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)))
    }

    fn drain(events: &Arc<Mutex<Vec<BufferEvent>>>) -> Vec<BufferEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    #[test]
    fn edit_emits_edited_and_updates_text() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "hello");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::Edited]);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello world");
        assert!(buffer.read_with(&cx, |b, _| b.is_dirty()));
    }

    #[test]
    fn save_clears_dirty_and_emits_saved() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "hi");
        buffer.update(&mut cx, |b, cx| b.edit(2..2, "!", cx));
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::Saved]);
        assert!(!buffer.read_with(&cx, |b, _| b.is_dirty()));
    }

    #[test]
    fn reload_emits_reloaded() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::Reloaded]);
    }

    #[test]
    fn language_changed_emits_language_changed() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.language_changed(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::LanguageChanged]);
    }

    #[test]
    fn diagnostics_updated_emits_event() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.diagnostics_updated(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::DiagnosticsUpdated]);
    }

    #[test]
    fn shared_lets_other_holders_observe_mutations() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "abc");
        let shared = buffer.read_with(&cx, |b, _| b.shared().clone());

        buffer.update(&mut cx, |b, cx| b.edit(3..3, "d", cx));
        cx.run_until_parked();

        assert_eq!(
            shared
                .read()
                .expect("buffer lock poisoned")
                .rope()
                .to_string(),
            "abcd"
        );
    }
}
