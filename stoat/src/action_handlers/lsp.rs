//! LSP buffer-lifecycle plumbing. This module routes
//! [`crate::buffer::BufferId`] open / close / save / change events to
//! the workspace's [`crate::host::LspHost`] so a real language server
//! can keep its document mirror in sync with the editor.
//!
//! `did_open` fires synchronously per [`notify_buffer_opened`] and
//! `did_change` fires after a 50ms quiet window per
//! [`notify_buffer_changes_pending`]. `did_save` / `did_close` are
//! still pending; both wait on user-facing buffer-save / buffer-close
//! actions that do not yet exist.

use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    display_map::{DisplayPoint, DisplaySnapshot, InlayKind},
    editor_state::ScrollGlide,
    host::{LanguageServerFeature, LspHost, OffsetEncoding},
    location_picker::{LocationEntry, LocationPicker},
    lsp::stamp::DocumentStamp,
    symbol_finder::{SymbolFinder, SymbolFinderEntry, SymbolFinderScope, SymbolTarget},
};
pub(crate) use lsp_types::Uri;
use lsp_types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, DocumentFormattingParams,
    DocumentRangeFormattingParams, DocumentSymbolParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, InlayHint, InlayHintLabel, InlayHintParams, OneOf,
    Position, PrepareRenameResponse, Range, ReferenceContext, ReferenceParams, RenameParams,
    SymbolInformation, SymbolKind, TextDocumentIdentifier, TextDocumentPositionParams, TextEdit,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_scheduler::Task;
use stoat_text::{Anchor, Bias, Point, Rope, SelectionGoal};

/// Which diagnostic [`goto_diagnostic`] goes to.
///
/// `Next` and `Prev` search out from the cursor's byte offset and stop rather
/// than wrapping when the search exhausts. `First` and `Last` take the ends of
/// the sorted list and ignore the cursor.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DiagnosticDirection {
    Next,
    Prev,
    First,
    Last,
}

/// Move the focused editor's primary cursor to the next or previous
/// LSP diagnostic for that buffer. No-op when the focused pane is
/// not an editor, the buffer has no path, or no diagnostic lies in
/// the requested direction.
pub(crate) fn goto_diagnostic(stoat: &mut Stoat, direction: DiagnosticDirection) -> UpdateEffect {
    // Repeating a search for the next one goes somewhere new, where repeating a
    // jump to the first goes nowhere, so only the two searches are repeatable.
    if matches!(
        direction,
        DiagnosticDirection::Next | DiagnosticDirection::Prev
    ) {
        stoat.last_motion = Some(crate::action_handlers::LastMotion::Diagnostic { dir: direction });
    }
    let (cursor_offset, buffer_id, _rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_off = buffer_snapshot.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buffer_snapshot.rope(), tail_off, head_off);
        (offset, editor.buffer_id, buffer_snapshot.rope().clone())
    };

    let path = match stoat.active_workspace().buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return UpdateEffect::None,
    };

    // Where each diagnostic sits now. The position the server named is in the
    // coordinates of text that has moved, so the anchor taken at publish is what
    // still points at it.
    let snapshot = {
        let editor = crate::action_handlers::focused_editor_mut(stoat).expect("editor");
        editor.display_map.snapshot()
    };
    let buffer_snapshot = snapshot.buffer_snapshot();
    let buffer_id = buffer_snapshot.buffer_id();
    // Both ends travel together through the one batch resolve, so the pair the
    // selection needs survives the trip rather than only where each span opens.
    let ends: Vec<Anchor> = stoat
        .diagnostics
        .spans(&path)
        .iter()
        .filter_map(|span| span.anchors)
        .filter(|(start, _)| start.buffer_id == Some(buffer_id))
        .flat_map(|(start, end)| [start, end])
        .collect();
    let offsets = buffer_snapshot.resolve_anchors_batch(&ends);
    let mut spans: Vec<(usize, usize)> = offsets.chunks_exact(2).map(|p| (p[0], p[1])).collect();
    spans.sort_unstable();

    // Both directions compare where the diagnostic opens, so stepping back from
    // inside one leaves it rather than selecting it again.
    let target = match direction {
        DiagnosticDirection::Next => spans.into_iter().find(|&(start, _)| start > cursor_offset),
        DiagnosticDirection::Prev => spans
            .into_iter()
            .rev()
            .find(|&(start, _)| start < cursor_offset),
        // The ends of the sorted list, whatever the cursor is near.
        DiagnosticDirection::First => spans.first().copied(),
        DiagnosticDirection::Last => spans.last().copied(),
    };

    let Some((start, end)) = target else {
        return UpdateEffect::None;
    };

    // The origin goes on the jumplist before the motion lands, so the jump back
    // returns to the reading position rather than to the diagnostic.
    crate::action_handlers::jump::push_jump(stoat);

    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    editor.selections.set_single_range(
        buffer_snapshot.anchor_at(start, Bias::Right),
        buffer_snapshot.anchor_at(end, Bias::Left),
        matches!(direction, DiagnosticDirection::Prev),
        SelectionGoal::None,
    );
    UpdateEffect::Redraw
}

/// Discriminator for the goto-style LSP requests that all return
/// `Option<GotoDefinitionResponse>` (a single Location or list of
/// candidates) and feed the same `Stoat::pending_lsp_jump` slot.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LspJumpKind {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
}

impl LspJumpKind {
    fn feature(self) -> LanguageServerFeature {
        match self {
            Self::Definition => LanguageServerFeature::GotoDefinition,
            Self::Declaration => LanguageServerFeature::GotoDeclaration,
            Self::TypeDefinition => LanguageServerFeature::GotoTypeDefinition,
            Self::Implementation => LanguageServerFeature::GotoImplementation,
        }
    }

    fn warn_label(self) -> &'static str {
        match self {
            Self::Definition => "goto_definition",
            Self::Declaration => "goto_declaration",
            Self::TypeDefinition => "goto_type_definition",
            Self::Implementation => "goto_implementation",
        }
    }

    fn status_label(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Declaration => "declaration",
            Self::TypeDefinition => "type definition",
            Self::Implementation => "implementation",
        }
    }
}

/// Issue a `textDocument/definition` request for the symbol under the
/// focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_definition(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::Definition)
}

/// Issue a `textDocument/declaration` request for the symbol under the
/// focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_declaration(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::Declaration)
}

/// Issue a `textDocument/typeDefinition` request for the symbol under
/// the focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_type_definition(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::TypeDefinition)
}

/// Issue a `textDocument/implementation` request for the symbol under
/// the focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_implementation(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::Implementation)
}

/// Issue a `textDocument/references` request for the symbol under the
/// focused editor's primary cursor and feed the results to the
/// multi-location picker via [`Stoat::pending_lsp_jump`]. A single
/// reference jumps directly. Several open the picker. The declaration is
/// included, matching the common editor default.
///
/// Falls back to code-graph reference navigation
/// ([`crate::code_index::nav::goto_references`]) when the server does not
/// advertise `references`, so references keep working with no language
/// server. No-op when the focused pane is not an editor, its buffer has
/// no path, or a review cursor does not map to a file line.
pub(crate) fn goto_references(stoat: &mut Stoat) -> UpdateEffect {
    let Some(site) = lsp_request_site(stoat) else {
        return UpdateEffect::None;
    };
    let hosts = crate::lsp::hosts::feature_hosts(
        stoat,
        site.buffer_id,
        LanguageServerFeature::GotoReference,
    );
    if hosts.is_empty() {
        return crate::code_index::nav::goto_references(stoat);
    }
    let Some(source_uri) = path_to_uri(&site.path) else {
        return UpdateEffect::None;
    };

    let fs = stoat.fs_host.clone();
    let LspRequestSite {
        buffer_id,
        path: source_path,
        rope: source_rope,
        offset,
    } = site;
    // The position was measured after edits whose change may still be sitting in
    // its debounce, and a server cannot place a position in text it has not been
    // sent.
    let pending_change = crate::lsp::sync::flush_pending_did_change(stoat, buffer_id);
    let task = stoat.spawn_woken(async move {
        if let Some(pending_change) = pending_change {
            pending_change.await;
        }
        let requests = hosts.iter().map(|(_, host)| {
            let encoding = host.offset_encoding();
            let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, offset, encoding);
            let params = ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            };
            async move { (encoding, host.references(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(locations)) => entries.extend(resolve_goto_targets(
                    GotoDefinitionResponse::Array(locations),
                    &source_path,
                    &source_rope,
                    encoding,
                    &*fs,
                )),
                Ok(None) => {},
                Err(err) => tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    "references request failed",
                ),
            }
        }
        dedup_locations(entries)
    });
    stoat.pending_lsp_jump = Some(("references", task));
    UpdateEffect::None
}

/// Resolve a focused review editor's cursor to the real working-tree file it
/// mirrors, readying that file for an LSP request.
///
/// Ensures the file's buffer is open and did-opened (no pane swap), then
/// returns its path, rope, and the cursor's byte offset in it. This is what
/// lets hover and goto work from the side-by-side diff, whose own buffer is a
/// pathless placeholder the language server knows nothing about. `None` when
/// the cursor is not on a new-side line or the source is not a working tree
/// (see [`review::review_cursor_file_position`]).
pub(crate) fn review_lsp_source(stoat: &mut Stoat) -> Option<(PathBuf, Rope, usize)> {
    let (path, line, col) = super::review::review_cursor_file_position(stoat)?;
    let content = super::read_string_via_host(&*stoat.fs_host, &path).ok()?;
    let lang = stoat.language_registry.for_path(&path);

    let (buffer_id, buffer) = {
        let ws = stoat.active_workspace_mut();
        let (buffer_id, buffer) = ws.buffers.open(&path, &content);
        if let Some(lang) = lang
            && ws.buffers.language_for(buffer_id).is_none()
        {
            ws.buffers.set_language(buffer_id, lang);
        }
        (buffer_id, buffer)
    };
    let workspace = stoat.active_workspace;
    let rope = buffer.read().expect("buffer lock").rope().clone();
    crate::lsp::session::notify_buffer_opened(stoat, workspace, buffer_id, &path, rope.clone());

    let offset = rope.point_to_offset(Point::new(line, col));
    Some((path, rope, offset))
}

/// The focused editor's cursor resolved to an LSP request site: the
/// source file, its rope, and the cursor's byte offset into it.
struct LspRequestSite {
    buffer_id: BufferId,
    path: PathBuf,
    rope: Rope,
    offset: usize,
}

/// Resolve the focused editor's cursor to an [`LspRequestSite`] for a
/// position-based request.
///
/// A working-tree review cursor resolves to the real file it mirrors via
/// [`review_lsp_source`], so requests target disk content rather than the
/// diff placeholder. Returns `None` when the focused pane is not an editor,
/// its buffer has no path, or a review cursor does not map to a file line.
fn lsp_request_site(stoat: &mut Stoat) -> Option<LspRequestSite> {
    let (focused_offset, buffer_id, focused_rope, is_review) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

    if is_review {
        let (path, rope, offset) = review_lsp_source(stoat)?;
        Some(LspRequestSite {
            buffer_id,
            path,
            rope,
            offset,
        })
    } else {
        let path = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)?;
        Some(LspRequestSite {
            buffer_id,
            path,
            rope: focused_rope,
            offset: focused_offset,
        })
    }
}

/// Issue an LSP jump-style request (definition / type definition /
/// implementation / declaration) for the symbol under the focused
/// editor's primary cursor. The async response is stored on
/// [`Stoat::pending_lsp_jump`] and applied by [`pump_lsp_jumps`] on
/// the next render tick.
///
/// From a working-tree review the cursor resolves to the real file via
/// [`review_lsp_source`], so the request targets disk content, not the diff
/// placeholder.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise the matching
/// [`LanguageServerFeature`], reports the language-server state to the
/// status bar via [`report_lsp_unavailable`] instead of doing nothing.
///
/// Replacing the prior pending task drops it and cancels its spawned
/// future, so only one in-flight jump is tracked at a time.
fn lsp_jump(stoat: &mut Stoat, kind: LspJumpKind) -> UpdateEffect {
    let Some(site) = lsp_request_site(stoat) else {
        return UpdateEffect::None;
    };
    let hosts = crate::lsp::hosts::feature_hosts(stoat, site.buffer_id, kind.feature());
    if hosts.is_empty() {
        return crate::lsp::session::report_lsp_unavailable(
            stoat,
            &format!("goto {}", kind.status_label()),
        );
    }
    let Some(source_uri) = path_to_uri(&site.path) else {
        return UpdateEffect::None;
    };

    let fs = stoat.fs_host.clone();
    let LspRequestSite {
        buffer_id,
        path: source_path,
        rope: source_rope,
        offset,
    } = site;
    // The position was measured after edits whose change may still be sitting in
    // its debounce, and a server cannot place a position in text it has not been
    // sent.
    let pending_change = crate::lsp::sync::flush_pending_did_change(stoat, buffer_id);
    let task = stoat.spawn_woken(async move {
        if let Some(pending_change) = pending_change {
            pending_change.await;
        }
        let requests = hosts.iter().map(|(_, host)| {
            let encoding = host.offset_encoding();
            let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, offset, encoding);
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            async move {
                let result = match kind {
                    LspJumpKind::Definition => host.goto_definition(params).await,
                    LspJumpKind::Declaration => host.goto_declaration(params).await,
                    LspJumpKind::TypeDefinition => host.goto_type_definition(params).await,
                    LspJumpKind::Implementation => host.goto_implementation(params).await,
                };
                (encoding, result)
            }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => entries.extend(resolve_goto_targets(
                    response,
                    &source_path,
                    &source_rope,
                    encoding,
                    &*fs,
                )),
                Ok(None) => {},
                Err(err) => tracing::warn!(
                    target: "stoat::lsp",
                    request = kind.warn_label(),
                    ?err,
                    "lsp jump request failed",
                ),
            }
        }
        dedup_locations(entries)
    });
    stoat.pending_lsp_jump = Some((kind.status_label(), task));
    UpdateEffect::None
}

