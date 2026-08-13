//! Keeping a server's copy of a document level with the buffer.
//!
//! A language server answers about text it was told about, so every edit has to
//! reach it before the next request names a position in the new text. Edits
//! arrive far faster than a server wants them, so each buffer's changes settle
//! behind a 50ms debounce and only the last snapshot is sent.
//!
//! What is sent depends on what the server asked for. A full-sync server gets
//! the whole document; an incremental one gets the ranges that changed, derived
//! from the patch between the last delivered snapshot and this one. A server
//! that syncs nothing is skipped.
//!
//! A request that must not race the debounce flushes it first through
//! [`flush_pending_did_change`], which hands back the task to await.

use crate::{
    app::Stoat,
    buffer::{BufferId, TextBufferSnapshot},
    host::{LspHost, OffsetEncoding},
    lsp::{hosts, util},
};
use lsp_types::{
    DidChangeTextDocumentParams, Range, TextDocumentContentChangeEvent, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri, VersionedTextDocumentIdentifier,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use stoat_scheduler::Task;
use stoat_text::{patch::Patch, Rope};

/// Quiet window after the last edit before a buffer's `did_change` fires,
/// keeping a typing run from becoming a storm of LSP traffic.
///
/// Nothing else waits on this. A request carrying a position has to know the
/// server holds the text that position was measured against, and so flushes the
/// window through [`flush_pending_did_change`] rather than racing it.
pub(crate) const LSP_DID_CHANGE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Scan every buffer in [`Stoat::lsp_opened`] for an updated
/// [`crate::buffer::Buffer::version`] and arm a 50ms debounce per
/// buffer that has changed. Replacing the entry in
/// [`Stoat::lsp_pending_changes`] drops the prior pending task,
/// which cancels its spawned future before its timer fires; only
/// the most recent edit's snapshot ever reaches the server.
///
/// Capability honouring: dispatches when the server advertises
/// [`TextDocumentSyncKind::FULL`] (full document text) or
/// [`TextDocumentSyncKind::INCREMENTAL`] (per-edit ranges via
/// [`patch_to_content_changes`]). `NONE` skips silently.
pub(crate) fn notify_buffer_changes_pending(stoat: &mut Stoat) {
    // Refilled rather than collected fresh, since the loop body needs `stoat`
    // mutably and this runs on every event.
    let mut opened = std::mem::take(&mut stoat.lsp_drain_buffers);
    opened.clear();
    opened.extend(stoat.lsp_opened.iter().copied());

    for id in opened.iter().copied() {
        // Skip buffers unchanged since the last sync before grouping hosts and
        // building plans. build_dispatch_plan would return empty plans anyway,
        // and the version write-back below would rewrite the same value.
        if let Some(buffer) = stoat.active_workspace().buffers.get(id) {
            let version = buffer.read().expect("buffer lock").version();
            if stoat.lsp_buffer_versions.get(&id) == Some(&version) {
                continue;
            }
        }

        if let Some(task) = dispatch_did_change(stoat, id, Some(LSP_DID_CHANGE_DEBOUNCE)) {
            stoat.lsp_pending_changes.insert(id, task);
        }
    }

    opened.clear();
    stoat.lsp_drain_buffers = opened;
}

/// Send `buffer_id`'s pending `did_change` now and hand back the delivery, or
/// `None` when the servers already have everything in the buffer.
///
/// A request that skips its own debounce carries a position measured after an
/// edit whose change is still sitting in the 50ms timer, so the server resolves
/// it against text without that edit in it. Typing a trigger character is the
/// case that shows: the position lands past a character the server cannot see,
/// and the answer comes back scoped to the wrong thing.
///
/// The caller has to await what this returns. Dispatching earlier is not
/// ordering, and two spawned tasks race.
///
/// Dropping the debounce handle cancels its timer before it fires, and a
/// cancelled delivery never advanced the delivered baseline, so the change
/// rebuilt here is the one it was holding.
pub(crate) fn flush_pending_did_change(stoat: &mut Stoat, buffer_id: BufferId) -> Option<Task<()>> {
    stoat.lsp_pending_changes.remove(&buffer_id);
    dispatch_did_change(stoat, buffer_id, None)
}

/// Build `id`'s change for every host that wants one and spawn the delivery,
/// waiting `debounce` first when given.
///
/// `None` when no host takes changes or the delta against what was last
/// delivered is empty.
fn dispatch_did_change(
    stoat: &mut Stoat,
    id: BufferId,
    debounce: Option<Duration>,
) -> Option<Task<()>> {
    // Group the buffer's hosts by the sync kind and encoding each
    // negotiated, so a FULL host and an INCREMENTAL one -- or two hosts on
    // different encodings -- each get content changes shaped their own way.
    // A host on sync NONE takes no change.
    let mut groups: HostSyncGroups = Vec::new();
    for host in hosts::hosts_for_buffer(stoat, id) {
        let sync_kind = resolve_sync_kind(&host.capabilities().text_document_sync);
        if !matches!(
            sync_kind,
            TextDocumentSyncKind::FULL | TextDocumentSyncKind::INCREMENTAL
        ) {
            continue;
        }
        let key = (sync_kind, host.offset_encoding());
        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, group)) => group.push(host),
            None => groups.push((key, vec![host])),
        }
    }

    let target = capture_dispatch_target(stoat, id);

    // The change is consumed for this buffer once seen, whether or not any
    // group took it, mirroring the sync-NONE path.
    if let Some(buffer) = stoat.active_workspace().buffers.get(id) {
        let v = buffer.read().expect("buffer lock").version();
        stoat.lsp_buffer_versions.insert(id, v);
    }

    let (uri, snapshot) = target?;
    if groups.is_empty() {
        return None;
    }

    // One monotonic LSP document version per buffer, shared across groups --
    // a per-server counter buys nothing since each server still sees it rise.
    //
    // Stamped here though the changes are shaped later, so a window whose
    // changes come out empty spends a number. The counter only has to rise, and
    // a cancelled task already leaves gaps in it.
    let lsp_version = {
        let version = stoat.lsp_doc_versions.entry(id).or_insert(0);
        *version += 1;
        *version
    };

    // The delivered baseline is per-buffer, so every group targets the same
    // text and version.
    let target_text = snapshot.visible_text.clone();
    let target_version = snapshot.version;

    let executor = stoat.executor.clone();
    let last_text = stoat.lsp_last_delivered_text.clone();
    let last_version = stoat.lsp_last_delivered_buffer_version.clone();

    Some(stoat.executor.spawn(async move {
        if let Some(debounce) = debounce {
            executor.timer(debounce).await;
        }

        let mut delivered = true;
        let mut sent = false;
        for ((sync_kind, encoding), hosts) in groups {
            // Shaped here rather than where the timer was armed. Every
            // keystroke inside the window replaces the task holding the last
            // payload, so building one per keystroke serializes a document to
            // throw it away.
            let content_changes = build_content_changes(
                &snapshot,
                id,
                sync_kind,
                encoding,
                &last_text,
                &last_version,
            );
            if content_changes.is_empty() {
                continue;
            }

            let params = DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: lsp_version,
                },
                content_changes,
            };
            for lsp in hosts {
                if let Err(err) = lsp.did_change(params.clone()).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "did_change notification failed");
                    delivered = false;
                }
            }
            sent = true;
        }

        // Only advance the delivered baseline when every server in every
        // group received the change, so a failed server's next delta still
        // replays it. A window that shaped no change for anyone delivered
        // nothing, and moving the baseline would drop the edits it holds.
        if delivered && sent {
            last_text
                .lock()
                .expect("lsp text mutex")
                .insert(id, target_text);
            last_version
                .lock()
                .expect("lsp version mutex")
                .insert(id, target_version);
        }
    }))
}

