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
    buffer_registry::OpenOrigin,
    display_map::InlayKind,
    editor_state::ScrollGlide,
    host::{FsHost, LanguageServerFeature, LspHost, OffsetEncoding},
    input_view::{InputView, SubmitTarget},
    location_picker::{location_haystack, LocationEntry, LocationPicker},
    lsp::stamp::DocumentStamp,
    render::editor,
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
    ops,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_scheduler::Task;
use stoat_text::{Anchor, Bias, LineEnding, Point, Rope, SelectionGoal};

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
    let mut spans: Vec<(usize, usize)> = offsets
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| (p[0], p[1]))
        .collect();
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
    let open = open_ropes(stoat);
    let executor = stoat.executor.clone();
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

        let mut answers = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(locations)) => {
                    answers.push((encoding, GotoDefinitionResponse::Array(locations)))
                },
                Ok(None) => {},
                Err(err) => tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    "references request failed",
                ),
            }
        }
        executor
            .spawn_blocking(move || {
                resolve_goto_targets(answers, &source_path, &source_rope, &open, &*fs)
            })
            .await
    });
    stoat.pending_lsp_jump = Some(("references", task));
    UpdateEffect::None
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
/// Returns `None` when the focused pane is not an editor or its buffer has
/// no path.
fn lsp_request_site(stoat: &mut Stoat) -> Option<LspRequestSite> {
    let (focused_offset, buffer_id, focused_rope) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (offset, editor.buffer_id, buf_snap.rope().clone())
    };

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

/// The rope of every open path-bound buffer, keyed by its path.
///
/// A goto target in an open file resolves against this text, because it is
/// what the server was sent. The file on disk does not hold the unsaved edits,
/// and it keeps its own line endings.
fn open_ropes(stoat: &Stoat) -> HashMap<PathBuf, Rope> {
    let buffers = &stoat.active_workspace().buffers;
    buffers
        .open_paths()
        .into_iter()
        .filter_map(|path| {
            let rope = buffers
                .get(buffers.id_for_path(&path)?)?
                .read()
                .ok()?
                .rope()
                .clone();
            Some((path, rope))
        })
        .collect()
}

/// Issue an LSP jump-style request (definition / type definition /
/// implementation / declaration) for the symbol under the focused
/// editor's primary cursor. The async response is stored on
/// [`Stoat::pending_lsp_jump`] and applied by [`pump_lsp_jumps`] on
/// the next render tick.
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
    let open = open_ropes(stoat);
    let executor = stoat.executor.clone();
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

        let mut answers = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => answers.push((encoding, response)),
                Ok(None) => {},
                Err(err) => tracing::warn!(
                    target: "stoat::lsp",
                    request = kind.warn_label(),
                    ?err,
                    "lsp jump request failed",
                ),
            }
        }
        executor
            .spawn_blocking(move || {
                resolve_goto_targets(answers, &source_path, &source_rope, &open, &*fs)
            })
            .await
    });
    stoat.pending_lsp_jump = Some((kind.status_label(), task));
    UpdateEffect::None
}

/// Resolve every server's goto answer into [`LocationEntry`] values.
///
/// The entries keep the order of `answers` and of the candidates inside each
/// one. One entry lets the caller jump directly, and several open a picker.
/// The resolve drops a candidate whose URI is not a `file:` path, or whose
/// file fails to read. The other candidates still resolve.
///
/// A goto request goes to every capable server, and two servers that index
/// one crate often answer with the same target. Only the first candidate for
/// each `(path, offset)` stays. The callers pass the answers in server
/// priority order, so the higher-priority server's copy is the one kept. A
/// redundant answer then does not open a picker over one target.
///
/// Each entry carries the byte offset under its server's [`OffsetEncoding`],
/// the 1-based line and column, the trimmed text of the target line, and the
/// bytes of the block a link names.
///
/// A server measures a position in an open file against the text it was
/// sent. Targets in the source file reuse `source_rope`, and targets in
/// another open file use its rope from `open`. The resolve reads each other
/// file through `fs` and builds its rope once for all the candidates in it, so
/// a file with no open buffer resolves too. That rope takes LF line endings, as
/// a buffer does, so the offset is the one the opened buffer holds. The reads
/// and the rope builds block, so both callers run this on the pool.
fn resolve_goto_targets(
    answers: Vec<(OffsetEncoding, GotoDefinitionResponse)>,
    source_path: &Path,
    source_rope: &Rope,
    open: &HashMap<PathBuf, Rope>,
    fs: &dyn FsHost,
) -> Vec<LocationEntry> {
    let candidates = answers.into_iter().flat_map(|(encoding, response)| {
        goto_candidates(response)
            .into_iter()
            .map(move |(uri, position, block)| (uri, position, block, encoding))
    });
    type Target = (usize, Position, Option<Range>, OffsetEncoding);
    let mut by_path: HashMap<PathBuf, Vec<Target>> = HashMap::new();
    for (index, (uri, position, block, encoding)) in candidates.enumerate() {
        if let Some(path) = crate::lsp::util::lsp_uri_to_path(&uri) {
            by_path
                .entry(path)
                .or_default()
                .push((index, position, block, encoding));
        }
    }

    let mut entries = Vec::new();
    for (path, targets) in by_path {
        let file_rope;
        let rope = if path == source_path {
            source_rope
        } else if let Some(rope) = open.get(&path) {
            rope
        } else {
            match super::read_string_via_host(fs, &path) {
                Ok(text) => file_rope = Rope::from(LineEnding::normalize(&text).as_ref()),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat::lsp",
                        path = %path.display(),
                        ?err,
                        "goto target file unreadable",
                    );
                    continue;
                },
            }
            &file_rope
        };

        let mut seen = HashSet::new();
        for (index, position, block, encoding) in targets {
            let offset = crate::lsp::util::lsp_pos_to_byte_offset(rope, position, encoding);
            if !seen.insert(offset) {
                continue;
            }
            let entry = LocationEntry {
                path: path.clone(),
                offset,
                line: position.line + 1,
                column: position.character + 1,
                text: line_text(rope, position.line),
                block: block.map(|range| {
                    crate::lsp::util::lsp_pos_to_byte_offset(rope, range.start, encoding)
                        ..crate::lsp::util::lsp_pos_to_byte_offset(rope, range.end, encoding)
                }),
            };
            entries.push((index, entry));
        }
    }

    entries.sort_unstable_by_key(|(index, _)| *index);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// The target URI, the landing position, and the named block of each