/// Resolve every candidate in a `GotoDefinitionResponse` into a
/// [`LocationEntry`]. A single-target response yields one entry (the
/// caller jumps directly); a multi-target response yields several (the
/// caller opens a picker). Candidates whose URI is not a `file:` path,
/// or whose target file cannot be read, are dropped rather than
/// aborting the whole batch, so one bad location does not sink the rest.
///
/// Same-file targets reuse the supplied source rope. Cross-file targets
/// read the destination through the supplied [`crate::host::FsHost`] so
/// a closed buffer still resolves without round-tripping through
/// `Stoat`. Each entry carries the byte offset after applying the
/// host's negotiated [`OffsetEncoding`], the 1-based line and column,
/// and the trimmed text of the target line for display.
fn resolve_goto_targets(
    response: GotoDefinitionResponse,
    source_path: &Path,
    source_rope: &Rope,
    encoding: OffsetEncoding,
    fs: &dyn crate::host::FsHost,
) -> Vec<LocationEntry> {
    let candidates: Vec<(Uri, Position)> = match response {
        GotoDefinitionResponse::Scalar(loc) => vec![(loc.uri, loc.range.start)],
        GotoDefinitionResponse::Array(locs) => locs
            .into_iter()
            .map(|loc| (loc.uri, loc.range.start))
            .collect(),
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| (link.target_uri, link.target_range.start))
            .collect(),
    };

    candidates
        .into_iter()
        .filter_map(|(uri, position)| {
            resolve_one_target(uri, position, source_path, source_rope, encoding, fs)
        })
        .collect()
}

fn resolve_one_target(
    uri: Uri,
    position: Position,
    source_path: &Path,
    source_rope: &Rope,
    encoding: OffsetEncoding,
    fs: &dyn crate::host::FsHost,
) -> Option<LocationEntry> {
    let target_path = crate::lsp::util::lsp_uri_to_path(&uri)?;

    let (offset, text) = if target_path == source_path {
        (
            crate::lsp::util::lsp_pos_to_byte_offset(source_rope, position, encoding),
            line_text(source_rope, position.line),
        )
    } else {
        let file_text = match super::read_string_via_host(fs, &target_path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::lsp",
                    path = %target_path.display(),
                    ?err,
                    "goto target file unreadable",
                );
                return None;
            },
        };
        let target_rope = Rope::from(file_text.as_str());
        let offset = crate::lsp::util::lsp_pos_to_byte_offset(&target_rope, position, encoding);
        (offset, line_text(&target_rope, position.line))
    };

    Some(LocationEntry {
        path: target_path,
        offset,
        line: position.line + 1,
        column: position.character + 1,
        text,
    })
}

/// Drop duplicate goto targets, keeping the first occurrence of each
/// `(path, offset)` in order.
///
/// Fanning a goto request out to every capable server routinely surfaces the
/// same definition twice (two servers indexing one crate answer identically).
/// Deduplicating keeps a redundant answer from opening a multi-location picker
/// over what is really one target. Order is preserved, so the highest-priority
/// server's copy of a shared target is the one kept.
fn dedup_locations(entries: Vec<LocationEntry>) -> Vec<LocationEntry> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert((entry.path.clone(), entry.offset)))
        .collect()
}

/// The trimmed text of `line` (0-based) in `rope`, for display in the
/// location picker. Returns an empty string when the line is out of
/// range so a stale position never panics.
fn line_text(rope: &Rope, line: u32) -> String {
    let start = rope.point_to_offset(Point::new(line, 0));
    let end = rope
        .point_to_offset(Point::new(line + 1, 0))
        .min(rope.len());
    rope.slice(start..end).to_string().trim().to_string()
}

/// Debounce before requesting inlay hints, so a burst of edits or scrolls
/// collapses into a single viewport request.
const INLAY_HINT_DEBOUNCE: Duration = Duration::from_millis(100);

/// One resolved inlay hint ready to splice into the display map. It bundles a
/// byte offset in the request-time buffer with the rendered text and the kind.
pub(crate) type InlayHintItem = (usize, String, InlayKind);

/// A completed inlay-hint request's payload. It carries the buffer the request
/// targeted and the hints resolved for its viewport.
pub(crate) type InlayHintResponse = (BufferId, Vec<InlayHintItem>);

/// Everything a viewport inlay-hint request carries. It names the target buffer
/// and version, the visible display-row window, the rope for offset conversion,
/// and the built request params.
struct InlayHintRequest {
    buffer_id: BufferId,
    version: u64,
    scroll_row: u32,
    end_row: u32,
    rope: Rope,
    params: InlayHintParams,
}

/// Request inlay hints for the focused editor's viewport when enabled, the
/// server supports them, and the (buffer, version, visible rows) key changed
/// since the last request. Buffer edits and scrolls change the key and
/// re-request. The response is applied by [`pump_lsp_inlay_hints`].
pub(crate) fn inlay_hints_trigger(stoat: &mut Stoat) {
    if !stoat.inlay_hints_enabled {
        return;
    }
    request_inlay_hints(stoat, INLAY_HINT_DEBOUNCE);
}

/// The focused editor's `(buffer_id, version, scroll_row, end_row)` inlay-hint
/// dedupe key, or `None` on a review view, absent editor, or missing viewport.
///
/// Mirrors [`build_inlay_hint_request`]'s editor read without cloning the rope,
/// so the trigger can bail before host resolution.
fn inlay_hint_key(stoat: &mut Stoat) -> Option<(BufferId, u64, u32, u32)> {
    let editor = crate::action_handlers::focused_editor_mut(stoat)?;
    if editor.review_view.is_some() {
        return None;
    }
    let viewport = editor.viewport_rows?;
    let scroll_row = editor.scroll_row;
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let end_row = (scroll_row + viewport).min(snapshot.line_count());
    Some((editor.buffer_id, buf_snap.version(), scroll_row, end_row))
}

/// Issue a viewport inlay-hint request for the focused editor, waiting
/// `debounce` before the server call (pass [`Duration::ZERO`] to skip it).
///
/// Returns whether a server capable of inlay hints was found.
///
/// A capable server still returns `true` without spawning when the request is
/// not viable yet (no viewport, a review view, a scroll glide still in flight)
/// or when the (buffer, version, visible rows) key has not moved. The caller
/// treats inlay hints as available either way, and the per-frame trigger
/// requests once viable. The response is applied by [`pump_lsp_inlay_hints`].
fn request_inlay_hints(stoat: &mut Stoat, debounce: Duration) -> bool {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return false;
    };
    // A wheel notch moves the scroll row, and the key includes it, so a flick
    // would arm and cancel a request per notch and discard every one of them.
    // The viewport the glide lands on is the only one worth asking about, and
    // the settle in `Stoat::frame_tick` triggers for it.
    if crate::action_handlers::focused_editor_mut(stoat)
        .is_some_and(|editor| editor.scroll_glide != ScrollGlide::None)
    {
        return true;
    }
    let Some(key) = inlay_hint_key(stoat) else {
        return true;
    };
    if stoat.last_inlay_hint_key == Some(key) {
        return true;
    }

    let Some((_, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::InlayHints)
            .into_iter()
            .next()
    else {
        return false;
    };
    let encoding = host.offset_encoding();
    let Some(request) = build_inlay_hint_request(stoat, encoding) else {
        return true;
    };

    stoat.last_inlay_hint_key = Some((
        request.buffer_id,
        request.version,
        request.scroll_row,
        request.end_row,
    ));

    let InlayHintRequest {
        buffer_id,
        rope,
        params,
        ..
    } = request;
    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        if !debounce.is_zero() {
            executor.timer(debounce).await;
        }
        match host.range_inlay_hint(params).await {
            Ok(Some(hints)) => Some((buffer_id, convert_inlay_hints(hints, &rope, encoding))),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "inlay_hint request failed");
                None
            },
        }
    });
    stoat.pending_inlay_hint_request.arm(task);
    true
}

/// Enable inlay hints from the ToggleInlayHints action, requesting the focused
/// viewport immediately and acknowledging in the status bar.
///
/// Skips the scroll debounce so hints appear on the keystroke rather than after
/// the settle delay. Reports "inlay hints on" when a capable server was found,
/// even if the request cannot be built yet, since the per-frame trigger issues
/// it once viable. Reports the [`report_lsp_unavailable`] reason otherwise.
pub(crate) fn enable_inlay_hints_now(stoat: &mut Stoat) {
    if request_inlay_hints(stoat, Duration::ZERO) {
        crate::lsp::session::set_lsp_status(stoat, "inlay hints on".to_string());
    } else {
        crate::lsp::session::report_lsp_unavailable(stoat, "inlay hints");
    }
}

fn build_inlay_hint_request(
    stoat: &mut Stoat,
    encoding: OffsetEncoding,
) -> Option<InlayHintRequest> {
    let (buffer_id, version, scroll_row, end_row, rope, start_offset, end_offset) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        if editor.review_view.is_some() {
            return None;
        }
        let viewport = editor.viewport_rows?;
        let scroll_row = editor.scroll_row;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope().clone();
        let end_row = (scroll_row + viewport).min(snapshot.line_count());
        (
            editor.buffer_id,
            buf_snap.version(),
            scroll_row,
            end_row,
            rope.clone(),
            display_row_offset(&snapshot, &rope, scroll_row),
            display_row_offset(&snapshot, &rope, end_row),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = path_to_uri(&path)?;
    let range = Range::new(
        crate::lsp::util::byte_offset_to_lsp_pos(&rope, start_offset, encoding),
        crate::lsp::util::byte_offset_to_lsp_pos(&rope, end_offset, encoding),
    );
    let params = InlayHintParams {
        work_done_progress_params: Default::default(),
        text_document: TextDocumentIdentifier { uri },
        range,
    };

    Some(InlayHintRequest {
        buffer_id,
        version,
        scroll_row,
        end_row,
        rope,
        params,
    })
}

/// Byte offset of the start of display `row`, clamped to the rope length.
fn display_row_offset(snapshot: &DisplaySnapshot, rope: &Rope, row: u32) -> usize {
    let rope_len = rope.len();
    snapshot
        .display_to_buffer(DisplayPoint::new(row, 0))
        .map(|point| rope.point_to_offset(point))
        .unwrap_or(rope_len)
        .min(rope_len)
}

/// Convert LSP inlay hints into [`InlayHintItem`]s using the request-time rope.
/// Both LSP hint kinds render as [`InlayKind::Hint`].
fn convert_inlay_hints(
    hints: Vec<InlayHint>,
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<InlayHintItem> {
    let positions: Vec<Position> = hints.iter().map(|hint| hint.position).collect();
    let offsets = crate::lsp::util::lsp_positions_to_byte_offsets_batch(rope, &positions, encoding);
    hints
        .into_iter()
        .zip(offsets)
        .map(|(hint, offset)| (offset, inlay_hint_text(&hint), InlayKind::Hint))
        .collect()
}

/// The rendered text of a hint. The label is joined when the server sends parts,
/// then wrapped in any requested left or right padding spaces.
fn inlay_hint_text(hint: &InlayHint) -> String {
    let core: String = match &hint.label {
        InlayHintLabel::String(s) => s.clone(),
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|part| part.value.as_str()).collect(),
    };
    let mut text = String::new();
    if hint.padding_left == Some(true) {
        text.push(' ');
    }
    text.push_str(&core);
    if hint.padding_right == Some(true) {
        text.push(' ');
    }
    text
}

/// Poll any in-flight inlay-hint request and splice the results into the focused
/// editor's display map, replacing the buffer's previous hint inlays. Returns
/// true when state changed.
pub(crate) fn pump_lsp_inlay_hints(stoat: &mut Stoat) -> bool {
    let Some(response) = stoat.pending_inlay_hint_request.poll() else {
        return false;
    };
    if let Some((buffer_id, items)) = response {
        apply_inlay_hints(stoat, buffer_id, items);
    }
    true
}

fn apply_inlay_hints(stoat: &mut Stoat, buffer_id: BufferId, items: Vec<InlayHintItem>) {
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let inserts: Vec<(Anchor, String, InlayKind)> = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        items
            .into_iter()
            .map(|(offset, text, kind)| (buf_snap.anchor_at(offset, Bias::Left), text, kind))
            .collect()
    };

    let prev = std::mem::take(&mut editor.hint_inlay_ids);
    editor.hint_inlay_ids = editor.display_map.splice_inlays(prev, inserts);
}

/// Remove every inlay hint from every editor's display map, across all
/// workspaces.
///
/// A hint is spliced into whichever editor was focused when its response
/// applied, so with splits or after switching buffers, hints outlive the moment
/// they were requested and sit in editors that are no longer focused. Once the
/// toggle is off the trigger returns early and never runs again, so a
/// focused-only clear would strand those hints forever. The sweep must reach
/// every editor.
pub(crate) fn clear_inlay_hints(stoat: &mut Stoat) {
    for ws in stoat.workspaces.values_mut() {
        for editor in ws.editors.values_mut() {
            let prev = std::mem::take(&mut editor.hint_inlay_ids);
            if !prev.is_empty() {
                editor.display_map.splice_inlays(prev, Vec::new());
            }
        }
    }
}

/// One actionable entry in [`CodeActionPicker`]. Variants reflect
/// how the entry's effect is obtained: applied from a directly
/// supplied [`WorkspaceEdit`] (with an optional chained command),
/// resolved via a follow-up `codeAction/resolve` call, or dispatched
/// as a `workspace/executeCommand`.
#[derive(Debug, Clone)]
pub(crate) enum CodeActionEntry {
    Direct {
        title: String,
        edit: Box<WorkspaceEdit>,
        command: Option<lsp_types::Command>,
        server: String,
    },
    NeedsResolve {
        title: String,
        action: Box<lsp_types::CodeAction>,
        server: String,
    },
    Command {
        title: String,
        command: lsp_types::Command,
        server: String,
    },
}

impl CodeActionEntry {
    pub(crate) fn title(&self) -> &str {
        match self {
            Self::Direct { title, .. }
            | Self::NeedsResolve { title, .. }
            | Self::Command { title, .. } => title,
        }
    }
}

/// Cursor-anchored code action picker. Painted as a numbered popup
/// over a 9-row viewport that follows [`Self::selected_idx`]; the
/// user navigates with `j`/`k`, picks the selected entry with Enter,
/// picks visible entries 1..=9 with the corresponding digit keys,
/// and dismisses with Escape or any other action.
#[derive(Debug, Clone)]
pub(crate) struct CodeActionPicker {
    pub(crate) entries: Vec<CodeActionEntry>,
    pub(crate) anchor_offset: usize,
    pub(crate) selected_idx: usize,
    /// The routed server that produced these actions, so resolve and execute
    /// route back to it rather than the sole host.
    pub(crate) server: String,
}