/// A buffer's fanned-out hosts grouped by the sync kind and encoding each
/// negotiated, so one shaped payload serves every host in a group.
type HostSyncGroups = Vec<(
    (TextDocumentSyncKind, OffsetEncoding),
    Vec<Arc<dyn LspHost>>,
)>;

/// The document and text every group's change will be built against, or `None`
/// when the buffer holds nothing the servers have not been given.
///
/// This is the part cheap enough to run at the keystroke that arms the debounce.
/// A snapshot clone is a refcount bump on persistent structures and the version
/// compare is a map read, where the payload those feed costs a whole-document
/// string or a fresh patch walk.
fn capture_dispatch_target(stoat: &Stoat, id: BufferId) -> Option<(Uri, TextBufferSnapshot)> {
    let workspace = stoat.active_workspace();
    let buffer = workspace.buffers.get(id)?;
    let guard = buffer.read().expect("buffer lock");

    // Against what the servers were last *given*, not what the sync pump has
    // last looked at. The pump marks a buffer seen before it spawns the
    // delivery, so a flush arriving between the two would find the seen mark
    // already current and build nothing, which is the case a flush exists for.
    let last_delivered_version = stoat
        .lsp_last_delivered_buffer_version
        .lock()
        .expect("lsp version mutex")
        .get(&id)
        .copied()
        .unwrap_or(0);
    if guard.version() == last_delivered_version {
        return None;
    }

    let path = workspace.buffers.path_for(id)?.to_path_buf();
    let uri = crate::action_handlers::lsp::path_to_uri(&path)?;
    Some((uri, guard.snapshot.clone()))
}

