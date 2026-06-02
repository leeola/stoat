use crate::globals::FsHostGlobal;
use gpui::{Context, EventEmitter};
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use stoat::buffer::{BufferId, CheckpointId, Encoding, LineEnding, SharedBuffer, TextBuffer};
use stoat_language::SyntaxMap;
use stoat_text::Anchor;

/// Entity-shaped wrapper around [`SharedBuffer`]. Mutations go through
/// the wrapper's methods so the entity emits [`BufferEvent`]s on the
/// gpui foreground; subscribers re-render in response.
pub struct Buffer {
    inner: SharedBuffer,
    file_path: Option<PathBuf>,
    syntax_map: Option<SyntaxMap>,
    marks: HashMap<char, Anchor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferEvent {
    Edited,
    LanguageChanged,
    DiagnosticsUpdated,
    Saved,
    SaveFailed { error: String },
    Reloaded,
}

impl EventEmitter<BufferEvent> for Buffer {}

impl Buffer {
    pub fn from_shared(inner: SharedBuffer) -> Self {
        Self {
            inner,
            file_path: None,
            syntax_map: None,
            marks: HashMap::new(),
        }
    }

    pub fn with_text(buffer_id: BufferId, text: &str) -> Self {
        Self {
            inner: Arc::new(RwLock::new(TextBuffer::with_text(buffer_id, text))),
            file_path: None,
            syntax_map: None,
            marks: HashMap::new(),
        }
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    /// Attach (or clear) the on-disk path this buffer represents.
    /// [`save`] consults this when deciding whether to write through
    /// [`FsHostGlobal`]; `None` keeps `save` in the flip-dirty-only
    /// fallback used by scratch buffers and tests.
    pub fn set_file_path(&mut self, path: Option<PathBuf>, cx: &mut Context<'_, Self>) {
        if self.file_path == path {
            return;
        }
        self.file_path = path;
        cx.notify();
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

    pub fn line_ending(&self) -> LineEnding {
        self.inner
            .read()
            .expect("buffer lock poisoned")
            .line_ending()
    }

    pub fn set_line_ending(&self, target: LineEnding, cx: &mut Context<'_, Self>) {
        self.inner
            .write()
            .expect("buffer lock poisoned")
            .set_line_ending(target);
        cx.emit(BufferEvent::Edited);
        cx.notify();
    }

    pub fn encoding(&self) -> Encoding {
        self.inner.read().expect("buffer lock poisoned").encoding()
    }

    /// Record `encoding` and replace the content with `decoded`, the
    /// text the caller produced by re-decoding the file's bytes. The
    /// content edit is skipped when `decoded` already matches, but the
    /// metadata update and [`BufferEvent::Edited`] always fire so the
    /// status bar reflects the new encoding.
    pub fn set_encoding(&self, encoding: Encoding, decoded: &str, cx: &mut Context<'_, Self>) {
        {
            let mut guard = self.inner.write().expect("buffer lock poisoned");
            guard.set_encoding(encoding);
            let current = guard.rope().to_string();
            if decoded != current {
                guard.edit(0..current.len(), decoded);
            }
        }
        cx.emit(BufferEvent::Edited);
        cx.notify();
    }

    /// Pop the most recent edit off the inner [`TextBuffer`]'s
    /// undo stack, applying its reverse and pushing the timestamp
    /// onto the redo stack. Returns `true` when the stack had an
    /// entry to undo. Emits [`BufferEvent::Edited`] only on
    /// success so subscribers refresh derived state.
    pub fn undo(&self, cx: &mut Context<'_, Self>) -> bool {
        let applied = self.inner.write().expect("buffer lock poisoned").undo();
        if applied {
            cx.emit(BufferEvent::Edited);
            cx.notify();
        }
        applied
    }

    /// Symmetric to [`Self::undo`]: pop the most recent entry off
    /// the redo stack and re-apply it.
    pub fn redo(&self, cx: &mut Context<'_, Self>) -> bool {
        let applied = self.inner.write().expect("buffer lock poisoned").redo();
        if applied {
            cx.emit(BufferEvent::Edited);
            cx.notify();
        }
        applied
    }

    /// Record a named position in the buffer's op log and return
    /// its [`CheckpointId`]. No event is emitted because the op log
    /// is not user-visible state and no derived caches depend on
    /// checkpoint membership.
    pub fn checkpoint(&self, label: Option<String>) -> CheckpointId {
        self.inner
            .write()
            .expect("buffer lock poisoned")
            .checkpoint(label)
    }

    /// Write the buffer's rope text through [`FsHostGlobal`] when
    /// the buffer carries a [`file_path`]; on success the inner
    /// dirty flag clears and [`BufferEvent::Saved`] fires. On IO
    /// failure the dirty flag stays set and
    /// [`BufferEvent::SaveFailed`] fires so the status bar can
    /// surface the error.
    ///
    /// Buffers with no [`file_path`] (scratch, test fixtures) skip
    /// the write and fall through to the historical flip-dirty +
    /// emit-[`BufferEvent::Saved`] behavior.
    pub fn save(&self, cx: &mut Context<'_, Self>) {
        let Some(path) = self.file_path.clone() else {
            self.inner.write().expect("buffer lock poisoned").dirty = false;
            cx.emit(BufferEvent::Saved);
            cx.notify();
            return;
        };
        let fs = cx.global::<FsHostGlobal>().0.clone();
        let text = self.text();
        match fs.write(&path, text.as_bytes()) {
            Ok(()) => {
                self.inner.write().expect("buffer lock poisoned").dirty = false;
                cx.emit(BufferEvent::Saved);
                cx.notify();
            },
            Err(err) => {
                cx.emit(BufferEvent::SaveFailed {
                    error: err.to_string(),
                });
                cx.notify();
            },
        }
    }

    /// Reload the buffer from disk via [`FsHostGlobal`].
    ///
    /// Reads the bytes at [`Self::file_path`], compares against the
    /// current buffer text via
    /// [`stoat::buffer_registry::fingerprint_bytes`], and replaces
    /// the inner rope only when the disk content differs.
    /// [`BufferEvent::Reloaded`] fires exactly when content changed;
    /// matches yield no event.
    ///
    /// Path-less buffers (scratch, test fixtures) fall back to
    /// emitting `Reloaded` without touching the host so callers that
    /// use the method as a signal trigger still observe it.
    ///
    /// Failure modes log at `tracing::warn` and return without
    /// emitting: a missing file, a read IO error, or non-UTF-8 disk
    /// bytes.
    pub fn reload(&self, cx: &mut Context<'_, Self>) {
        let Some(path) = self.file_path.clone() else {
            cx.emit(BufferEvent::Reloaded);
            cx.notify();
            return;
        };
        let fs = cx.global::<FsHostGlobal>().0.clone();
        let mut bytes = Vec::new();
        if let Err(err) = fs.read(&path, &mut bytes) {
            tracing::warn!(?path, %err, "buffer reload: fs read failed");
            return;
        }
        let Ok(disk_text) = std::str::from_utf8(&bytes) else {
            tracing::warn!(?path, "buffer reload: disk bytes are not valid UTF-8");
            return;
        };
        let current_fingerprint =
            self.read(|b| stoat::buffer_registry::fingerprint_bytes(&b.rope().to_string()));
        let disk_fingerprint = stoat::buffer_registry::fingerprint_bytes(disk_text);
        if disk_fingerprint == current_fingerprint {
            return;
        }
        {
            let mut guard = self.inner.write().expect("buffer lock poisoned");
            let len = guard.rope().len();
            guard.edit(0..len, disk_text);
            guard.dirty = false;
        }
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
    fn undo_returns_false_and_does_not_emit_on_empty_history() {
        let mut cx = TestAppContext::single();
        // Buffer::with_text seeds history with the initial population edit
        // when text is non-empty, so use the empty constructor to get a
        // genuinely empty history.
        let buffer = new_buffer(&mut cx, "");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        let applied = buffer.update(&mut cx, |b, cx| b.undo(cx));
        cx.run_until_parked();

        assert!(!applied);
        assert_eq!(drain(&events), Vec::<BufferEvent>::new());
    }

    #[test]
    fn undo_then_redo_round_trips_an_edit() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "hello");
        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        cx.run_until_parked();
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello world");

        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        let undone = buffer.update(&mut cx, |b, cx| b.undo(cx));
        cx.run_until_parked();
        assert!(undone);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello");
        assert_eq!(drain(&events), vec![BufferEvent::Edited]);

        let redone = buffer.update(&mut cx, |b, cx| b.redo(cx));
        cx.run_until_parked();
        assert!(redone);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello world");
        assert_eq!(drain(&events), vec![BufferEvent::Edited]);
    }

    #[test]
    fn redo_returns_false_on_empty_history() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        let applied = buffer.update(&mut cx, |b, cx| b.redo(cx));
        cx.run_until_parked();

        assert!(!applied);
        assert_eq!(drain(&events), Vec::<BufferEvent>::new());
    }