/// Issue a `textDocument/codeAction` request for the focused editor's
/// primary selection range. The async response is stored on
/// [`Stoat::pending_code_action_request`] and applied by
/// [`pump_lsp_code_actions`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::CodeAction`], reports the language-server
/// state to the status bar instead. Replacing the prior pending
/// task drops it, cancelling its spawned future -- only one in-flight
/// code-action request is tracked at a time.
pub(crate) fn code_action(stoat: &mut Stoat) -> UpdateEffect {
    let (range_byte, anchor_offset, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let head = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        ((lo, hi), head, editor.buffer_id, buf_snap.rope().clone())
    };

    let Some((server, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::CodeAction)
            .into_iter()
            .next()
    else {
        return crate::lsp::session::report_lsp_unavailable(stoat, "code actions");
    };
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let lsp_range = crate::lsp::util::byte_range_to_lsp_range(
        &source_rope,
        range_byte.0..range_byte.1,
        encoding,
    );

    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        range: lsp_range,
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.code_action(params).await {
            Ok(Some(actions)) => Some(actions),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "code_action request failed");
                None
            },
        }
    });
    stoat.pending_code_action_request = Some(task);
    stoat.pending_code_action_picker = Some(CodeActionPicker {
        entries: Vec::new(),
        anchor_offset,
        selected_idx: 0,
        server,
    });
    // The picker is reset to an empty list above so a stale popup
    // from a prior request does not persist while the new one is
    // in flight; pump_lsp_code_actions overwrites it on response.
    UpdateEffect::None
}

/// Poll any in-flight code-action request
/// ([`Stoat::pending_code_action_request`]) and translate the result
/// into a [`CodeActionPicker`]. Filters out `Command`-only entries
/// and `CodeAction` items that have neither a `WorkspaceEdit` nor a
/// resolve trigger. Clears the picker when no actionable entries
/// remain.
pub(crate) fn pump_lsp_code_actions(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_code_action_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(actions)) => {
            let server = stoat
                .pending_code_action_picker
                .as_ref()
                .map(|picker| picker.server.clone())
                .unwrap_or_default();
            let entries: Vec<CodeActionEntry> = actions
                .into_iter()
                .filter_map(|item| match item {
                    CodeActionOrCommand::CodeAction(ca) => {
                        match (ca.edit.clone(), ca.data.clone(), ca.command.clone()) {
                            (Some(edit), _, command) => Some(CodeActionEntry::Direct {
                                title: ca.title.clone(),
                                edit: Box::new(edit),
                                command,
                                server: server.clone(),
                            }),
                            (None, Some(_), _) => Some(CodeActionEntry::NeedsResolve {
                                title: ca.title.clone(),
                                action: Box::new(ca),
                                server: server.clone(),
                            }),
                            (None, None, Some(command)) => Some(CodeActionEntry::Command {
                                title: ca.title.clone(),
                                command,
                                server: server.clone(),
                            }),
                            (None, None, None) => None,
                        }
                    },
                    CodeActionOrCommand::Command(command) => Some(CodeActionEntry::Command {
                        title: command.title.clone(),
                        command,
                        server: server.clone(),
                    }),
                })
                .collect();
            if entries.is_empty() {
                crate::lsp::session::set_lsp_status(
                    stoat,
                    "lsp: no code actions available".to_string(),
                );
                stoat.pending_code_action_picker = None;
            } else if let Some(picker) = stoat.pending_code_action_picker.as_mut() {
                picker.entries = entries;
            }
            true
        },
        Poll::Ready(None) => {
            crate::lsp::session::set_lsp_status(
                stoat,
                "lsp: no code actions available".to_string(),
            );
            stoat.pending_code_action_picker = None;
            true
        },
        Poll::Pending => {
            stoat.pending_code_action_request = Some(task);
            false
        },
    }
}

/// Poll any in-flight `codeAction/resolve` task
/// ([`Stoat::pending_code_action_resolve`]). On `Ready(Some(edit))`
/// applies the edit via [`crate::lsp::edit_apply::apply_workspace_edit`];
/// errors are logged and swallowed so a malformed edit does not crash
/// the app. On `Ready(None)` the resolve produced no edit, which is a
/// silent no-op.
pub(crate) fn pump_lsp_code_action_resolve(stoat: &mut Stoat) -> bool {
    let Some((requested_at, edit)) = stoat.pending_code_action_resolve.poll() else {
        return false;
    };
    if let Some(edit) = edit
        && !skipped_as_stale(stoat, &requested_at, "code action")
    {
        apply_code_action_edit(stoat, edit, requested_at.encoding());
    }
    true
}

/// Whether the buffer moved since `stamp` was taken, reporting a skipped `what`
/// in the status bar when it did.
///
/// A reply names offsets in the text the request measured. Applying it to a
/// buffer that has changed since puts the edit somewhere the user never asked
/// for, so the reply is dropped and the reason said out loud.
fn skipped_as_stale(stoat: &mut Stoat, stamp: &DocumentStamp, what: &str) -> bool {
    if stamp.is_current(stoat) {
        return false;
    }
    crate::lsp::session::set_lsp_status(stoat, format!("lsp: {what} skipped, buffer changed"));
    true
}

/// Apply a code-action [`WorkspaceEdit`] and log+swallow any error.
/// Code actions arrive from the server and may fail to apply for
/// reasons orthogonal to user action (URI scheme, missing buffer);
/// crashing the app on a server-driven failure is the wrong shape.
fn apply_code_action_edit(stoat: &mut Stoat, edit: WorkspaceEdit, encoding: OffsetEncoding) {
    if let Err(err) = crate::lsp::edit_apply::apply_workspace_edit(stoat, edit, encoding) {
        tracing::warn!(
            target: "stoat::lsp",
            ?err,
            "code_action workspace edit failed to apply",
        );
    }
}

/// User has picked entry `index` from the open code-action picker.
/// `Direct` entries apply immediately; `NeedsResolve` entries spawn
/// a `codeAction/resolve` task whose result is applied by
/// [`pump_lsp_code_action_resolve`]. Clears the picker either way.
/// No-op when no picker is open or `index` is out of range.
pub(crate) fn pick_code_action(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_code_action_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    let buffer_id = stoat.focused_editor_ids().map(|(_, id)| id);
    match entry {
        CodeActionEntry::Direct {
            edit,
            command,
            server,
            ..
        } => {
            let encoding = resolve_code_action_host(stoat, &server, buffer_id).offset_encoding();
            apply_code_action_edit(stoat, *edit, encoding);
            if let Some(command) = command {
                dispatch_execute_command(stoat, &server, buffer_id, command);
            }
        },
        CodeActionEntry::NeedsResolve { action, server, .. } => {
            let lsp = resolve_code_action_host(stoat, &server, buffer_id);
            let encoding = lsp.offset_encoding();
            let task = stoat.spawn_woken(async move {
                match lsp.code_action_resolve(*action).await {
                    Ok(resolved) => resolved.edit,
                    Err(err) => {
                        tracing::warn!(
                            target: "stoat::lsp",
                            ?err,
                            "codeAction/resolve request failed",
                        );
                        None
                    },
                }
            });
            let stamp = buffer_id.and_then(|id| DocumentStamp::take(stoat, id, encoding));
            stoat.pending_code_action_resolve.arm(stamp, task);
        },
        CodeActionEntry::Command {
            command, server, ..
        } => {
            dispatch_execute_command(stoat, &server, buffer_id, command);
        },
    }
    true
}

/// Resolve the host a code action's resolve or command should target: the named
/// producing server, falling back to the buffer's code-action host and then the
/// sole host.
fn resolve_code_action_host(
    stoat: &Stoat,
    server: &str,
    buffer_id: Option<BufferId>,
) -> Arc<dyn LspHost> {
    if let Some(host) = stoat.lsp_registry.client(server) {
        return host;
    }
    match buffer_id {
        Some(id) => {
            crate::lsp::hosts::lsp_for_feature(stoat, id, LanguageServerFeature::CodeAction)
        },
        None => crate::lsp::hosts::lsp_host(stoat),
    }
}

/// Spawn a `workspace/executeCommand` request through
/// [`Stoat::executor`] and detach the task. The result `Option<Value>`
/// is generally a server-side side-effect (servers that produce edits
/// reply via the `workspace/applyEdit` request path); errors are
/// logged and swallowed so a failing command does not crash the app.
fn dispatch_execute_command(
    stoat: &Stoat,
    server: &str,
    buffer_id: Option<BufferId>,
    command: lsp_types::Command,
) {
    let lsp = resolve_code_action_host(stoat, server, buffer_id);
    let label = command.command.clone();
    let params = lsp_types::ExecuteCommandParams {
        command: command.command,
        arguments: command.arguments.unwrap_or_default(),
        work_done_progress_params: Default::default(),
    };
    stoat
        .executor
        .spawn(async move {
            if let Err(err) = lsp.execute_command(params).await {
                tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    command = %label,
                    "workspace/executeCommand request failed",
                );
            }
        })
        .detach();
}

/// Resolved prepare-rename payload carried from the spawned task to
/// [`pump_lsp_prepare_rename`]. Captures both the symbol byte range
/// (so submit can build a `RenameParams` with the right position) and
/// the placeholder text seeded into the input modal.
#[derive(Debug, Clone)]
pub(crate) struct RenamePrep {
    pub(crate) source_uri: Uri,
    pub(crate) symbol_position: Position,
    pub(crate) placeholder: String,
    /// The routed server that answered prepare, carried so submit targets the
    /// same one. `buffer_id` is its fallback route when the name no longer
    /// resolves at submit time.
    pub(crate) server: Option<String>,
    pub(crate) buffer_id: BufferId,
}

/// Open input-modal state for the rename flow. Carries the
/// [`crate::input_view::InputView`] so render can paint the
/// embedded editor and submit can read the typed name; carries
/// the symbol's URI and request position so submit can build the
/// `RenameParams` without touching the editor again.
#[derive(Debug)]
pub(crate) struct RenameInputState {
    pub(crate) input: crate::input_view::InputView,
    pub(crate) source_uri: Uri,
    pub(crate) symbol_position: Position,
    pub(crate) anchor_offset: usize,
    /// The server that answered prepare, resolved again at submit so both halves
    /// of the rename hit the same one. `buffer_id` is the fallback route.
    pub(crate) server: Option<String>,
    pub(crate) buffer_id: BufferId,
}

/// Issue a `textDocument/prepareRename` request for the symbol under
/// the focused editor's primary cursor. The async response is stored
/// on [`Stoat::pending_prepare_rename`] and applied by
/// [`pump_lsp_prepare_rename`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::RenameSymbol`], reports the language-server
/// state to the status bar instead.
pub(crate) fn rename_symbol(stoat: &mut Stoat) -> UpdateEffect {
    let (cursor_offset, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (offset, editor.buffer_id, buf_snap.rope().clone())
    };

    let Some((server, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::RenameSymbol)
            .into_iter()
            .next()
    else {
        return crate::lsp::session::report_lsp_unavailable(stoat, "rename");
    };
    let server = Some(server);
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);

    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        position,
    };

    let task = stoat.spawn_woken(async move {
        let response = match host.prepare_rename(params).await {
            Ok(Some(resp)) => resp,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "prepare_rename request failed");
                return None;
            },
        };
        let placeholder = match response {
            PrepareRenameResponse::Range(range) => {
                let span = crate::lsp::util::lsp_range_to_byte_range(&source_rope, range, encoding);
                source_rope.slice(span).to_string()
            },
            PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => placeholder,
            PrepareRenameResponse::DefaultBehavior { .. } => String::new(),
        };
        Some(RenamePrep {
            source_uri,
            symbol_position: position,
            placeholder,
            server,
            buffer_id,
        })
    });
    stoat.pending_prepare_rename = Some(task);
    UpdateEffect::None
}

/// Poll any in-flight prepare-rename task and, on `Ready(Some)`, open
/// the input modal seeded with the placeholder text. The input is born
/// in insert mode so typing routes through `handle_insert_key` into the
/// modal's [`crate::input_view::InputView`].
pub(crate) fn pump_lsp_prepare_rename(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_prepare_rename.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(prep)) => {
            let anchor_offset = {
                let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
                    return true;
                };
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                let sel = editor.selections.newest_anchor();
                let tail_off = buf_snap.resolve_anchor(&sel.tail());
                let head_off = buf_snap.resolve_anchor(&sel.head());
                stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off)
            };
            let executor = stoat.executor.clone();
            let ws = stoat.active_workspace_mut();
            let input = crate::input_view::InputView::create(
                ws,
                executor,
                crate::input_view::SubmitTarget::RenameSymbol,
                &prep.placeholder,
                "insert",
                1,
            );
            stoat.rename_input = Some(RenameInputState {
                input,
                source_uri: prep.source_uri,
                symbol_position: prep.symbol_position,
                anchor_offset,
                server: prep.server,
                buffer_id: prep.buffer_id,
            });
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_prepare_rename = Some(task);
            false
        },
    }
}

/// Submit the rename input: read the typed text, fire
/// `textDocument/rename`, and tear down the modal. Returns true when
/// the modal was open (so the caller can short-circuit other submit
/// branches).
pub(crate) fn rename_input_submit(stoat: &mut Stoat) -> bool {
    let Some(rename_state) = stoat.rename_input.take() else {
        return false;
    };
    let new_name = rename_state.input.text(stoat.active_workspace());
    let ws = stoat.active_workspace_mut();
    rename_state.input.dispose(ws);

    if new_name.is_empty() {
        return true;
    }

    let params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: rename_state.source_uri,
            },
            position: rename_state.symbol_position,
        },
        new_name,
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let lsp = rename_state
        .server
        .as_deref()
        .and_then(|name| stoat.lsp_registry.client(name))
        .unwrap_or_else(|| {
            crate::lsp::hosts::lsp_for_feature(
                stoat,
                rename_state.buffer_id,
                LanguageServerFeature::RenameSymbol,
            )
        });
    let encoding = lsp.offset_encoding();
    let task = stoat.spawn_woken(async move {
        match lsp.rename(params).await {
            Ok(edit) => edit,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "rename request failed");
                None
            },
        }
    });
    let stamp = DocumentStamp::take(stoat, rename_state.buffer_id, encoding);
    stoat.pending_rename.arm(stamp, task);
    true
}

/// Cancel the rename input modal without firing rename. Disposes the
/// embedded input.
pub(crate) fn rename_input_cancel(stoat: &mut Stoat) -> bool {
    let Some(rename_state) = stoat.rename_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    rename_state.input.dispose(ws);
    true
}

/// Poll any in-flight rename task and apply its [`WorkspaceEdit`].
pub(crate) fn pump_lsp_rename(stoat: &mut Stoat) -> bool {
    let Some((requested_at, edit)) = stoat.pending_rename.poll() else {
        return false;
    };
    if let Some(edit) = edit
        && !skipped_as_stale(stoat, &requested_at, "rename")
    {
        let encoding = requested_at.encoding();
        if let Err(err) = crate::lsp::edit_apply::apply_workspace_edit(stoat, edit, encoding) {
            tracing::warn!(
                target: "stoat::lsp",
                ?err,
                "rename workspace edit failed to apply",
            );
        }
    }
    true
}