/// candidate in `response`.
///
/// A link lands on its selection range, the symbol's name, because its target
/// range starts at the docs and attributes above the name. That target range
/// is the block, the whole declaration. A bare location names no block.
fn goto_candidates(response: GotoDefinitionResponse) -> Vec<(Uri, Position, Option<Range>)> {
    match response {
        GotoDefinitionResponse::Scalar(loc) => vec![(loc.uri, loc.range.start, None)],
        GotoDefinitionResponse::Array(locs) => locs
            .into_iter()
            .map(|loc| (loc.uri, loc.range.start, None))
            .collect(),
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| {
                (
                    link.target_uri,
                    link.target_selection_range.start,
                    Some(link.target_range),
                )
            })
            .collect(),
    }
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
/// dedupe key, or `None` when no editor is focused or it has no viewport.
///
/// Mirrors [`build_inlay_hint_request`]'s editor read without cloning the rope,
/// so the trigger can bail before host resolution.
fn inlay_hint_key(stoat: &mut Stoat) -> Option<(BufferId, u64, u32, u32)> {
    let editor = crate::action_handlers::focused_editor_mut(stoat)?;
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
        let viewport = editor.viewport_rows?;
        let scroll_row = editor.scroll_row;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope().clone();
        let end_row = (scroll_row + viewport).min(snapshot.line_count());
        let visible = editor::visible_byte_range(&snapshot, &rope, scroll_row, end_row);
        (
            editor.buffer_id,
            buf_snap.version(),
            scroll_row,
            end_row,
            rope,
            visible.start,
            visible.end,
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
/// [`InputView`] so render can paint the
/// embedded editor and submit can read the typed name; carries
/// the symbol's URI and request position so submit can build the
/// `RenameParams` without touching the editor again.
#[derive(Debug)]
pub(crate) struct RenameInputState {
    pub(crate) input: InputView,
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
/// modal's [`InputView`].
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
            let input = InputView::create(
                ws,
                executor,
                SubmitTarget::RenameSymbol,
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
/// modal's [`InputView`]. The modal seed is empty;
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
    crate::buffer_lifecycle::open_file_in_pane(stoat, focused, path, OpenOrigin::Visited);

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

/// Build the multi-location picker over `entries`, with its prompt focused.
///
/// The input opens in insert mode so the reader narrows the candidates by
/// typing, the way every other target list works.
pub(crate) fn open_location_picker(
    stoat: &mut Stoat,
    entries: Vec<LocationEntry>,
) -> LocationPicker {
    let haystacks = entries.iter().map(location_haystack).collect();
    let executor = stoat.executor.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.set_focused_mode("insert".into());
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(
        ws,
        executor.clone(),
        SubmitTarget::LocationPicker,
        "",
        "insert",
        1,
    );
    let preview = crate::picker::Preview::new(ws, executor.clone());
    LocationPicker::new(entries, haystacks, input, preview, executor, redraw)
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
                    apply_jump(stoat, &entry.path, entry.offset, entry.block);
                },
                _ => {
                    stoat.location_picker = Some(open_location_picker(stoat, entries));
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
/// A `block` of bytes, the declaration a link names, frames in the pane per
/// [`super::view::frame_jump_on_block`]. A jump with no block follows the
/// cursor with the least scroll.
pub(crate) fn apply_jump(
    stoat: &mut Stoat,
    path: &Path,
    offset: usize,
    block: Option<ops::Range<usize>>,
) {
    super::jump::push_jump(stoat);

    let buffer_before =
        crate::action_handlers::focused_editor_mut(stoat).map(|editor| editor.buffer_id);

    let focused = stoat.active_workspace().panes.focus();
    crate::buffer_lifecycle::open_file_in_pane(stoat, focused, path, OpenOrigin::Visited);
    super::movement::jump_to_offset(stoat, offset);

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };

    // Landing in another file means a freshly shown editor with no prior view to
    // glide from, so it snaps.
    let same_buffer = Some(editor.buffer_id) == buffer_before;
    match (block, same_buffer) {
        (Some(block), _) => {
            super::view::frame_jump_on_block(editor, block, scrolloff, same_buffer);
        },
        (None, true) => {
            super::view::follow_jump(editor, scrolloff);
        },
        (None, false) => {
            super::view::ensure_cursor_in_view(editor, scrolloff);
        },
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
mod tests;