    #[test]
    fn checkpoint_returns_distinct_ids() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        let first = buffer.update(&mut cx, |b, _| b.checkpoint(None));
        let second = buffer.update(&mut cx, |b, _| b.checkpoint(Some("label".into())));
        cx.run_until_parked();

        assert_ne!(first, second);
        assert_eq!(drain(&events), Vec::<BufferEvent>::new());
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

    fn install_fs_global(
        cx: &mut TestAppContext,
        fs: Arc<stoat::host::FakeFs>,
    ) -> Arc<stoat::host::FakeFs> {
        let arc: Arc<dyn stoat::host::FsHost> = fs.clone();
        cx.update(|cx| cx.set_global(FsHostGlobal(arc)));
        fs
    }

    #[test]
    fn save_with_path_writes_through_fs_and_clears_dirty() {
        use stoat::host::FsHost;
        let mut cx = TestAppContext::single();
        let fs = install_fs_global(&mut cx, Arc::new(stoat::host::FakeFs::new()));
        let buffer = new_buffer(&mut cx, "hello");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/tmp/out.txt")), cx)
        });
        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::Saved]);
        assert!(!buffer.read_with(&cx, |b, _| b.is_dirty()));
        let mut buf = Vec::new();
        (*fs)
            .read(Path::new("/tmp/out.txt"), &mut buf)
            .expect("file present after save");
        assert_eq!(String::from_utf8(buf).expect("utf8"), "hello world");
    }

    #[test]
    fn save_with_path_io_failure_keeps_dirty_and_emits_save_failed() {
        let mut cx = TestAppContext::single();
        let fs = install_fs_global(&mut cx, Arc::new(stoat::host::FakeFs::new()));
        fs.fail_writes_to("/tmp/locked.txt", std::io::ErrorKind::PermissionDenied);
        let buffer = new_buffer(&mut cx, "important");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/tmp/locked.txt")), cx)
        });
        buffer.update(&mut cx, |b, cx| b.edit(9..9, "!", cx));
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        let drained = drain(&events);
        assert!(
            matches!(drained.as_slice(), [BufferEvent::SaveFailed { .. }]),
            "expected SaveFailed event, got {drained:?}",
        );
        assert!(buffer.read_with(&cx, |b, _| b.is_dirty()));
    }

    #[test]
    fn save_without_path_keeps_legacy_dirty_flip() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "scratch");
        buffer.update(&mut cx, |b, cx| b.edit(7..7, "!", cx));
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::Saved]);
        assert!(!buffer.read_with(&cx, |b, _| b.is_dirty()));
    }

    #[test]
    fn reload_with_matching_disk_content_is_noop() {
        let mut cx = TestAppContext::single();
        let fs = install_fs_global(&mut cx, Arc::new(stoat::host::FakeFs::new()));
        fs.insert_file("/tmp/same.txt", b"same");
        let buffer = new_buffer(&mut cx, "same");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/tmp/same.txt")), cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BufferEvent>::new());
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "same");
    }

    #[test]
    fn reload_with_differing_disk_content_replaces_text() {
        let mut cx = TestAppContext::single();
        let fs = install_fs_global(&mut cx, Arc::new(stoat::host::FakeFs::new()));
        fs.insert_file("/tmp/file.txt", b"new content");
        let buffer = new_buffer(&mut cx, "old content");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/tmp/file.txt")), cx)
        });
        buffer.update(&mut cx, |b, cx| b.edit(0..0, "x", cx));
        assert!(buffer.read_with(&cx, |b, _| b.is_dirty()));
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferEvent::Reloaded]);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "new content");
        assert!(!buffer.read_with(&cx, |b, _| b.is_dirty()));
    }

    #[test]
    fn reload_with_read_error_emits_nothing() {
        let mut cx = TestAppContext::single();
        let _fs = install_fs_global(&mut cx, Arc::new(stoat::host::FakeFs::new()));
        let buffer = new_buffer(&mut cx, "kept");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/tmp/missing.txt")), cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BufferEvent>::new());
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "kept");
    }

    #[test]
    fn reload_with_invalid_utf8_emits_nothing() {
        let mut cx = TestAppContext::single();
        let fs = install_fs_global(&mut cx, Arc::new(stoat::host::FakeFs::new()));
        fs.insert_file("/tmp/binary.bin", [0xffu8, 0xfe, 0xfd]);
        let buffer = new_buffer(&mut cx, "kept");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/tmp/binary.bin")), cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &buffer);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BufferEvent>::new());
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "kept");
    }

    #[test]
    fn set_file_path_no_op_when_unchanged_does_not_notify() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, "x");
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/a")), cx)
        });
        assert_eq!(
            buffer.read_with(&cx, |b, _| b.file_path().map(Path::to_path_buf)),
            Some(PathBuf::from("/a")),
        );
        buffer.update(&mut cx, |b, cx| {
            b.set_file_path(Some(PathBuf::from("/a")), cx)
        });
        assert_eq!(
            buffer.read_with(&cx, |b, _| b.file_path().map(Path::to_path_buf)),
            Some(PathBuf::from("/a")),
        );
    }
}