/// Issue a `textDocument/documentSymbol` request for the focused
/// buffer. The async response is stored on
/// [`Stoat::pending_symbol_picker_request`] and applied by
/// [`pump_lsp_symbol_picker`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::DocumentSymbols`], reports the
/// language-server state to the status bar instead.
pub(crate) fn open_symbol_picker(stoat: &mut Stoat) -> UpdateEffect {
    let (buffer_id, rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        (editor.buffer_id, buf_snap.rope().clone())
    };

    let hosts =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::DocumentSymbols);
    if hosts.is_empty() {
        return crate::lsp::session::report_lsp_unavailable(stoat, "document symbols");
    }

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let task = stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|(_, host)| {
            let encoding = host.offset_encoding();
            let params = params.clone();
            async move { (encoding, host.document_symbol(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => entries.extend(crate::symbol_finder::symbol_picker_entries(
                    &rope, encoding, response,
                )),
                Ok(None) => {},
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, "document_symbol request failed")
                },
            }
        }
        entries
    });
    stoat.pending_symbol_picker_request = Some(task);
    stoat.set_focused_mode("normal".into());
    let executor = stoat.executor.clone();
    let finder = {
        let ws = stoat.active_workspace_mut();
        SymbolFinder::new(
            ws,
            executor,
            buffer_id,
            SymbolFinderScope::Document,
            Vec::new(),
        )
    };
    stoat.symbol_finder = Some(finder);
    UpdateEffect::None
}

/// Poll any in-flight document-symbol request and fill the open
/// [`SymbolFinder`] with the entries every capable server merged, refiltering
/// against the current query.
///
/// The request task converts and concatenates each server's response, so this
/// only installs the result. An empty result keeps the modal open over an empty
/// list, matching finder behavior.
pub(crate) fn pump_lsp_symbol_picker(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_symbol_picker_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(entries) => {
            let query = symbol_finder_query(stoat);
            if let Some(finder) = stoat.symbol_finder.as_mut() {
                finder.set_entries(entries, &query);
            }
            true
        },
        Poll::Pending => {
            stoat.pending_symbol_picker_request = Some(task);
            false
        },
    }
}

/// The text currently typed into the symbol finder's input, or empty when no
/// finder is open.
fn symbol_finder_query(stoat: &Stoat) -> String {
    stoat
        .symbol_finder
        .as_ref()
        .map(|finder| finder.input.text(stoat.active_workspace()))
        .unwrap_or_default()
}

/// Refilter the open symbol finder against its current input on the render/idle
/// path, so typing narrows the list without a dedicated key handler.
///
/// Document scope filters the fixed list locally. Workspace scope also re-issues
/// `workspace/symbol` whenever the query changes, coalesced to one request in
/// flight. A change while a request is pending sets `query_dirty`, and the pump
/// re-fires with the latest text when the in-flight request lands.
pub(crate) fn sync_symbol_finder(stoat: &mut Stoat) {
    let query = symbol_finder_query(stoat);

    let (reissue, servers, buffer_id) = match stoat.symbol_finder.as_ref() {
        Some(finder) => (
            finder.scope == SymbolFinderScope::Workspace && finder.last_query != query,
            finder.servers.clone(),
            finder.buffer_id,
        ),
        None => return,
    };

    if reissue {
        let in_flight = stoat.pending_workspace_symbol_request.is_some();
        if let Some(finder) = stoat.symbol_finder.as_mut() {
            finder.last_query = query.clone();
            finder.query_dirty = in_flight;
        }
        if !in_flight {
            let task = spawn_workspace_symbol_request(stoat, &servers, buffer_id, query.clone());
            stoat.pending_workspace_symbol_request = Some(task);
        }
    }

    if let Some(finder) = stoat.symbol_finder.as_mut() {
        finder.refilter(&query);
    }

    crate::symbol_finder::sync_symbol_finder_preview(stoat);
    sync_symbol_finder_doc(stoat);
}

/// Fire a hover request for the selected symbol's documentation when the
/// selection changes, coalesced to one request in flight keyed by the filtered
/// index it targets. A moved selection clears the stale doc. The next tick
/// re-fires once the in-flight request lands.
fn sync_symbol_finder_doc(stoat: &mut Stoat) {
    let Some((buffer_id, sel_key, target)) = stoat.symbol_finder.as_ref().map(|finder| {
        let target = finder.selected_entry().map(|e| e.target.clone());
        let sel_key = target.as_ref().map(|_| finder.selected);
        (finder.buffer_id, sel_key, target)
    }) else {
        return;
    };

    let (doc_for, in_flight) = match stoat.symbol_finder.as_ref() {
        Some(finder) => (finder.doc_for, finder.pending_doc.is_some()),
        None => return,
    };

    if doc_for == sel_key {
        return;
    }

    if let Some(finder) = stoat.symbol_finder.as_mut() {
        finder.doc_markdown = None;
        finder.doc_lines = None;
    }
    if in_flight {
        return;
    }

    let Some(target) = target else {
        if let Some(finder) = stoat.symbol_finder.as_mut() {
            finder.doc_for = None;
        }
        return;
    };

    let task = spawn_symbol_doc_request(stoat, buffer_id, &target);
    if let Some(finder) = stoat.symbol_finder.as_mut() {
        finder.pending_doc = task;
        finder.doc_for = sel_key;
    }
}

/// Fire one `textDocument/hover` for a symbol entry's documentation, returning
/// the flattened markdown or `None` on an empty, failed, or unroutable response.
///
/// Document entries convert their buffer offset to an LSP position. Workspace
/// entries use their stored position and path. Best-effort: a server that never
/// received the file may reject the request, which yields `None`.
fn spawn_symbol_doc_request(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    target: &SymbolTarget,
) -> Option<Task<Option<String>>> {
    let (_, host) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::Hover)
            .into_iter()
            .next()?;
    let encoding = host.offset_encoding();

    let (uri, position) = match target {
        SymbolTarget::Offset(offset) => {
            let ws = stoat.active_workspace();
            let path = ws.buffers.path_for(buffer_id).map(Path::to_path_buf)?;
            let uri = path_to_uri(&path)?;
            let rope = ws.buffers.get(buffer_id)?.read().ok()?.rope().clone();
            let position = crate::lsp::util::byte_offset_to_lsp_pos(&rope, *offset, encoding);
            (uri, position)
        },
        SymbolTarget::Workspace { path, position, .. } => (path_to_uri(path)?, *position),
    };

    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    Some(stoat.spawn_woken(async move {
        match host.hover(params).await {
            Ok(Some(hover)) => {
                let (text, _plain) = crate::lsp::hover::flatten_hover_contents(hover.contents);
                (!text.is_empty()).then_some(text)
            },
            _ => None,
        }
    }))
}

/// Poll the in-flight symbol-doc hover and install its markdown, discarding a
/// response whose selection has since moved so the pump's next sync re-fires.
pub(crate) fn pump_symbol_finder_doc(stoat: &mut Stoat) -> bool {
    let Some(finder) = stoat.symbol_finder.as_mut() else {
        return false;
    };
    let Some(mut task) = finder.pending_doc.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(doc) => {
            let sel_key = finder.selected_entry().map(|_| finder.selected);
            if finder.doc_for == sel_key {
                finder.doc_markdown = doc;
                finder.doc_lines = None;
            }
            true
        },
        Poll::Pending => {
            finder.pending_doc = Some(task);
            false
        },
    }
}

/// Apply the user's pick from the open graph-navigation picker, jumping to the
/// entry's symbol and opening another file if needed, and clear the picker.
///
/// No-op when no picker is open or `index` is out of range.
pub(crate) fn pick_symbol(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_symbol_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    crate::code_index::nav::jump_to_symbol(stoat, entry.symbol);
    true
}

/// One workspace-symbol result from a `workspace/symbol` fan-out.
///
/// `title` is the symbol name, `path` the absolute filesystem path to open, and
/// `position` the LSP position in the target file. `encoding` is the offset
/// encoding of the server that produced this entry, so a fan-out across servers
/// that negotiated different encodings still resolves each position on accept.
/// The pump converts these into [`SymbolFinderEntry`] with a
/// [`SymbolTarget::Workspace`] target.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSymbolEntry {
    pub(crate) title: String,
    pub(crate) kind: Option<SymbolKind>,
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
    pub(crate) encoding: OffsetEncoding,
}

/// Convert a workspace-symbol result into a finder entry, taking the display
/// line from the target position and a cross-file [`SymbolTarget::Workspace`].
fn workspace_finder_entry(entry: WorkspaceSymbolEntry) -> SymbolFinderEntry {
    SymbolFinderEntry {
        title: entry.title,
        kind: entry.kind,
        line: entry.position.line,
        target: SymbolTarget::Workspace {
            path: entry.path,
            position: entry.position,
            encoding: entry.encoding,
        },
    }
}

/// Open the workspace-symbol query input modal. When the server does not
/// advertise [`LanguageServerFeature::WorkspaceSymbols`], reports the
/// language-server state to the status bar instead of opening. The input is
/// born in insert mode so typing routes through `handle_insert_key` into the
/// modal's [`crate::input_view::InputView`]. The modal seed is empty;
/// submit fires the request, cancel disposes the input.
pub(crate) fn open_workspace_symbol_picker(stoat: &mut Stoat) -> UpdateEffect {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };
    let servers: Vec<String> =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::WorkspaceSymbols)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
    if servers.is_empty() {
        return crate::lsp::session::report_lsp_unavailable(stoat, "workspace symbols");
    }

    let task = spawn_workspace_symbol_request(stoat, &servers, buffer_id, String::new());
    stoat.pending_workspace_symbol_request = Some(task);
    stoat.set_focused_mode("normal".into());

    let executor = stoat.executor.clone();
    let finder = {
        let ws = stoat.active_workspace_mut();
        SymbolFinder::new(
            ws,
            executor,
            buffer_id,
            SymbolFinderScope::Workspace,
            servers,
        )
    };
    stoat.symbol_finder = Some(finder);
    UpdateEffect::Redraw
}

/// Fire a `workspace/symbol` request for `query` across `servers`, falling back
/// to every capable host on `buffer_id` when the named servers no longer
/// resolve. Each server's response is converted with its own offset encoding
/// and merged into the returned entries.
fn spawn_workspace_symbol_request(
    stoat: &mut Stoat,
    servers: &[String],
    buffer_id: BufferId,
    query: String,
) -> Task<Vec<WorkspaceSymbolEntry>> {
    let params = WorkspaceSymbolParams {
        query,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };

    let mut hosts: Vec<Arc<dyn LspHost>> = servers
        .iter()
        .filter_map(|name| stoat.lsp_registry.client(name))
        .collect();
    if hosts.is_empty() {
        hosts = crate::lsp::hosts::feature_hosts(
            stoat,
            buffer_id,
            LanguageServerFeature::WorkspaceSymbols,
        )
        .into_iter()
        .map(|(_, host)| host)
        .collect();
    }

    stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|host| {
            let encoding = host.offset_encoding();
            let params = params.clone();
            async move { (encoding, host.workspace_symbol(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => entries.extend(workspace_symbol_entries(response, encoding)),
                Ok(None) => {},
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, "workspace_symbol request failed")
                },
            }
        }
        entries
    })
}

/// Poll any in-flight workspace-symbol request and fill the
/// [`WorkspaceSymbolPicker`] with the entries every capable server merged.
///
/// The request task converts and concatenates each server's response, so this
/// only installs the result. Drops the picker when no server returned an entry.
pub(crate) fn pump_lsp_workspace_symbol(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_workspace_symbol_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(entries) => {
            let Some(finder) = stoat.symbol_finder.as_ref() else {
                return true;
            };
            let dirty = finder.query_dirty;
            let servers = finder.servers.clone();
            let buffer_id = finder.buffer_id;
            let query = finder.input.text(stoat.active_workspace());

            let finder_entries: Vec<SymbolFinderEntry> =
                entries.into_iter().map(workspace_finder_entry).collect();
            if let Some(finder) = stoat.symbol_finder.as_mut() {
                finder.set_entries(finder_entries, &query);
                finder.query_dirty = false;
            }
            if dirty {
                let task = spawn_workspace_symbol_request(stoat, &servers, buffer_id, query);
                stoat.pending_workspace_symbol_request = Some(task);
            }
            true
        },
        Poll::Pending => {
            stoat.pending_workspace_symbol_request = Some(task);
            false
        },
    }
}

fn workspace_symbol_entries(
    response: WorkspaceSymbolResponse,
    encoding: OffsetEncoding,
) -> Vec<WorkspaceSymbolEntry> {
    let mut entries: Vec<WorkspaceSymbolEntry> = Vec::new();
    match response {
        WorkspaceSymbolResponse::Flat(items) => {
            for SymbolInformation {
                name,
                location,
                kind,
                ..
            } in items
            {
                let Some(path) = crate::lsp::util::lsp_uri_to_path(&location.uri) else {
                    continue;
                };
                entries.push(WorkspaceSymbolEntry {
                    title: name,
                    kind: Some(kind),
                    path,
                    position: location.range.start,
                    encoding,
                });
            }
        },
        WorkspaceSymbolResponse::Nested(items) => {
            for WorkspaceSymbol {
                name,
                location,
                kind,
                ..
            } in items
            {
                let (uri, position) = match location {
                    OneOf::Left(loc) => (loc.uri, loc.range.start),
                    OneOf::Right(workspace_loc) => {
                        // `WorkspaceLocation` carries no range, so fall back to
                        // the start of file. A future `workspaceSymbol/resolve`
                        // round-trip would refine this.
                        (workspace_loc.uri, Position::new(0, 0))
                    },
                };
                let Some(path) = crate::lsp::util::lsp_uri_to_path(&uri) else {
                    continue;
                };
                entries.push(WorkspaceSymbolEntry {
                    title: name,
                    kind: Some(kind),
                    path,
                    position,
                    encoding,
                });
            }
        },
    }
    entries
}

/// Open a picked workspace symbol's file in the focused pane and jump the
/// primary cursor to `position`, resolved against the file's server `encoding`.
pub(crate) fn open_workspace_symbol_target(
    stoat: &mut Stoat,
    path: &Path,
    position: Position,
    encoding: OffsetEncoding,
) {
    let focused = stoat.active_workspace().panes.focus();
    crate::buffer_lifecycle::open_file_in_pane(stoat, focused, path);

    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let rope = buf_snap.rope().clone();
    let offset = crate::lsp::util::lsp_pos_to_byte_offset(&rope, position, encoding);
    crate::action_handlers::movement::jump_to_offset(stoat, offset);
}