/// The content changes one group's servers want, in the shape its negotiated
/// sync kind takes.
///
/// The baseline an INCREMENTAL delta is measured from is read here rather than
/// captured earlier, so the delta describes what those servers hold as the
/// notification goes out. Empty means the buffer says nothing new to this
/// group, which the caller answers by sending it nothing.
fn build_content_changes(
    snapshot: &TextBufferSnapshot,
    id: BufferId,
    sync_kind: TextDocumentSyncKind,
    encoding: OffsetEncoding,
    last_text: &Mutex<HashMap<BufferId, Rope>>,
    last_version: &Mutex<HashMap<BufferId, u64>>,
) -> Vec<TextDocumentContentChangeEvent> {
    match sync_kind {
        TextDocumentSyncKind::FULL => vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: snapshot.visible_text.to_string(),
        }],
        TextDocumentSyncKind::INCREMENTAL => {
            let delivered_version = last_version
                .lock()
                .expect("lsp version mutex")
                .get(&id)
                .copied()
                .unwrap_or(0);
            let delivered_text = last_text
                .lock()
                .expect("lsp text mutex")
                .get(&id)
                .cloned()
                .unwrap_or_default();
            let patch = snapshot.edits_since(delivered_version);
            patch_to_content_changes(&delivered_text, &snapshot.visible_text, &patch, encoding)
        },
        _ => Vec::new(),
    }
}

/// Translate a [`Patch`] of byte-range edits between `old_rope` and
/// `new_rope` into a sequence of [`TextDocumentContentChangeEvent`]s.
///
/// LSP applies the changes in order, each against the document the previous
/// one left behind. Emitting them back to front satisfies that without
/// tracking any running position. The rightmost edit goes first, and it can
/// only move text after itself, so every edit still to come sits at an offset
/// nothing has disturbed. Every range is therefore in the coordinates of
/// `old_rope`, which is what lets
/// [`byte_offset_to_lsp_pos`](util::byte_offset_to_lsp_pos)
/// answer each one in a seek instead of a walk from the start of the document.
///
/// Patch edits are disjoint and ascending, which is the property the argument
/// above rests on.
///
/// One encoding covers all three edit shapes. The range spans what the edit
/// replaced and the text is what it replaced it with, so a deletion carries an
/// empty text, an insertion an empty range, and a replacement neither.
fn patch_to_content_changes(
    old_rope: &Rope,
    new_rope: &Rope,
    patch: &Patch<usize>,
    encoding: OffsetEncoding,
) -> Vec<TextDocumentContentChangeEvent> {
    let mut changes = Vec::new();

    for edit in patch.into_iter().rev() {
        if edit.old.is_empty() && edit.new.is_empty() {
            continue;
        }

        let start = util::byte_offset_to_lsp_pos(old_rope, edit.old.start, encoding);
        let end = util::byte_offset_to_lsp_pos(old_rope, edit.old.end, encoding);

        changes.push(TextDocumentContentChangeEvent {
            range: Some(Range::new(start, end)),
            range_length: None,
            text: new_rope.slice(edit.new.start..edit.new.end).to_string(),
        });
    }

    changes
}