/// Format response carried from the spawned task to
/// [`pump_lsp_format`]. Pairs the target document URI with the
/// returned text edits so the pump can build a single-document
/// [`WorkspaceEdit`].
#[derive(Debug, Clone)]
pub(crate) struct FormatResponse {
    pub(crate) uri: Uri,
    pub(crate) edits: Vec<TextEdit>,
}

/// Issue a `textDocument/rangeFormatting` request for the focused
/// editor's primary selection. The async response is stored on
/// [`Stoat::pending_format_request`] and applied by
/// [`pump_lsp_format`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::Format`], reports the language-server state
/// to the status bar instead.
pub(crate) fn format_selections(stoat: &mut Stoat) -> UpdateEffect {
    let (range_byte, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        ((lo, hi), editor.buffer_id, buf_snap.rope().clone())
    };

    let Some((_, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::Format)
            .into_iter()
            .next()
    else {
        return crate::lsp::session::report_lsp_unavailable(stoat, "format");
    };
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let lsp_range = crate::lsp::util::byte_range_to_lsp_range(
        &source_rope,
        range_byte.0..range_byte.1,
        encoding,
    );

    let params = DocumentRangeFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        range: lsp_range,
        options: stoat.buffer_formatting_options(buffer_id),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.range_formatting(params).await {
            Ok(Some(edits)) if !edits.is_empty() => Some(FormatResponse {
                uri: source_uri,
                edits,
            }),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "range_formatting request failed");
                None
            },
        }
    });
    let stamp = DocumentStamp::take(stoat, buffer_id, encoding);
    stoat.pending_format_request.arm(stamp, task);
    UpdateEffect::None
}

/// Issue a `textDocument/formatting` request for the whole focused
/// document. The async response is stored on
/// [`Stoat::pending_format_request`] and applied by [`pump_lsp_format`]
/// on the next render tick, sharing the single-document apply path with
/// [`format_selections`].
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::Format`], reports the language-server state
/// to the status bar instead.
pub(crate) fn format_document(stoat: &mut Stoat) -> UpdateEffect {
    let Some(buffer_id) = crate::action_handlers::focused_editor_mut(stoat).map(|e| e.buffer_id)
    else {
        return UpdateEffect::None;
    };
    let Some((_, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::Format)
            .into_iter()
            .next()
    else {
        return crate::lsp::session::report_lsp_unavailable(stoat, "format");
    };
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        options: stoat.buffer_formatting_options(buffer_id),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.formatting(params).await {
            Ok(Some(edits)) if !edits.is_empty() => Some(FormatResponse {
                uri: source_uri,
                edits,
            }),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "formatting request failed");
                None
            },
        }
    });
    let stamp = DocumentStamp::take(stoat, buffer_id, encoding);
    stoat.pending_format_request.arm(stamp, task);
    UpdateEffect::None
}

/// Poll any in-flight format request and apply the returned text
/// edits as a single-document [`WorkspaceEdit`]. Errors from
/// [`crate::lsp::edit_apply::apply_workspace_edit`] are logged and
/// swallowed so a malformed edit does not crash the app.
pub(crate) fn pump_lsp_format(stoat: &mut Stoat) -> bool {
    let Some((requested_at, response)) = stoat.pending_format_request.poll() else {
        return false;
    };
    if let Some(FormatResponse { uri, edits }) = response
        && !skipped_as_stale(stoat, &requested_at, "format")
    {
        #[allow(clippy::mutable_key_type)]
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri, edits);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        if let Err(err) =
            crate::lsp::edit_apply::apply_workspace_edit(stoat, edit, requested_at.encoding())
        {
            tracing::warn!(
                target: "stoat::lsp",
                ?err,
                "format text edit failed to apply",
            );
        }
    }
    true
}

/// Poll any in-flight LSP jump request ([`Stoat::pending_lsp_jump`])
/// and dispatch on how many locations resolved. Zero locations reports
/// "lsp: no {label} found" in the status bar, naming the jump kind. One
/// jumps to it directly via [`apply_jump`]. Two or more
/// open a [`LocationPicker`] in [`Stoat::location_picker`] so the user
/// chooses. On `Pending` puts the task back. Returns true when state
/// changed so the caller can request a redraw.
pub(crate) fn pump_lsp_jumps(stoat: &mut Stoat) -> bool {
    let Some((label, mut task)) = stoat.pending_lsp_jump.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(mut entries) => {
            match entries.len() {
                0 => crate::lsp::session::set_lsp_status(stoat, format!("lsp: no {label} found")),
                1 => {
                    let entry = entries.remove(0);
                    apply_jump(stoat, &entry.path, entry.offset);
                },
                _ => {
                    stoat.location_picker = Some(LocationPicker::new(entries));
                },
            }
            true
        },
        Poll::Pending => {
            stoat.pending_lsp_jump = Some((label, task));
            false
        },
    }
}

/// Open `path` in the focused pane and collapse every selection onto
/// `offset`. Opening is a no-op when the file is already the pane's
/// buffer.
///
/// A jump issued from a diff review parks the review session first so
/// the review editor survives the pane swap (the gc guard keeps parked
/// editors) and R re-enters the diff.
pub(crate) fn apply_jump(stoat: &mut Stoat, path: &Path, offset: usize) {
    let from_review =
        crate::action_handlers::focused_editor_mut(stoat).is_some_and(|e| e.review_view.is_some());
    if from_review {
        super::review::park_review_session(stoat);
        stoat.set_focused_mode("normal".to_string());
    } else {
        super::jump::push_jump(stoat);
    }

    let buffer_before =
        crate::action_handlers::focused_editor_mut(stoat).map(|editor| editor.buffer_id);

    let focused = stoat.active_workspace().panes.focus();
    crate::buffer_lifecycle::open_file_in_pane(stoat, focused, path);
    super::movement::jump_to_offset(stoat, offset);

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };

    // Landing in another file means a freshly shown editor with no prior view to
    // glide from, so it snaps.
    if Some(editor.buffer_id) == buffer_before {
        super::view::follow_jump(editor, scrolloff);
    } else {
        super::view::ensure_cursor_in_view(editor, scrolloff);
    }
}

/// Convert an absolute filesystem path to an `lsp_types::Uri`. Returns
/// `None` for paths that cannot be encoded as a `file://` URI (e.g.
/// non-UTF-8 paths). Mirrors the production behaviour Helix uses
/// internally; LSP servers expect `file:` URIs for local files.
pub(crate) fn path_to_uri(path: &Path) -> Option<Uri> {
    let encoded = crate::lsp::util::percent_encode_path(path.to_str()?);
    Uri::from_str(&format!("file://{encoded}")).ok()
}

#[cfg(test)]
mod tests {
    use crate::{
        test_fixture::{
            diag, enable_document_symbols, enable_document_symbols_and_hover,
            enable_workspace_symbols, flat_symbol, install_two_servers, open_buffer, seed,
        },
        test_harness::TestHarness,
    };
    use ratatui::style::Style;
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };
    use stoat_action::OpenFile;

    /// The layout reads the stored width instead of measuring, so it has to be
    /// what measuring would have found. The fixture puts the widest line in the
    /// middle and splits lines across spans, since taking the first, the last,
    /// or one span per line all give a plausible wrong answer.
    #[test]
    fn a_popup_stores_the_width_measuring_its_lines_would_find() {
        use crate::{
            editor_state::EditorId,
            render::{hover, hover::HoverPopup},
        };

        let span = |text: &str| (text.to_string(), Style::default());
        let lines = vec![
            vec![span("short")],
            vec![span("four"), span("teen chars")],
            vec![span("middling")],
        ];

        let popup = HoverPopup::new(lines.clone(), 0, EditorId::default());
        assert_eq!(popup.max_line_width, 14);
        assert_eq!(
            popup.max_line_width,
            lines
                .iter()
                .map(|line| hover::line_width(line))
                .max()
                .unwrap_or(0),
        );

        assert_eq!(
            HoverPopup::new(Vec::new(), 0, EditorId::default()).max_line_width,
            0,
            "an empty body measures zero rather than panicking"
        );
    }

    #[test]
    fn lsp_for_feature_routes_to_the_capable_server() {
        use crate::{
            host::{LanguageServerFeature, LspHost},
            lsp::registry::ServerSelector,
        };
        use lsp_types::{CompletionOptions, HoverProviderCapability, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let hover_server = std::sync::Arc::new(crate::host::FakeLsp::new());
        hover_server.set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        let completion_server = std::sync::Arc::new(crate::host::FakeLsp::new());
        completion_server.set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), hover_server.clone());
        h.stoat
            .lsp_registry
            .insert("tailwind".into(), completion_server.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("tailwind".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("a.rs"))
            .expect("buffer open");

        let hover: std::sync::Arc<dyn LspHost> = hover_server.clone();
        let completion: std::sync::Arc<dyn LspHost> = completion_server.clone();
        assert!(
            std::sync::Arc::ptr_eq(
                &crate::lsp::hosts::lsp_for_feature(&h.stoat, id, LanguageServerFeature::Hover),
                &hover,
            ),
            "hover routes to the hover-capable server"
        );
        assert!(
            std::sync::Arc::ptr_eq(
                &crate::lsp::hosts::lsp_for_feature(
                    &h.stoat,
                    id,
                    LanguageServerFeature::Completion
                ),
                &completion,
            ),
            "completion routes to the completion-capable server"
        );
    }

    #[test]
    fn goto_definition_routes_to_a_secondary_when_the_primary_lacks_it() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{OneOf, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        secondary.set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert_eq!(
            cursor_offset(&mut h),
            8,
            "the capable secondary served goto-definition"
        );
    }

    /// Install two definition-capable fakes routed primary-then-secondary for
    /// `rust`, open a three-line buffer, and return the fakes and its path.
    fn two_definition_servers(
        h: &mut TestHarness,
    ) -> (
        std::sync::Arc<crate::host::FakeLsp>,
        std::sync::Arc<crate::host::FakeLsp>,
        PathBuf,
    ) {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{OneOf, ServerCapabilities};

        let caps = ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        };
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(caps.clone());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(caps);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(h, path.clone());
        (primary, secondary, path)
    }

    #[test]
    fn goto_definition_merges_distinct_locations_from_two_servers() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_definition_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_definition(p, 0, 0, p, 1, 0);
        secondary.set_definition(p, 0, 0, p, 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        let offsets: Vec<usize> = picker.entries().iter().map(|e| e.offset).collect();
        assert_eq!(offsets, vec![4, 8], "both servers' targets, primary first");
    }

    #[test]
    fn goto_definition_dedups_a_shared_location() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_definition_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_definition(p, 0, 0, p, 2, 0);
        secondary.set_definition(p, 0, 0, p, 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert!(
            h.stoat.location_picker.is_none(),
            "identical answers dedup to a single direct jump"
        );
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn goto_definition_survives_a_failing_server() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_definition_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_method_error("textDocument/definition", std::io::ErrorKind::Other);
        secondary.set_definition(p, 0, 0, p, 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert!(h.stoat.location_picker.is_none());
        assert_eq!(
            cursor_offset(&mut h),
            8,
            "the healthy server's answer lands despite the peer erroring"
        );
    }

    #[test]
    fn workspace_symbol_submit_uses_the_host_stashed_at_open() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{OneOf, ServerCapabilities, SymbolKind};

        let mut h = TestHarness::with_size(80, 24);
        let capable = std::sync::Arc::new(crate::host::FakeLsp::new());
        capable.set_capabilities(ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        });
        let other = std::sync::Arc::new(crate::host::FakeLsp::new());
        other.set_capabilities(ServerCapabilities::default());
        h.stoat
            .lsp_registry
            .insert("capable".into(), capable.clone());
        h.stoat.lsp_registry.insert("other".into(), other.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("capable".into()),
                ServerSelector::all("other".into()),
            ],
        );

        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let main = root.join("main.rs");
        open_buffer(&mut h, main.clone());
        capable.add_workspace_symbol(
            "f",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();

        // The capable server drops workspace symbols mid-query, so re-resolving
        // by capability would find no capable server. The query-change re-issue
        // must still target the server stashed at open, resolved by name.
        capable.set_capabilities(ServerCapabilities::default());
        h.type_keys("f");
        h.settle();

        let finder = h
            .stoat
            .symbol_finder
            .as_ref()
            .expect("finder filled from the stashed server");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["foo"]);
    }

    #[test]
    fn drive_background_applies_pushed_diagnostics() {
        use crate::host::lsp::LspNotification;
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\n")]);
        let path = root.join("main.rs");
        let uri = super::path_to_uri(&path).expect("file uri");
        h.fake_lsp()
            .push_notification(LspNotification::Diagnostics {
                uri,
                diagnostics: vec![diag(0, 0, "boom")],
                version: None,
            });

        // No input event and no settle(): the background pass alone (the
        // redraw-wake path) must drain the pushed notification and apply it.
        h.stoat.drive_background();

        assert_eq!(h.stoat.diagnostics.get(&path), &[diag(0, 0, "boom")]);
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buffer_snapshot.rope(),
            buffer_snapshot.resolve_anchor(&sel.tail()),
            buffer_snapshot.resolve_anchor(&sel.head()),
        )
    }

    #[test]
    fn goto_next_diagnostic_jumps_forward() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 4);
    }

    /// The ends of the list are taken whatever the cursor is near, so a cursor
    /// already past the first still reaches back to it.
    #[test]
    fn goto_first_diagnostic_reaches_back_past_the_cursor() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoLastDiagnostic);
        assert_eq!(cursor_offset(&mut h), 8, "test setup: on the last");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoFirstDiagnostic);
        assert_eq!(cursor_offset(&mut h), 4);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            cursor_offset(&mut h),
            8,
            "the origin went on the jumplist before the landing",
        );
    }

    /// A buffer with nothing to go to moves nothing, and leaves the jumplist
    /// alone rather than recording a jump that went nowhere.
    #[test]
    fn goto_first_diagnostic_with_none_pushes_no_jump() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        // A known entry to land on. A push by the no-op takes its place as what
        // the jump back reaches.
        h.type_keys("l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        assert_eq!(cursor_offset(&mut h), 3, "test setup: away from the entry");

        assert_eq!(
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoFirstDiagnostic),
            crate::app::UpdateEffect::None,
        );
        assert_eq!(cursor_offset(&mut h), 3, "the cursor stayed put");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            cursor_offset(&mut h),
            1,
            "the jump back reaches the earlier entry, so the no-op pushed none",
        );
    }

    #[test]
    fn goto_next_diagnostic_steps_through_each() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 4);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn goto_diagnostic_converts_each_servers_position_with_its_encoding() {
        use crate::host::OffsetEncoding;
        let mut h = TestHarness::with_size(80, 24);
        let ra = std::sync::Arc::new(crate::host::FakeLsp::new());
        ra.set_offset_encoding(OffsetEncoding::Utf8);
        let clippy = std::sync::Arc::new(crate::host::FakeLsp::new());
        clippy.set_offset_encoding(OffsetEncoding::Utf16);
        h.stoat.lsp_registry.insert("ra".into(), ra);
        h.stoat.lsp_registry.insert("clippy".into(), clippy);

        // Line 0 "éx" and line 1 "éy": é is two UTF-8 bytes but one UTF-16 unit,
        // so x sits at byte 2 and y at byte 6.
        let root = seed(&mut h, &[("a.rs", "\u{e9}x\n\u{e9}y\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());

        // ra (utf-8) names x at char 2; clippy (utf-16) names y at char 1.
        h.stoat.diagnostics.replace_from_server(
            path.clone(),
            "ra".into(),
            vec![diag(0, 2, "ra")],
            crate::lsp::util::publish_spans(
                &path,
                &[diag(0, 2, "ra")],
                OffsetEncoding::Utf8,
                &h.stoat.active_workspace().buffers,
            ),
        );
        let path2 = path.clone();
        h.stoat.diagnostics.replace_from_server(
            path,
            "clippy".into(),
            vec![diag(1, 1, "clippy")],
            crate::lsp::util::publish_spans(
                &path2,
                &[diag(1, 1, "clippy")],
                OffsetEncoding::Utf16,
                &h.stoat.active_workspace().buffers,
            ),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 2, "ra's utf-8 column lands on x");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(
            cursor_offset(&mut h),
            6,
            "clippy's utf-16 column lands on y"
        );
    }

    #[test]
    fn goto_next_diagnostic_no_op_after_last() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(0, 0, "only")]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 11);
    }

    #[test]
    fn goto_prev_diagnostic_jumps_backward() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(0, 0, "first"), diag(2, 0, "third")]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 8);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn goto_prev_diagnostic_no_op_before_first() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(2, 0, "only")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn diagnostics_picker_enter_jumps_focused_cursor() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
        assert!(h.stoat.diagnostics_picker.is_some());

        h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert!(h.stoat.diagnostics_picker.is_none());
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn diagnostics_picker_esc_closes_without_jumping() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first")]);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(h.stoat.diagnostics_picker.is_none());
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn goto_diagnostic_no_op_with_empty_diagnostics() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn space_l_w_jumps_to_next_diagnostic() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        h.type_keys("space l w");
        assert_eq!(cursor_offset(&mut h), 4);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    /// A diagnostic several columns wide, since the shared one-column helper
    /// leaves a span a bare block cursor reads the same as.
    fn wide_diag(line: u32, col: u32, width: u32) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(line, col),
                lsp_types::Position::new(line, col + width),
            ),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            ..Default::default()
        }
    }

    /// Stepping to a diagnostic selects its whole span rather than landing a
    /// bare cursor where it opens.
    #[test]
    fn goto_next_diagnostic_selects_the_span() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abcdef\nghijkl\nmnopqr\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![wide_diag(1, 1, 3), wide_diag(2, 2, 3)]);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(
            h.selection_spans(),
            vec![(8, 11, false)],
            "the first diagnostic's span, forward",
        );
    }

    /// Stepping back leaves the span reversed, which puts the cursor on its
    /// start so a repeat carries on the way it went.
    #[test]
    fn goto_prev_diagnostic_selects_reversed() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abcdef\nghijkl\nmnopqr\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![wide_diag(1, 1, 3), wide_diag(2, 2, 3)]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 20);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(h.selection_spans(), vec![(16, 19, true)]);
    }

    /// The motion records where it left, so a jump back returns to the reading
    /// position rather than to the diagnostic.
    #[test]
    fn goto_next_diagnostic_pushes_a_jump() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abcdef\nghijkl\nmnopqr\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![wide_diag(1, 1, 3)]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 10, "the span's last cell");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(cursor_offset(&mut h), 2);
    }

    /// Alt-. after a diagnostic jump repeats the jump, not the find before it.
    #[test]
    fn repeat_last_motion_replays_a_diagnostic_jump() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);

        h.type_keys("f b");
        assert_eq!(cursor_offset(&mut h), 1, "the find lands first");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 4);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RepeatLastMotion);
        assert_eq!(
            cursor_offset(&mut h),
            8,
            "the second diagnostic, where replaying the find would hold",
        );
    }

    #[test]
    fn space_l_shift_w_jumps_to_prev_diagnostic() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.seed_diagnostics(path, vec![diag(0, 0, "first"), diag(2, 0, "third")]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
        h.type_keys("space l shift-w");
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_goto_definition(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn enable_goto_references(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            references_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn focused_buffer_path(h: &TestHarness) -> PathBuf {
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(eid) = pane.view else {
            panic!("focused pane is not an editor");
        };
        let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
        ws.buffers
            .path_for(buffer_id)
            .expect("focused buffer has path")
            .to_path_buf()
    }

    #[test]
    fn goto_definition_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert!(
            h.stoat.location_picker.is_none(),
            "single target skips picker"
        );
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    fn enable_goto_declaration(h: &TestHarness) {
        use lsp_types::{DeclarationCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            declaration_provider: Some(DeclarationCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn goto_declaration_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_declaration(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDeclaration);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_declaration_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDeclaration);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn space_l_shift_j_jumps_to_declaration() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_declaration(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("space l J");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn goto_definition_multiple_targets_opens_picker() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(
            &mut h,
            &[
                ("main.rs", "abc\n"),
                ("lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n"),
            ],
        );
        let main_path = root.join("main.rs");
        let lib_path = root.join("lib.rs");
        open_buffer(&mut h, main_path.clone());
        let lib = lib_path.to_str().unwrap();
        h.fake_lsp().set_definitions(
            main_path.to_str().unwrap(),
            0,
            0,
            &[(lib, 0, 3), (lib, 1, 3), (lib, 2, 3)],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        assert_eq!(picker.entries().len(), 3);
        assert_eq!(
            focused_buffer_path(&h),
            main_path,
            "picker does not jump yet"
        );

        h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        h.settle();

        assert!(h.stoat.location_picker.is_none());
        assert_eq!(focused_buffer_path(&h), lib_path);
        assert_eq!(cursor_offset(&mut h), 15);
    }

    #[test]
    fn goto_definition_opens_target_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(
            &mut h,
            &[
                ("main.rs", "abc\n"),
                ("lib.rs", "fn one() {}\nfn two() {}\n"),
            ],
        );
        let main_path = root.join("main.rs");
        let lib_path = root.join("lib.rs");
        open_buffer(&mut h, main_path.clone());
        h.fake_lsp().set_definition(
            main_path.to_str().unwrap(),
            0,
            0,
            lib_path.to_str().unwrap(),
            1,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert_eq!(focused_buffer_path(&h), lib_path);
        assert_eq!(cursor_offset(&mut h), 15);
    }

    #[test]
    fn goto_definition_no_result_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert_eq!(focused_buffer_path(&h), path);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no definition found"),
        );
    }

    #[test]
    fn in_flight_code_action_shows_a_status_segment() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        // Hold the response open so the request stays in flight through render.
        h.fake_lsp()
            .set_request_delay("textDocument/codeAction", Duration::from_secs(60));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(
            h.stoat.pending_code_action_request.is_some(),
            "the delayed code-action request stays in flight",
        );

        let buf = h.render_composited();
        let shown = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.replace('─', " ").contains("lsp: code actions...")
        });
        assert!(
            shown,
            "the status bar shows the in-flight code-action segment"
        );
    }

    #[test]
    fn code_action_no_result_reports_none_available() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no code actions available"),
        );
    }

    #[test]
    fn goto_definition_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn goto_references_multiple_opens_picker() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_references(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_references(p, 0, 0, &[(p, 0, 0), (p, 1, 0), (p, 2, 0)]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoReferences);
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        assert_eq!(picker.entries().len(), 3);

        h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        h.settle();

        assert!(h.stoat.location_picker.is_none());
        assert_eq!(cursor_offset(&mut h), 4);
    }

    #[test]
    fn goto_references_unsupported_uses_code_graph() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_references(p, 0, 0, &[(p, 0, 0), (p, 1, 0), (p, 2, 0)]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoReferences);
        h.settle();

        assert!(h.stoat.location_picker.is_none(), "LSP path is gated off");
        assert!(h.stoat.pending_lsp_jump.is_none());
        assert_eq!(
            cursor_offset(&mut h),
            0,
            "code-graph fallback no-ops on empty graph"
        );
    }

    #[test]
    fn space_l_j_jumps_to_definition() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("space l j");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_goto_type_definition(h: &TestHarness) {
        use lsp_types::{ServerCapabilities, TypeDefinitionProviderCapability};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn goto_type_definition_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_type_definition(
            path.to_str().unwrap(),
            0,
            0,
            path.to_str().unwrap(),
            2,
            0,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_type_definition_opens_target_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(
            &mut h,
            &[
                ("main.rs", "abc\n"),
                ("types.rs", "struct One;\nstruct Two;\n"),
            ],
        );
        let main_path = root.join("main.rs");
        let types_path = root.join("types.rs");
        open_buffer(&mut h, main_path.clone());
        h.fake_lsp().set_type_definition(
            main_path.to_str().unwrap(),
            0,
            0,
            types_path.to_str().unwrap(),
            1,
            7,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(focused_buffer_path(&h), types_path);
        assert_eq!(cursor_offset(&mut h), 19);
    }

    #[test]
    fn goto_type_definition_no_result_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_type_definition_unsupported_capability_is_noop() {
        use lsp_types::{OneOf, ServerCapabilities};
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_type_definition(
            path.to_str().unwrap(),
            0,
            0,
            path.to_str().unwrap(),
            0,
            2,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn space_l_k_jumps_to_type_definition() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_type_definition(
            path.to_str().unwrap(),
            0,
            0,
            path.to_str().unwrap(),
            2,
            0,
        );
        h.type_keys("space l k");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_goto_implementation(h: &TestHarness) {
        use lsp_types::{ImplementationProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn goto_implementation_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_implementation_opens_target_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(
            &mut h,
            &[
                ("trait.rs", "trait X {}\n"),
                ("impl.rs", "impl X for One {}\nimpl X for Two {}\n"),
            ],
        );
        let trait_path = root.join("trait.rs");
        let impl_path = root.join("impl.rs");
        open_buffer(&mut h, trait_path.clone());
        h.fake_lsp().set_implementation(
            trait_path.to_str().unwrap(),
            0,
            0,
            impl_path.to_str().unwrap(),
            1,
            5,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(focused_buffer_path(&h), impl_path);
        assert_eq!(cursor_offset(&mut h), 23);
    }

    #[test]
    fn goto_implementation_no_result_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_implementation_unsupported_capability_is_noop() {
        use lsp_types::{OneOf, ServerCapabilities};
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn space_l_t_jumps_to_implementation() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("space l t");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn g_s_jumps_to_implementation() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("g s");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn goto_definition_without_a_server_reports_no_server() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat
            .set_lsp_host(std::sync::Arc::new(crate::host::NoopLsp));
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no language server running"),
        );
    }

    #[test]
    fn unsupported_feature_with_two_servers_reports_does_not_support() {
        use lsp_types::ServerCapabilities;
        let mut h = TestHarness::with_size(80, 24);
        // Two servers run but neither advertises hover, so the sole-host probe is
        // a noop. The report must still name the missing capability rather than
        // claim the server is still starting.
        let _ = install_two_servers(&mut h, ServerCapabilities::default());
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support hover"),
        );
    }

    fn enable_code_action(h: &TestHarness) {
        use lsp_types::{CodeActionProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[allow(clippy::mutable_key_type)]
    fn direct_action(
        title: &str,
        file: &str,
        line: u32,
        col: u32,
        text: &str,
    ) -> lsp_types::CodeActionOrCommand {
        use lsp_types::{
            CodeAction, CodeActionOrCommand, Position, Range, TextEdit, Uri, WorkspaceEdit,
        };
        use std::{collections::HashMap, str::FromStr};
        let uri = Uri::from_str(&format!("file://{file}")).expect("uri");
        let edit = TextEdit {
            range: Range::new(Position::new(line, col), Position::new(line, col)),
            new_text: text.to_string(),
        };
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri, vec![edit]);
        let workspace_edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: None,
            diagnostics: None,
            edit: Some(workspace_edit),
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        })
    }

    fn unresolved_action(title: &str) -> lsp_types::CodeActionOrCommand {
        use lsp_types::{CodeAction, CodeActionOrCommand};
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: Some(serde_json::Value::Null),
        })
    }

    fn command_only_action(title: &str) -> lsp_types::CodeActionOrCommand {
        use lsp_types::{CodeActionOrCommand, Command};
        CodeActionOrCommand::Command(Command {
            title: title.to_string(),
            command: "noop".to_string(),
            arguments: None,
        })
    }

    fn buffer_text(h: &TestHarness, path: &Path) -> String {
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(path)
            .expect("buffer for path");
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("buffer lock");
        guard.rope().to_string()
    }

    #[test]
    fn code_action_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert!(h.stoat.pending_code_action_request.is_none());
    }

    #[test]
    fn code_action_no_response_clears_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert!(h.stoat.pending_code_action_request.is_none());
    }

    #[test]
    fn code_action_populates_picker_with_titles() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![
                direct_action("Add import", path.to_str().unwrap(), 0, 0, "use a;\n"),
                direct_action("Inline variable", path.to_str().unwrap(), 0, 0, ""),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        let picker = h
            .stoat
            .pending_code_action_picker
            .as_ref()
            .expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title()).collect();
        assert_eq!(titles, vec!["Add import", "Inline variable"]);
    }

    #[test]
    fn code_action_retains_command_only_entries() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![
                command_only_action("Run command"),
                direct_action("Real edit", path.to_str().unwrap(), 0, 0, "X"),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        let picker = h
            .stoat
            .pending_code_action_picker
            .as_ref()
            .expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title()).collect();
        assert_eq!(titles, vec!["Run command", "Real edit"]);
    }

    #[test]
    fn code_action_pick_command_dispatches_execute_command() {
        use lsp_types::{CodeActionOrCommand, Command};
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![CodeActionOrCommand::Command(Command {
                title: "Apply import".to_string(),
                command: "rust-analyzer.applyImport".to_string(),
                arguments: Some(vec![serde_json::json!({"target": "std::io"})]),
            })],
        );
        h.type_keys("space l a");
        h.settle();
        h.type_keys("1");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        let observed = h.fake_lsp().observed_executed_commands();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].command, "rust-analyzer.applyImport");
        assert_eq!(
            observed[0].arguments,
            vec![serde_json::json!({"target": "std::io"})]
        );
    }

    #[test]
    fn code_action_navigates_with_jk_and_picks_with_enter() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let actions: Vec<lsp_types::CodeActionOrCommand> = (0..12)
            .map(|i| {
                direct_action(
                    &format!("Action {i}"),
                    path.to_str().unwrap(),
                    0,
                    0,
                    &format!("// {i}\n"),
                )
            })
            .collect();
        h.fake_lsp()
            .set_code_actions(path.to_str().unwrap(), actions);
        h.type_keys("space l a");
        h.settle();
        for _ in 0..11 {
            h.type_keys("j");
        }
        let picker = h.stoat.pending_code_action_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_idx, 11);

        h.type_keys("enter");
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert_eq!(buffer_text(&h, &path), "// 11\nabc\n");
    }

    /// The guard reads keys ahead of the keymap and closes on anything it does
    /// not name, so an arrow it does not name dismisses the picker.
    #[test]
    fn code_action_navigates_with_the_arrows() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let actions: Vec<lsp_types::CodeActionOrCommand> = (0..3)
            .map(|i| {
                direct_action(
                    &format!("Action {i}"),
                    path.to_str().unwrap(),
                    0,
                    0,
                    &format!("// {i}\n"),
                )
            })
            .collect();
        h.fake_lsp()
            .set_code_actions(path.to_str().unwrap(), actions);
        h.type_keys("space l a");
        h.settle();

        let selected = |h: &TestHarness| {
            h.stoat
                .pending_code_action_picker
                .as_ref()
                .expect("the arrow steps the picker rather than closing it")
                .selected_idx
        };

        h.type_keys("down");
        h.type_keys("down");
        let after_down = selected(&h);
        h.type_keys("up");

        assert_eq!(
            (after_down, selected(&h)),
            (2, 1),
            "down walks toward the last entry and up walks back"
        );
    }

    #[test]
    fn code_action_pick_one_applies_edit() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action(
                "Insert prefix",
                path.to_str().unwrap(),
                0,
                0,
                "// hi\n",
            )],
        );
        h.type_keys("space l a");
        h.settle();
        h.type_keys("1");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert_eq!(buffer_text(&h, &path), "// hi\nabc\n");
    }

    #[test]
    fn code_action_resolve_path_applies_resolved_edit() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_code_actions(path.to_str().unwrap(), vec![unresolved_action("Refactor")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_some());
        crate::action_handlers::lsp::pick_code_action(&mut h.stoat, 0);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert!(!h.stoat.pending_code_action_resolve.is_pending());
    }

    #[test]
    fn code_action_resolve_routes_back_to_the_producing_server() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{
            request::CodeActionResolveRequest, CodeActionProviderCapability, ServerCapabilities,
        };

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let producer = std::sync::Arc::new(crate::host::FakeLsp::new());
        producer.set_capabilities(ServerCapabilities {
            code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("producer".into(), producer.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("producer".into()),
            ],
        );

        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        producer.set_code_actions(path.to_str().unwrap(), vec![unresolved_action("Refactor")]);
        producer.set_pending_mode::<CodeActionResolveRequest>(true);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(
            h.stoat.pending_code_action_picker.is_some(),
            "the producing server served the code action"
        );

        crate::action_handlers::lsp::pick_code_action(&mut h.stoat, 0);
        h.settle();

        assert_eq!(
            producer.pending_count("codeAction/resolve"),
            1,
            "resolve routes back to the producing server"
        );
        assert_eq!(
            primary.pending_count("codeAction/resolve"),
            0,
            "resolve does not go to the primary that never saw the action"
        );
    }

    #[test]
    fn code_action_escape_dismisses_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
        );
        h.type_keys("space l a");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_some());
        h.type_keys("escape");
        assert!(h.stoat.pending_code_action_picker.is_none());
    }

    #[test]
    fn space_l_a_triggers_code_action() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
        );
        h.type_keys("space l a");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_some());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn snapshot_code_action_picker() {
        let mut h = TestHarness::with_size(40, 12);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![
                direct_action("Add import", path.to_str().unwrap(), 0, 0, "X"),
                direct_action("Inline", path.to_str().unwrap(), 0, 0, "X"),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        h.assert_snapshot("snapshot_code_action_picker");
    }

    fn enable_rename(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            rename_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    #[allow(clippy::mutable_key_type)]
    fn rename_workspace_edit(
        file: &str,
        line: u32,
        col: u32,
        len: u32,
        new: &str,
    ) -> lsp_types::WorkspaceEdit {
        use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit, Uri, WorkspaceEdit};
        use std::{collections::HashMap, str::FromStr};
        let uri = Uri::from_str(&format!("file://{file}")).expect("uri");
        let edit = TextEdit {
            range: LspRange::new(
                LspPosition::new(line, col),
                LspPosition::new(line, col + len),
            ),
            new_text: new.to_string(),
        };
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri, vec![edit]);
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    #[test]
    fn rename_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        assert!(h.stoat.rename_input.is_none());
        assert!(h.stoat.pending_prepare_rename.is_none());
    }

    #[test]
    fn rename_no_response_does_not_open_modal() {
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        assert!(h.stoat.rename_input.is_none());
    }

    #[test]
    fn rename_range_response_seeds_placeholder_from_rope() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "foo");
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    /// A range whose endpoints arrive out of order seeds an empty
    /// placeholder rather than taking the editor down.
    ///
    /// A server that has not seen the latest edits answers over a document
    /// state the buffer no longer holds, and a line past the buffer converts
    /// to the rope's end. Either way the start offset lands above the end
    /// offset, and the rope slice then subtracts the chunk start from the
    /// smaller end offset and underflows.
    ///
    /// The fixture spans several rope chunks because the arithmetic only
    /// underflows once the two offsets fall in different ones.
    #[test]
    fn rename_range_response_with_an_inverted_range_seeds_nothing() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", &"fn foo() {}\n".repeat(20))]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(19, 0),
                LspPosition::new(0, 6),
            )),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "");
    }

    #[test]
    fn rename_with_placeholder_form() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::RangeWithPlaceholder {
                range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)),
                placeholder: "Renamed".to_string(),
            },
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "Renamed");
    }

    #[test]
    fn rename_submit_applies_workspace_edit() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.fake_lsp().set_rename(
            path.to_str().unwrap(),
            0,
            0,
            rename_workspace_edit(path.to_str().unwrap(), 0, 3, 3, "bar"),
        );
        h.type_keys("space l r");
        h.settle();
        assert!(h.stoat.rename_input.is_some());
        crate::action_handlers::lsp::rename_input_submit(&mut h.stoat);
        h.settle();
        assert!(h.stoat.rename_input.is_none());
        assert_eq!(buffer_text(&h, &path), "fn bar() {}\n");
    }

    /// Move the focused buffer on without going through a keypress, which would
    /// also pump the reply and close the window the guard exists for.
    fn edit_focused_buffer_directly(h: &mut TestHarness, text: &str) {
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, text);
    }

    #[test]
    fn a_rename_reply_is_dropped_when_the_buffer_moved_under_it() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.fake_lsp().set_rename(
            path.to_str().unwrap(),
            0,
            0,
            rename_workspace_edit(path.to_str().unwrap(), 0, 3, 3, "bar"),
        );
        h.type_keys("space l r");
        h.settle();

        crate::action_handlers::lsp::rename_input_submit(&mut h.stoat);
        edit_focused_buffer_directly(&mut h, "x");
        h.settle();

        assert_eq!(buffer_text(&h, &path), "xfn foo() {}\n");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: rename skipped, buffer changed"),
        );
    }

    #[test]
    fn rename_cancel_discards_modal() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.type_keys("space l r");
        h.settle();
        assert!(h.stoat.rename_input.is_some());
        let cancelled = crate::action_handlers::lsp::rename_input_cancel(&mut h.stoat);
        assert!(cancelled);
        assert!(h.stoat.rename_input.is_none());
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn space_l_r_triggers_rename() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.type_keys("space l r");
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "foo");
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn snapshot_rename_input_modal() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(40, 12);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        h.assert_snapshot("snapshot_rename_input");
    }

    use lsp_types::{DocumentSymbol, DocumentSymbolResponse};

    #[test]
    fn symbol_picker_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        assert!(h.stoat.symbol_finder.is_none());
        assert!(h.stoat.pending_symbol_picker_request.is_none());
    }

    #[test]
    fn symbol_picker_no_response_keeps_modal_open() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("modal stays open");
        assert!(finder.entries.is_empty(), "no symbols yields an empty list");
    }

    #[test]
    fn symbol_picker_populates_with_flat_symbols() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("foo", path.to_str().unwrap(), 0, 3),
                flat_symbol("bar", path.to_str().unwrap(), 1, 3),
            ]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["foo", "bar"]);
    }

    #[test]
    fn symbol_picker_flattens_nested_symbols() {
        use lsp_types::{Position as LspPosition, Range as LspRange, SymbolKind};
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn outer() {\n  fn inner() {}\n}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let range = LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1));
        let inner = {
            #[allow(deprecated)]
            DocumentSymbol {
                name: "inner".to_string(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range,
                selection_range: range,
                children: None,
            }
        };
        let outer = {
            #[allow(deprecated)]
            DocumentSymbol {
                name: "outer".to_string(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range,
                selection_range: range,
                children: Some(vec![inner]),
            }
        };
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Nested(vec![outer]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["outer", "outer.inner"]);
    }

    #[test]
    fn symbol_picker_pick_jumps_to_offset() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("foo", path.to_str().unwrap(), 0, 3),
                flat_symbol("bar", path.to_str().unwrap(), 1, 3),
            ]),
        );
        h.type_keys("space l s");
        h.settle();
        h.type_keys("down");
        h.type_keys("enter");
        assert!(h.stoat.symbol_finder.is_none());
        assert_eq!(cursor_offset(&mut h), 15);
    }

    #[test]
    fn symbol_picker_keeps_all_entries() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "x\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let many: Vec<lsp_types::SymbolInformation> = (0..15)
            .map(|i| flat_symbol(&format!("sym{i}"), path.to_str().unwrap(), 0, 0))
            .collect();
        h.fake_lsp()
            .set_document_symbols(path.to_str().unwrap(), DocumentSymbolResponse::Flat(many));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        assert_eq!(finder.entries.len(), 15);
        assert_eq!(finder.selected, 0);
    }

    #[test]
    fn symbol_picker_navigates_with_arrows_and_picks_with_enter() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let mut text = String::new();
        for _ in 0..15 {
            text.push_str("fn x() {}\n");
        }
        let root = seed(&mut h, &[("main.rs", text.as_str())]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let many: Vec<lsp_types::SymbolInformation> = (0..15)
            .map(|i| flat_symbol(&format!("sym{i}"), path.to_str().unwrap(), i as u32, 3))
            .collect();
        h.fake_lsp()
            .set_document_symbols(path.to_str().unwrap(), DocumentSymbolResponse::Flat(many));

        h.type_keys("space l s");
        h.settle();
        for _ in 0..11 {
            h.type_keys("down");
        }
        let finder = h.stoat.symbol_finder.as_ref().expect("finder");
        assert_eq!(finder.selected, 11);

        h.type_keys("enter");
        assert!(h.stoat.symbol_finder.is_none());
        assert_eq!(cursor_offset(&mut h), 11 * 10 + 3);
    }

    #[test]
    fn symbol_picker_escape_dismisses() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        h.type_keys("space l s");
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        h.type_keys("escape");
        assert!(h.stoat.symbol_finder.is_none());
    }

    #[test]
    fn space_l_s_triggers_symbol_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        h.type_keys("space l s");
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn snapshot_symbol_picker() {
        let mut h = TestHarness::with_size(60, 16);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("foo", path.to_str().unwrap(), 0, 3),
                flat_symbol("bar", path.to_str().unwrap(), 1, 3),
            ]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        h.assert_snapshot("snapshot_symbol_picker");
    }

    #[test]
    fn symbol_finder_renders_hover_doc_above_source() {
        let mut h = TestHarness::with_size(120, 30);
        enable_document_symbols_and_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn target() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("target", path.to_str().unwrap(), 0, 3)]),
        );
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 3, "TARGETDOC");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        assert_eq!(
            h.stoat
                .symbol_finder
                .as_ref()
                .unwrap()
                .doc_markdown
                .as_deref(),
            Some("TARGETDOC"),
        );
        let buf = h.stoat.render();
        let shown = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("TARGETDOC")
        });
        assert!(shown, "the hover doc renders in the preview pane");
    }

    #[test]
    fn symbol_finder_hover_none_leaves_doc_empty() {
        let mut h = TestHarness::with_size(120, 30);
        enable_document_symbols_and_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn target() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("target", path.to_str().unwrap(), 0, 3)]),
        );
        // No set_hover, so the server answers None. The doc stays empty and no
        // error surfaces.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        assert!(
            h.stoat
                .symbol_finder
                .as_ref()
                .unwrap()
                .doc_markdown
                .is_none(),
            "an empty hover response leaves the doc area empty",
        );
        assert!(
            h.stoat.pending_message.is_none(),
            "a missing doc surfaces no error",
        );
    }

    #[test]
    fn symbol_finder_hover_doc_follows_selection() {
        let mut h = TestHarness::with_size(120, 30);
        enable_document_symbols_and_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn aaa() {}\nfn bbb() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("aaa", path.to_str().unwrap(), 0, 3),
                flat_symbol("bbb", path.to_str().unwrap(), 1, 3),
            ]),
        );
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 3, "AAA DOC");
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 1, 3, "BBB DOC");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        assert_eq!(
            h.stoat
                .symbol_finder
                .as_ref()
                .unwrap()
                .doc_markdown
                .as_deref(),
            Some("AAA DOC"),
        );

        h.type_keys("down");
        h.settle();
        assert_eq!(
            h.stoat
                .symbol_finder
                .as_ref()
                .unwrap()
                .doc_markdown
                .as_deref(),
            Some("BBB DOC"),
            "the doc follows the selection, discarding the previous entry's doc",
        );
    }

    /// Install two identically-capable fakes routed primary-then-secondary for
    /// `rust`. The caller seeds and opens its own buffer.
    fn document_symbol_caps() -> lsp_types::ServerCapabilities {
        use lsp_types::{OneOf, ServerCapabilities};
        ServerCapabilities {
            document_symbol_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        }
    }

    #[test]
    fn document_symbols_merge_from_two_servers() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary) = install_two_servers(&mut h, document_symbol_caps());
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        primary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", p, 0, 3)]),
        );
        secondary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("bar", p, 1, 3)]),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["foo", "bar"],
            "both servers' symbols, primary first"
        );
    }

    #[test]
    fn document_symbols_convert_each_with_its_servers_encoding() {
        use crate::{host::OffsetEncoding, symbol_finder::SymbolTarget};
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary) = install_two_servers(&mut h, document_symbol_caps());
        primary.set_offset_encoding(OffsetEncoding::Utf8);
        secondary.set_offset_encoding(OffsetEncoding::Utf16);
        let root = seed(&mut h, &[("main.rs", "\u{e9}x\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();

        // `x` sits at byte offset 2. `é` is two UTF-8 bytes but one UTF-16 unit,
        // so each server names `x`'s column in its own encoding.
        primary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("utf8", p, 0, 2)]),
        );
        secondary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("utf16", p, 0, 1)]),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let resolved: Vec<(&str, usize)> = finder
            .entries
            .iter()
            .map(|e| match &e.target {
                SymbolTarget::Offset(offset) => (e.title.as_str(), *offset),
                other => unreachable!("document symbols carry offset targets, got {other:?}"),
            })
            .collect();
        assert_eq!(
            resolved,
            vec![("utf8", 2), ("utf16", 2)],
            "each server's column resolves with its own encoding"
        );
    }

    #[test]
    fn workspace_symbols_merge_from_two_servers() {
        use lsp_types::{OneOf, ServerCapabilities, SymbolKind};
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary) = install_two_servers(
            &mut h,
            ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
        );
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        primary.add_workspace_symbol(
            "f",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );
        secondary.add_workspace_symbol(
            "f",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["foo", "bar"],
            "both servers' symbols, primary first"
        );
    }

    #[test]
    fn workspace_symbol_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        assert!(h.stoat.symbol_finder.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn workspace_symbol_opens_finder_modal() {
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn workspace_symbol_query_populates_finder() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        h.fake_lsp().add_workspace_symbol(
            "f",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );
        h.fake_lsp().add_workspace_symbol(
            "f",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["foo", "bar"]);
    }

    #[test]
    fn workspace_symbol_query_handles_nested_response() {
        use crate::symbol_finder::SymbolTarget;
        use lsp_types::{
            Location, OneOf, Position as LspPosition, Range as LspRange, SymbolKind, Uri,
            WorkspaceLocation, WorkspaceSymbol, WorkspaceSymbolResponse,
        };
        use std::str::FromStr;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        let main_uri = Uri::from_str(&format!("file://{}", main.to_str().unwrap())).unwrap();
        let lib_uri = Uri::from_str(&format!("file://{}", lib.to_str().unwrap())).unwrap();
        let nested = WorkspaceSymbolResponse::Nested(vec![
            WorkspaceSymbol {
                name: "foo".to_string(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                container_name: None,
                location: OneOf::Left(Location::new(
                    main_uri,
                    LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)),
                )),
                data: None,
            },
            WorkspaceSymbol {
                name: "bar".to_string(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                container_name: None,
                location: OneOf::Right(WorkspaceLocation { uri: lib_uri }),
                data: None,
            },
        ]);
        h.fake_lsp().set_workspace_symbol_response("f", nested);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let entries: Vec<(&str, &Path, LspPosition)> = finder
            .entries
            .iter()
            .map(|e| match &e.target {
                SymbolTarget::Workspace { path, position, .. } => {
                    (e.title.as_str(), path.as_path(), *position)
                },
                other => unreachable!("workspace symbols carry workspace targets, got {other:?}"),
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                ("foo", main.as_path(), LspPosition::new(0, 3)),
                ("bar", lib.as_path(), LspPosition::new(0, 0)),
            ]
        );
    }

    #[test]
    fn workspace_symbol_coalesces_mid_flight_query_edits() {
        use std::time::Duration;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        // Hold the initial empty-query request in flight so later edits coalesce
        // onto it rather than firing their own requests.
        h.fake_lsp()
            .set_request_delay("workspace/symbol", Duration::from_secs(60));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("a");
        h.settle();
        h.type_keys("b");
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        assert!(
            finder.query_dirty,
            "edits while a request is in flight mark the query dirty",
        );
        assert_eq!(finder.last_query, "ab", "the latest text is remembered");
        assert!(
            h.stoat.pending_workspace_symbol_request.is_some(),
            "only the one in-flight request exists; no second fired mid-flight",
        );
    }

    #[test]
    fn workspace_symbol_pick_opens_target_file() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        h.fake_lsp().add_workspace_symbol(
            "bar",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("b a r");
        h.settle();
        h.type_keys("enter");
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(editor_id) = pane.view else {
            panic!("not an editor");
        };
        let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
        let path = ws
            .buffers
            .path_for(buffer_id)
            .expect("buffer path")
            .to_path_buf();
        assert_eq!(path, lib);
        assert_eq!(cursor_offset(&mut h), 3);
    }

    #[test]
    fn workspace_symbol_navigates_with_arrows_and_picks_with_enter() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let mut files: Vec<(&str, &str)> = (0..12)
            .map(|i| {
                let path = Box::leak(format!("f{i}.rs").into_boxed_str()) as &str;
                (path, "fn target() {}\n")
            })
            .collect();
        files.push(("anchor.rs", "fn anchor() {}\n"));
        let root = seed(&mut h, &files);
        let anchor_path = root.join("anchor.rs");
        open_buffer(&mut h, anchor_path);
        for i in 0..12 {
            let p = root.join(format!("f{i}.rs"));
            h.fake_lsp().add_workspace_symbol(
                "t",
                "target",
                SymbolKind::FUNCTION,
                p.to_str().unwrap(),
                0,
                3,
            );
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("t");
        h.settle();

        for _ in 0..11 {
            h.type_keys("down");
        }
        let finder = h.stoat.symbol_finder.as_ref().expect("finder");
        assert_eq!(finder.selected, 11);

        h.type_keys("enter");
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(eid) = pane.view else {
            panic!("not an editor");
        };
        let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
        let path = ws.buffers.path_for(buffer_id).expect("path").to_path_buf();
        assert_eq!(path, root.join("f11.rs"));
        assert_eq!(cursor_offset(&mut h), 3);
    }

    #[test]
    fn workspace_symbol_cancel_clears_modal() {
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        h.type_keys("escape");
        assert!(h.stoat.symbol_finder.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn space_l_shift_s_triggers_workspace_symbol() {
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        h.type_keys("space l shift-s");
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn snapshot_workspace_symbol_finder() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(60, 16);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let main = root.join("main.rs");
        open_buffer(&mut h, main.clone());
        h.fake_lsp().add_workspace_symbol(
            "f",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        h.settle();
        h.assert_snapshot("snapshot_workspace_symbol_finder");
    }

    fn enable_format(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_formatting_provider: Some(OneOf::Left(true)),
            document_range_formatting_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn format_text_edit(
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        new: &str,
    ) -> lsp_types::TextEdit {
        use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit};
        TextEdit {
            range: LspRange::new(
                LspPosition::new(line, col),
                LspPosition::new(end_line, end_col),
            ),
            new_text: new.to_string(),
        }
    }

    #[test]
    fn format_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_range_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
    }

    #[test]
    fn format_no_response_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
    }

    #[test]
    fn format_applies_returned_edits() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_range_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    #[test]
    fn format_equals_keystroke_triggers() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_range_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        h.type_keys("=");
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    #[test]
    fn format_document_applies_returned_edits() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    #[test]
    fn a_format_reply_is_dropped_when_the_buffer_moved_under_it() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);

        // The buffer moves while the request is in flight, so every position
        // the server sent back names text one character along from what it
        // measured. Edited directly because a keypress would also pump the
        // reply, closing the window before the edit lands.
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");
        h.settle();

        assert_eq!(
            buffer_text(&h, &path),
            "xfn  foo (){}\n",
            "the reply was applied to text the server never saw"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: format skipped, buffer changed"),
        );
    }

    #[test]
    fn formatting_asks_for_the_indentation_the_buffer_uses() {
        // Two leading levels each, so the style detector has something to vote
        // on. One file indents with tabs and the other with two spaces.
        for (name, text, tab_size, insert_spaces) in [
            ("tabs.rs", "fn a() {\n\tlet b = 1;\n\t\tc();\n}\n", 4, false),
            (
                "spaces.rs",
                "fn a() {\n  let b = 1;\n    c();\n}\n",
                2,
                true,
            ),
        ] {
            let mut h = TestHarness::with_size(80, 24);
            enable_format(&h);
            let root = seed(&mut h, &[(name, text)]);
            let path = root.join(name);
            open_buffer(&mut h, path.clone());

            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
            h.settle();

            let observed = h.fake_lsp().observed_formatting();
            assert_eq!(observed.len(), 1, "{name}");
            assert_eq!(observed[0].options.tab_size, tab_size, "{name} tab size");
            assert_eq!(
                observed[0].options.insert_spaces, insert_spaces,
                "{name} insert spaces",
            );
        }
    }

    #[test]
    fn range_formatting_asks_for_the_indentation_too() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("a.rs", "fn a() {\n  let b = 1;\n    c();\n}\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();

        let observed = h.fake_lsp().observed_range_formatting();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].options.tab_size, 2);
        assert!(observed[0].options.insert_spaces);
    }

    #[test]
    fn format_document_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support format"),
        );
    }

    #[test]
    fn space_l_f_formats_document() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        h.type_keys("space l f");
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    fn enable_inlay_hints(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            inlay_hint_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn type_hint(line: u32, col: u32, label: &str) -> lsp_types::InlayHint {
        use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position};
        InlayHint {
            position: Position::new(line, col),
            label: InlayHintLabel::String(label.to_string()),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }
    }

    fn hint_ids_len(h: &mut TestHarness) -> usize {
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .hint_inlay_ids
            .len()
    }

    fn focused_editor_id(h: &TestHarness) -> crate::editor_state::EditorId {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        }
    }

    fn editor_hint_ids_len(h: &TestHarness, id: crate::editor_state::EditorId) -> usize {
        h.stoat
            .active_workspace()
            .editors
            .get(id)
            .expect("editor")
            .hint_inlay_ids
            .len()
    }

    #[test]
    fn snapshot_inlay_hints_render_when_enabled() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        h.assert_snapshot("inlay_hints_enabled");
    }

    #[test]
    fn inlay_hints_toggle_off_clears_inlays() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        assert_eq!(hint_ids_len(&mut h), 1);

        h.type_keys("space l h");
        assert_eq!(hint_ids_len(&mut h), 0);
    }

    #[test]
    fn inlay_hints_toggle_off_clears_unfocused_editors() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("a.rs", "let x = 1\n"), ("b.rs", "let y = 2\n")]);
        let a = root.join("a.rs");
        open_buffer(&mut h, a.clone());
        h.fake_lsp()
            .set_range_inlay_hints(a.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        let a_editor = focused_editor_id(&h);
        assert_eq!(editor_hint_ids_len(&h, a_editor), 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        open_buffer(&mut h, root.join("b.rs"));
        assert_ne!(
            focused_editor_id(&h),
            a_editor,
            "opening b.rs in the split moves focus off a.rs's editor"
        );

        h.type_keys("space l h");
        assert_eq!(
            editor_hint_ids_len(&h, a_editor),
            0,
            "toggle-off clears hints from the unfocused a.rs editor"
        );
    }

    #[test]
    fn inlay_hints_toggle_on_requests_without_the_debounce() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.settle();
        assert_eq!(
            hint_ids_len(&mut h),
            1,
            "toggle-on applies hints without advancing the debounce clock"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("inlay hints on"),
            "toggle-on acknowledges in the status bar"
        );
    }

    #[test]
    fn inlay_hints_toggle_off_acknowledges() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.settle();
        h.type_keys("space l h");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("inlay hints off"),
            "toggle-off acknowledges in the status bar"
        );
    }

    #[test]
    fn inlay_hints_toggle_on_without_a_capable_server_reports_why() {
        let mut h = TestHarness::with_size(40, 8);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.type_keys("space l h");
        h.settle();
        assert_eq!(
            hint_ids_len(&mut h),
            0,
            "no capable server means no hints are applied"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support inlay hints"),
            "toggle-on with no inlay capability reports why"
        );
    }

    #[test]
    fn inlay_hints_refresh_after_edit() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_range_inlay_hints(p, vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        assert_eq!(hint_ids_len(&mut h), 1);

        h.fake_lsp()
            .set_range_inlay_hints(p, vec![type_hint(0, 5, ": u32"), type_hint(0, 8, ": b")]);
        h.type_keys("i");
        h.type_text("z");
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(150));
        assert_eq!(hint_ids_len(&mut h), 2);
    }

    fn tree_sitter_token_count(h: &mut TestHarness) -> usize {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .semantic_token_highlights()
            .values()
            .map(|channel| channel.len())
            .sum()
    }

    #[test]
    fn switching_back_keeps_tree_sitter_highlights_on_first_frame() {
        let mut h = TestHarness::with_size(24, 4);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);

        // A render cycle drives the parse so A's tokens land in the registry and
        // on its editor.
        open_buffer(&mut h, root.join("a.rs"));
        h.snapshot();
        assert!(tree_sitter_token_count(&mut h) > 0, "file A parses on open");

        open_buffer(&mut h, root.join("b.rs"));
        h.snapshot();

        // Switch back to A with no render or parse cycle in between. The parse
        // pipeline skips a version-current buffer, so the fresh editor is styled
        // only if it was seeded from the registry's retained tokens.
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        assert!(
            tree_sitter_token_count(&mut h) > 0,
            "re-shown buffer is styled on the first frame after switch-back"
        );
    }

    #[test]
    fn a_same_file_jump_glides_while_a_cross_file_jump_snaps() {
        use crate::editor_state::ScrollGlide;

        let mut h = TestHarness::with_size(40, 12);
        let long: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        let root = seed(&mut h, &[("a.rs", long.as_str()), ("b.rs", long.as_str())]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        h.settle();
        {
            let editor =
                crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            editor.scroll_glide = ScrollGlide::None;
        }

        super::apply_jump(&mut h.stoat, &root.join("a.rs"), long.len());
        {
            let editor =
                crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            assert!(
                editor.scroll_row > 20,
                "the view followed the cursor down the file"
            );
            assert_eq!(
                editor.scroll_glide,
                ScrollGlide::Page,
                "a same-file jump glides from where the view was"
            );
            editor.scroll_glide = ScrollGlide::None;
        }

        super::apply_jump(&mut h.stoat, &root.join("b.rs"), long.len());
        h.settle();
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert!(
            editor.scroll_row > 20,
            "the cursor is still pulled into view across files"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::None,
            "a fresh editor has no origin to glide from, so it snaps"
        );
    }
}