fn resolve_sync_kind(cap: &Option<TextDocumentSyncCapability>) -> TextDocumentSyncKind {
    match cap {
        Some(TextDocumentSyncCapability::Kind(k)) => *k,
        Some(TextDocumentSyncCapability::Options(o)) => {
            o.change.unwrap_or(TextDocumentSyncKind::NONE)
        },
        None => TextDocumentSyncKind::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };

    fn edit_buffer(h: &mut TestHarness, range: std::ops::Range<usize>, text: &str) {
        h.edit_focused(range, text);
    }

    fn arm_change(h: &mut TestHarness) {
        notify_buffer_changes_pending(&mut h.stoat);
    }

    #[test]
    fn did_change_fires_after_debounce_window() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "// hi\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].text_document.version, 1);
        assert_eq!(changes[0].content_changes.len(), 1);
        assert_eq!(changes[0].content_changes[0].range, None);
        assert_eq!(changes[0].content_changes[0].text, "// hi\nfn a() {}\n");
    }

    #[test]
    fn did_change_coalesces_rapid_edits() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "//1\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(20));
        edit_buffer(&mut h, 0..0, "//2\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1, "second edit must cancel the first timer");
        assert_eq!(changes[0].content_changes[0].text, "//2\n//1\nfn a() {}\n");
    }

    #[test]
    fn did_change_skipped_when_sync_kind_is_none() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "// hi\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        assert!(h.fake_lsp().observed_changes().is_empty());
    }

    #[test]
    fn did_change_shapes_params_per_host_sync_kind() {
        use crate::lsp::registry::ServerSelector;

        let mut h = TestHarness::with_size(80, 24);
        let full = Arc::new(crate::host::FakeLsp::new());
        full.set_text_document_sync(TextDocumentSyncKind::FULL);
        let incremental = Arc::new(crate::host::FakeLsp::new());
        incremental.set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        h.stoat.lsp_registry.insert("full".into(), full.clone());
        h.stoat
            .lsp_registry
            .insert("incremental".into(), incremental.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("full".into()),
                ServerSelector::all("incremental".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let full_changes = full.observed_changes();
        assert_eq!(full_changes.len(), 1);
        assert_eq!(
            full_changes[0].content_changes[0].range, None,
            "the FULL host receives whole-document text"
        );
        assert_eq!(full_changes[0].content_changes[0].text, "Xabc\n");

        let inc_changes = incremental.observed_changes();
        assert_eq!(inc_changes.len(), 1);
        assert_eq!(
            inc_changes[0].content_changes[0].range,
            Some(Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            )),
            "the INCREMENTAL host receives a ranged change for the same edit"
        );
        assert_eq!(inc_changes[0].content_changes[0].text, "X");
    }

    #[test]
    fn did_change_reaches_the_full_host_past_a_sync_none_peer() {
        use crate::lsp::registry::ServerSelector;

        let mut h = TestHarness::with_size(80, 24);
        // No sync set defaults to NONE.
        let none = Arc::new(crate::host::FakeLsp::new());
        let full = Arc::new(crate::host::FakeLsp::new());
        full.set_text_document_sync(TextDocumentSyncKind::FULL);
        h.stoat.lsp_registry.insert("none".into(), none.clone());
        h.stoat.lsp_registry.insert("full".into(), full.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("none".into()),
                ServerSelector::all("full".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let full_changes = full.observed_changes();
        assert_eq!(
            full_changes.len(),
            1,
            "the FULL host gets the change even though a sync-NONE peer sorts first"
        );
        assert_eq!(full_changes[0].content_changes[0].text, "Xabc\n");
        assert!(
            none.observed_changes().is_empty(),
            "the sync-NONE host takes no change"
        );
    }

    #[test]
    fn did_change_independent_per_buffer() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = seed(&mut h, &[("a.rs", "x\n"), ("b.rs", "y\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "A");
        open_buffer(&mut h, root.join("b.rs"));
        edit_buffer(&mut h, 0..0, "B");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let mut changes = h.fake_lsp().observed_changes();
        changes.sort_by(|a, b| {
            a.text_document
                .uri
                .as_str()
                .cmp(b.text_document.uri.as_str())
        });
        assert_eq!(changes.len(), 2);
        assert!(changes[0].text_document.uri.as_str().ends_with("/a.rs"));
        assert_eq!(changes[0].content_changes[0].text, "Ax\n");
        assert!(changes[1].text_document.uri.as_str().ends_with("/b.rs"));
        assert_eq!(changes[1].content_changes[0].text, "By\n");
    }

    #[test]
    fn did_change_incremental_single_insertion() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        let cc = &changes[0].content_changes;
        assert_eq!(cc.len(), 1, "single insertion -> single content_change");
        assert_eq!(cc[0].text, "X");
        assert_eq!(
            cc[0].range,
            Some(Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            )),
        );
    }

    #[test]
    fn did_change_incremental_single_deletion() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 1..2, "");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        let cc = &changes[0].content_changes;
        assert_eq!(cc.len(), 1);
        assert_eq!(cc[0].text, "");
        assert_eq!(
            cc[0].range,
            Some(Range::new(
                lsp_types::Position::new(0, 1),
                lsp_types::Position::new(0, 2),
            )),
        );
    }

    #[test]
    fn did_change_incremental_reverts_typed_text_after_undo() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));

        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let changes = h.fake_lsp().observed_changes();
        assert_eq!(
            changes.len(),
            2,
            "the undo sends its own incremental change"
        );
        let cc = &changes[1].content_changes;
        assert_eq!(cc.len(), 1, "reverting the insertion is one deletion");
        assert_eq!(cc[0].text, "");
        assert_eq!(
            cc[0].range,
            Some(Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 1),
            )),
            "the deletion covers the typed X",
        );
    }

    #[test]
    fn did_change_incremental_subsequent_dispatch_starts_from_last_delivered() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));

        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let after_first = h.fake_lsp().observed_changes();
        assert_eq!(after_first.len(), 1);
        assert!(after_first[0].content_changes.iter().any(|c| c.text == "X"));

        edit_buffer(&mut h, 4..4, "Z");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let all = h.fake_lsp().observed_changes();
        assert_eq!(all.len(), 2);
        let second = &all[1];
        for change in &second.content_changes {
            assert_ne!(
                change.text, "X",
                "second dispatch must not redeliver the prior insertion",
            );
        }
        assert_eq!(second.content_changes.len(), 1);
        assert_eq!(second.content_changes[0].text, "Z");
        assert_eq!(
            second.content_changes[0].range,
            Some(Range::new(
                lsp_types::Position::new(0, 4),
                lsp_types::Position::new(0, 4),
            )),
        );
    }

    /// The payload is shaped when the timer fires rather than when a keystroke
    /// armed it, so a window holding several edits produces one set of changes
    /// covering all of them against what the server was last given.
    #[test]
    fn did_change_incremental_coalesces_rapid_edits() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));

        // A delivered baseline first, so the window below measures against text
        // the server actually holds rather than against an empty document.
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        assert_eq!(h.fake_lsp().observed_changes().len(), 1);

        // Two edits inside one debounce window. The first arms the timer, the
        // second replaces the task holding it.
        edit_buffer(&mut h, 4..4, "Y");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(20));
        edit_buffer(&mut h, 5..5, "Z");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let all = h.fake_lsp().observed_changes();
        assert_eq!(all.len(), 2, "the window delivered one payload, not two");
        assert_eq!(
            all[1]
                .content_changes
                .iter()
                .map(|c| c.text.clone())
                .collect::<Vec<_>>(),
            ["YZ"],
            "covering both edits, and neither redelivering the baseline's X",
        );
    }

    #[test]
    fn did_change_incremental_replacement_carries_its_new_text() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abcdef\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 2..4, "ZZ");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        let cc = &changes[0].content_changes;
        assert_eq!(cc.len(), 1, "a replacement is one content_change");
        assert_eq!(
            cc[0].text, "ZZ",
            "the replacing text reaches the server, not a bare deletion",
        );
        assert_eq!(
            cc[0].range,
            Some(Range::new(
                lsp_types::Position::new(0, 2),
                lsp_types::Position::new(0, 4),
            )),
            "the range covers the bytes being replaced",
        );
    }

    /// Two edits in one dispatch arrive back to front, each in the
    /// coordinates of the document before any of them applied.
    ///
    /// The first edit inserts a line, so a scheme numbering the second against
    /// the state the first leaves behind would place it a row lower. Fixing
    /// both the order and the row is what distinguishes the two.
    #[test]
    fn did_change_incremental_emits_edits_back_to_front() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "aaa\nbbb\nccc\n")]);
        open_buffer(&mut h, root.join("a.rs"));

        edit_buffer(&mut h, 8..8, "Y");
        edit_buffer(&mut h, 0..0, "X\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1, "both edits ride one dispatch");
        let cc = &changes[0].content_changes;
        assert_eq!(cc.len(), 2);

        assert_eq!(cc[0].text, "Y", "the later edit is delivered first");
        assert_eq!(
            cc[0].range,
            Some(Range::new(
                lsp_types::Position::new(2, 0),
                lsp_types::Position::new(2, 0),
            )),
            "row 2 is where it sat before the line above was inserted",
        );

        assert_eq!(cc[1].text, "X\n");
        assert_eq!(
            cc[1].range,
            Some(Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            )),
        );
    }

    #[test]
    fn did_change_incremental_skips_when_buffer_already_at_delivered_state() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let baseline = h.fake_lsp().observed_changes().len();
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        assert_eq!(
            h.fake_lsp().observed_changes().len(),
            baseline,
            "no edit since last delivery -> no new dispatch",
        );
    }
}
