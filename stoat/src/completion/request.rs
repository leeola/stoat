//! Per-keystroke completion trigger pipeline.
//!
//! [`trigger`] runs after every event in [`crate::app::Stoat::update`].
//! In insert mode with a focused editor pane it computes the cursor
//! context, decides which sources apply, and spawns a debounced
//! task on the stored [`stoat_scheduler::Executor`]. Replacing the
//! prior in-flight task drops it -- async-task cancels the dropped
//! future before its debounce timer or downstream LSP request can
//! land, which is what keeps stale responses from painting over
//! fresh ones.
//!
//! [`pump`] polls the in-flight task each render tick and writes
//! resolved [`CompletionPopup`] results onto
//! [`crate::app::Stoat::pending_completion`]. The hover pipeline
//! at `stoat/src/action_handlers/lsp.rs::pump_lsp_hover` is the
//! local reference shape; the broader design follows
//! `references/helix/helix-term/src/handlers/completion/request.rs`.
//!
//! Suppression: pressing `Esc` while a popup is open clears the
//! popup, cancels the in-flight task, and stamps
//! [`crate::app::Stoat::last_completion_signature`] to the current
//! buffer version so the very-next [`trigger`] returns early instead
//! of immediately re-arming the request from the unchanged buffer.
//! Any actual edit bumps the buffer version, the signature mismatch
//! re-fires the trigger, and the popup comes back on the next
//! response.

use crate::{
    app::Stoat,
    buffer::{BufferId, TextBufferSnapshot},
    completion::{
        applicable_sources, CompletionContext, CompletionItem, CompletionPopup, CompletionSource,
    },
    fuzzy,
    host::{FsHost, LanguageServerFeature, LspHost, OffsetEncoding},
    keymap_state,
    lsp::util,
    pane::{FocusTarget, View},
};
use lsp_types::{
    CompletionContext as LspCompletionContext, CompletionParams, CompletionTriggerKind,
    PartialResultParams, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams,
};
use std::{
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_scheduler::Task;
use stoat_text::{Point, Rope};

/// Quiet window from the most recent keystroke before a completion
/// request is dispatched. Each new keystroke replaces the in-flight
/// task and restarts the timer.
pub(crate) const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(150);

/// Owned snapshot of [`CompletionContext`] for a spawned task. The
/// public context borrows from the rope and prefix; this struct
/// holds owned strings so it can outlive the trigger frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextOwned {
    pub cursor_offset: usize,
    pub prefix: String,
    pub prefix_range: Range<usize>,
    pub text_before_cursor: String,
}

impl ContextOwned {
    fn as_borrowed(&self) -> CompletionContext<'_> {
        CompletionContext {
            cursor_offset: self.cursor_offset,
            prefix: &self.prefix,
            prefix_range: self.prefix_range.clone(),
            text_before_cursor: &self.text_before_cursor,
        }
    }
}

/// Compute the cursor's completion context from a rope plus byte
/// offset. Walks back from the cursor through identifier-or-path
/// characters (alphanumeric, `_`, `/`, `.`, `-`, `~`) to determine
/// the prefix and its byte range.
///
/// [`CompletionContext::text_before_cursor`] is the prefix plus the one
/// character preceding it, not the whole line. That character is what a
/// caller reads to spot a trigger character, since those (`(`, `,`, `:`)
/// are never prefix characters and so sit just outside it. Nothing needs
/// more. The path-shape tests all look for prefix characters, and the path
/// suffix walk covers the same character class the prefix does. The window
/// stops at the line start, so it never reports text from the line above.
///
/// Bounded rather than sliced from the line start because this runs on
/// every insert-mode keystroke, and a long line would otherwise cost a
/// copy of everything left of the cursor to answer a question about the
/// few characters beside it.
pub(crate) fn compute_context(rope: &Rope, cursor_offset: usize) -> ContextOwned {
    let cursor_offset = cursor_offset.min(rope.len());
    let row = rope.offset_to_point(cursor_offset).row;
    let line_start = rope.point_to_offset(Point::new(row, 0));

    let mut prefix_start = cursor_offset;
    let mut window_start = cursor_offset;
    for ch in rope.reversed_chars_at(cursor_offset) {
        if window_start <= line_start {
            break;
        }
        window_start -= ch.len_utf8();
        if !is_word_or_path_char(ch) {
            // Kept by the window so a trigger character is readable, but not
            // by the prefix, which is what it terminates.
            break;
        }
        prefix_start = window_start;
    }

    ContextOwned {
        cursor_offset,
        prefix: rope.slice(prefix_start..cursor_offset).to_string(),
        prefix_range: prefix_start..cursor_offset,
        text_before_cursor: rope.slice(window_start..cursor_offset).to_string(),
    }
}

fn is_word_or_path_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '/' | '.' | '-' | '~')
}

/// Stamp [`Stoat::last_completion_signature`] with the focused editor's
/// current buffer signature, so the very-next [`trigger`] sees an
/// unchanged signature and returns early instead of re-arming a request.
///
/// Called after a completion is accepted, so the text the accept just
/// inserted does not immediately reopen the popup on the next event.
pub(crate) fn record_dismiss(stoat: &mut Stoat) {
    let Some((buffer_id, version)) = focused_buffer_signature(stoat) else {
        return;
    };
    stoat.last_completion_signature = Some((buffer_id, version));
}

/// Per-event entry point. In insert mode with a focused
/// [`View::Editor`] pane and no modal open, computes the cursor
/// context and arms a debounced completion request. Outside that gate,
/// clears any in-flight request, the popup, and the suppression
/// signature so re-entering insert mode starts from a clean slate.
///
/// A modal input (finder, palette, isearch, ...) owns the keystream
/// while open, so completion never triggers beneath one: opening a
/// modal flows through the clearing branch and dismisses any live
/// popup.
pub(crate) fn trigger(stoat: &mut Stoat) {
    if !insert_mode_in_editor_pane(stoat) {
        stoat.pending_completion_request = None;
        stoat.pending_completion = None;
        stoat.last_completion_signature = None;
        stoat.active_snippet = None;
        return;
    }

    // Probed before the full snapshot, which clones the rope and allocates two
    // paths that an unchanged buffer would throw away. The popup check still
    // runs first, since a motion moves the cursor out of range without touching
    // the version the dedupe reads.
    let Some((buffer_id, buffer_version, cursor_offset)) = focused_buffer_probe(stoat) else {
        return;
    };

    let signature = (buffer_id, buffer_version);
    let edited = stoat.last_completion_signature != Some(signature);

    // A motion moves the cursor without touching the version, so the popup has
    // to notice the cursor leaving the range it was filtered for. An edit is
    // not that case. It moves the cursor by construction, and typing on is the
    // very thing the refine below answers, so the check waits until that has
    // declined.
    if !edited {
        if let Some(popup) = &stoat.pending_completion
            && cursor_left_popup(popup, cursor_offset)
        {
            stoat.pending_completion = None;
        }
        return;
    }
    stoat.last_completion_signature = Some(signature);

    let snapshot = match focused_editor_snapshot(stoat) {
        Some(s) => s,
        None => return,
    };

    let owned = compute_context(&snapshot.buffer.visible_text, snapshot.cursor_offset);
    // Signature help asks the same question on this same event, so it reads
    // this rather than walking the rope again.
    stoat.completion_context = Some((
        (
            snapshot.buffer_id,
            snapshot.buffer_version,
            snapshot.cursor_offset,
        ),
        owned.clone(),
    ));

    let completion_hosts = crate::lsp::hosts::feature_hosts(
        stoat,
        snapshot.buffer_id,
        LanguageServerFeature::Completion,
    );

    // Any server that declared the character wants to answer it, so the
    // immediate path is open even when the language's primary server has no
    // interest in it.
    let trigger_char = owned.text_before_cursor.chars().last();
    let is_trigger_char = trigger_char.is_some_and(|ch| {
        completion_hosts
            .iter()
            .any(|(_, host)| declares_trigger(host.as_ref(), ch))
    });

    // A server asked to answer a trigger character wants to answer it, so the
    // cached list is not consulted even where the prefix only grew.
    if !is_trigger_char {
        match refine_open_popup(stoat, &owned) {
            Refine::Fresh => {
                // The edit moved the cursor somewhere the open popup was never
                // filtered for, so it goes now rather than lingering until the
                // request it triggers lands.
                if let Some(popup) = &stoat.pending_completion
                    && cursor_left_popup(popup, owned.cursor_offset)
                {
                    stoat.pending_completion = None;
                }
            },
            Refine::Done(popup) => {
                stoat.pending_completion_request = None;
                install_popup(stoat, popup);
                return;
            },
            Refine::Ask { servers } => {
                ask_again(stoat, &snapshot, owned, servers);
                return;
            },
        }
    }

    let mut sources = applicable_sources(&owned.as_borrowed());
    if is_trigger_char && !sources.contains(&CompletionSource::Lsp) {
        let at = sources
            .iter()
            .position(|s| matches!(s, CompletionSource::Word))
            .unwrap_or(sources.len());
        sources.insert(at, CompletionSource::Lsp);
    }
    if sources.is_empty() {
        stoat.pending_completion_request = None;
        stoat.pending_completion = None;
        return;
    }

    let fs_host = stoat.fs_host.clone();
    let executor = stoat.executor.clone();
    let home_dir = stoat.env_host.var("HOME").map(PathBuf::from);
    let base_dir = base_dir_for(snapshot.source_path.as_deref(), &snapshot.git_root);

    let trigger = match trigger_char.filter(|_| is_trigger_char) {
        Some(ch) => LspTrigger::Typed(ch),
        None => LspTrigger::Invoked,
    };

    // The pieces a position is built from rather than a built one, since each
    // host needs the position in the encoding it negotiated.
    let lsp_request = (sources.contains(&CompletionSource::Lsp) && !completion_hosts.is_empty())
        .then(|| LspRequest {
            source_path: snapshot.source_path.clone(),
            cursor_offset: snapshot.cursor_offset,
            trigger,
        });

    // A trigger character skips the completion debounce, so without this the
    // position it carries would reach a server whose document is still waiting
    // on the change debounce and does not have that character yet. A debounced
    // request waits long enough that the change has already gone out, and
    // forcing it there would only defeat the quiet window.
    let pending_change = is_trigger_char
        .then(|| crate::lsp::sync::flush_pending_did_change(stoat, snapshot.buffer_id))
        .flatten();

    let task = stoat.spawn_woken(run_request(
        executor,
        owned,
        sources,
        completion_hosts,
        fs_host,
        snapshot.buffer,
        base_dir,
        home_dir,
        lsp_request,
        is_trigger_char,
        pending_change,
    ));
    stoat.pending_completion_request = Some(task);
}

/// Whether the cursor has left the span `popup` was filtered for.
fn cursor_left_popup(popup: &CompletionPopup, cursor_offset: usize) -> bool {
    cursor_offset < popup.prefix_range.start || cursor_offset > popup.prefix_range.end
}

/// What an open popup can do for a prefix that has grown since it was filled.
enum Refine {
    /// Nothing cached applies, so ask every source afresh.
    Fresh,
    /// The cached items answer this prefix outright.
    Done(CompletionPopup),
    /// These servers stopped early last time and are asked again. Everything
    /// else stays on the open popup and is merged back when the answer lands.
    Ask { servers: Vec<String> },
}

/// What a landed completion request means for the open popup.
///
/// A re-ask is not a whole answer, so it cannot simply be installed. Keeping
/// the two apart is what lets the popup's own items stay where they are for the
/// whole debounce window instead of being copied out of it per keystroke.
pub(crate) enum RequestOutcome {
    /// A full fetch, which stands alone and replaces whatever is open.
    Replace(CompletionPopup),
    /// Answers from the servers that stopped early last time.
    ///
    /// `fresh` holds only those servers' items, unranked, since ranking has to
    /// wait until the popup's surviving items have joined them. `asked` names
    /// the servers whose earlier items this supersedes, which is every server
    /// re-asked and not merely the ones that answered.
    Refill {
        fresh: CompletionPopup,
        asked: Vec<String>,
    },
}

/// Decide what the open popup can do for `owned`'s prefix.
///
/// The cached items only answer a question narrower than the one they were
/// fetched for, so the prefix has to have grown at the same anchor and to still
/// start with what it grew from. A prefix that shrank, moved, or diverged can
/// match items that were never fetched.
///
/// The items are filtered as well as re-ranked, which the install path
/// deliberately does not do. A server filters for itself and its list is
/// authoritative at the moment it answered, but between answers the client is
/// what narrows, so a cached item the prefix no longer matches goes rather than
/// sinking down the list.
fn refine_open_popup(stoat: &mut Stoat, owned: &ContextOwned) -> Refine {
    let Some(popup) = &stoat.pending_completion else {
        return Refine::Fresh;
    };

    let grew = owned.prefix_range.start == popup.prefix_range.start
        && owned.prefix.len() > popup.prefix.len()
        && owned.prefix.starts_with(&popup.prefix);
    if !grew {
        return Refine::Fresh;
    }

    // A server that stopped early is re-asked, and `ask_again` does not replace
    // the popup until its request lands a debounce later, so the popup stays
    // where it is and keeps its items. Taking it would blank the popup for that
    // window, on most keystrokes, since servers mark their lists incomplete
    // routinely, and copying them out would pay for a filter the landing
    // request redoes anyway.
    if !popup.incomplete.is_empty() {
        return Refine::Ask {
            servers: popup.incomplete.clone(),
        };
    }

    // Nothing is re-asked, so this call replaces the popup outright. The items
    // carry over untouched. Only which of them match, and in what order, has
    // moved.
    let popup = stoat
        .pending_completion
        .take()
        .expect("borrowed one just above");

    let mut matches = surviving(&popup.items, &popup.matches, &[], &owned.prefix);
    rank_scored(&popup.items, &mut matches);

    Refine::Done(CompletionPopup {
        items: popup.items,
        matches,
        selected_idx: 0,
        anchor_offset: owned.prefix_range.start,
        prefix_range: owned.prefix_range.clone(),
        prefix: owned.prefix.clone(),
        incomplete: Vec::new(),
    })
}

/// The rows of `matches` that `prefix` still matches, in the order they were
/// given, rescored against it.
///
/// Rows naming a server in `stale` are dropped whatever they score. That server
/// is being asked again, and its own answer supersedes what it said before.
///
/// Scoring and filtering are the same pass because the score is what decides
/// the match. A caller that ranks what comes back therefore has its scores
/// already and does not go over the haystacks twice.
///
/// Narrows the index rather than the items, so a keystroke over a large popup
/// moves a pair of `u32`s per row rather than a struct of four `String`s.
fn surviving(
    items: &[CompletionItem],
    matches: &[(u32, u32)],
    stale: &[String],
    prefix: &str,
) -> Vec<(u32, u32)> {
    let fresh: Vec<(u32, u32)> = matches
        .iter()
        .copied()
        .filter(|&(index, _)| {
            items[index as usize]
                .server
                .as_deref()
                .is_none_or(|name| !stale.iter().any(|s| s == name))
        })
        .collect();

    let haystacks: Vec<&str> = fresh
        .iter()
        .map(|&(index, _)| match_against(&items[index as usize]))
        .collect();

    scores_for(&haystacks, prefix)
        .into_iter()
        .zip(&fresh)
        .filter_map(|(score, &(index, _))| Some((index, score?)))
        .collect()
}

/// Every item as a match row, scored against `prefix`, keeping the ones it does
/// not match at zero.
///
/// A fresh popup shows everything its sources answered. A server filters for
/// itself and offers items whose labels the prefix does not match, so dropping
/// them here would hide answers it meant the reader to see. Narrowing a popup
/// already open is [`surviving`], which does drop them.
fn all_matches(items: &[CompletionItem], prefix: &str) -> Vec<(u32, u32)> {
    let haystacks: Vec<&str> = items.iter().map(match_against).collect();
    scores_for(&haystacks, prefix)
        .into_iter()
        .enumerate()
        .map(|(index, score)| (index as u32, score.unwrap_or(0)))
        .collect()
}

/// Ask `servers` again for a prefix they answered incompletely, leaving the
/// items held from everywhere else on the open popup until the answer lands.
///
/// Re-requesting everything would throw away answers that are still good, and
/// leaving the server out would leave the popup permanently short of what it
/// has to offer. The debounce stays, so typing through several characters asks
/// once rather than once a keystroke.
fn ask_again(
    stoat: &mut Stoat,
    snapshot: &EditorSnapshot,
    owned: ContextOwned,
    servers: Vec<String>,
) {
    let hosts: Vec<(String, Arc<dyn LspHost>)> = crate::lsp::hosts::feature_hosts(
        stoat,
        snapshot.buffer_id,
        LanguageServerFeature::Completion,
    )
    .into_iter()
    .filter(|(name, _)| servers.iter().any(|wanted| wanted == name))
    .collect();

    // Unbuilt, so each host is asked in the encoding it negotiated rather than
    // in whatever the first of them uses.
    let request = LspRequest {
        source_path: snapshot.source_path.clone(),
        cursor_offset: snapshot.cursor_offset,
        trigger: LspTrigger::Incomplete,
    };

    let Some(request) =
        Some(request).filter(|_| !hosts.is_empty() && snapshot.source_path.is_some())
    else {
        // The server is no longer there to ask, so the popup's own items are all
        // of it. Narrowing them to the grown prefix is what the landing request
        // would otherwise have done, and the items carry over as they are.
        let narrowed = stoat.pending_completion.take().map(|popup| {
            let mut matches = surviving(&popup.items, &popup.matches, &servers, &owned.prefix);
            rank_scored(&popup.items, &mut matches);
            (popup.items, matches)
        });
        let (items, matches) = narrowed.unwrap_or_else(|| (Arc::from([]), Vec::new()));

        stoat.pending_completion_request = None;
        install_popup(
            stoat,
            CompletionPopup {
                items,
                matches,
                selected_idx: 0,
                anchor_offset: owned.prefix_range.start,
                prefix_range: owned.prefix_range,
                prefix: owned.prefix,
                incomplete: Vec::new(),
            },
        );
        return;
    };

    let executor = stoat.executor.clone();
    let buffer = snapshot.buffer.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(COMPLETION_DEBOUNCE).await;

        let mut items: Vec<CompletionItem> = Vec::new();
        let mut incomplete = Vec::new();
        for (name, host) in &hosts {
            let encoding = host.offset_encoding();
            let Some(params) = build_lsp_params(
                request.source_path.as_deref(),
                &buffer.visible_text,
                request.cursor_offset,
                encoding,
                Some(host_context(host.as_ref(), request.trigger)),
            ) else {
                continue;
            };
            let (raw, complete) = crate::completion::lsp::fetch(host.as_ref(), params).await;

            if !complete {
                incomplete.push(name.clone());
            }

            let server: Arc<str> = Arc::from(name.as_str());
            items.extend(
                executor
                    .spawn_blocking({
                        let owned = owned.clone();
                        let buffer = buffer.clone();
                        move || {
                            let ctx = owned.as_borrowed();
                            crate::completion::lsp::translate_all(
                                raw, &ctx, &buffer, &server, encoding,
                            )
                        }
                    })
                    .await,
            );
        }

        // These go back unranked. The popup's survivors join them at install,
        // and ranking a partial list only to re-rank the whole one is wasted.
        RequestOutcome::Refill {
            fresh: CompletionPopup {
                anchor_offset: owned.prefix_range.start,
                prefix_range: owned.prefix_range,
                prefix: owned.prefix,
                incomplete,
                ..CompletionPopup::showing(items)
            },
            asked: servers,
        }
    });
    stoat.pending_completion_request = Some(task);
}

/// Install `popup` as the current one, or clear the popup when it came out
/// empty, the way a landed request does.
fn install_popup(stoat: &mut Stoat, popup: CompletionPopup) {
    if popup.items.is_empty() {
        stoat.pending_completion = None;
    } else {
        stoat.completion_generation = stoat.completion_generation.wrapping_add(1);
        stoat.pending_completion = Some(popup);
    }
    crate::action_handlers::completion::arm_completion_resolve(stoat);
}

/// Merge a re-ask's answer with what the popup it was asked for still holds.
///
/// The popup was left alone for the whole debounce window, so this is where its
/// items are narrowed to the prefix the ask captured and the re-asked servers'
/// earlier answers give way to their new ones. Moving them through rather than
/// copying is the point of deferring the merge to here.
///
/// A popup gone by now was accepted or dismissed mid-flight, and reviving what
/// it held would undo that, so only the fresh items install.
fn install_refill(stoat: &mut Stoat, fresh: CompletionPopup, asked: Vec<String>) {
    // The one place a popup's item list is rebuilt, because this is the one
    // place its membership grows. Everywhere else narrows the index instead.
    let mut items: Vec<CompletionItem> = match stoat.pending_completion.take() {
        Some(popup) => surviving(&popup.items, &popup.matches, &asked, &fresh.prefix)
            .into_iter()
            .map(|(index, _)| popup.items[index as usize].clone())
            .collect(),
        None => Vec::new(),
    };
    items.extend(fresh.items.iter().cloned());
    let matches = rank_by_prefix(&items, &fresh.prefix);

    install_popup(
        stoat,
        CompletionPopup {
            items: items.into(),
            matches,
            selected_idx: 0,
            anchor_offset: fresh.anchor_offset,
            prefix_range: fresh.prefix_range,
            prefix: fresh.prefix,
            incomplete: fresh.incomplete,
        },
    );
}

/// Poll the in-flight completion task. On `Ready` resolves its
/// [`RequestOutcome`] against [`Stoat::pending_completion`], installing a full
/// answer outright and merging a re-ask into what is open. Returns `true` when
/// the popup state changed, mirroring the convention used by the
/// other LSP pumps so the render loop can drive both for free.
pub(crate) fn pump(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_completion_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(RequestOutcome::Replace(popup)) => {
            install_popup(stoat, popup);
            true
        },
        Poll::Ready(RequestOutcome::Refill { fresh, asked }) => {
            install_refill(stoat, fresh, asked);
            true
        },
        Poll::Pending => {
            stoat.pending_completion_request = Some(task);
            false
        },
    }
}

struct EditorSnapshot {
    /// The whole snapshot rather than its rope, since the sources anchor their
    /// replace ranges against the fragment tree they read them off.
    buffer: TextBufferSnapshot,
    cursor_offset: usize,
    buffer_id: BufferId,
    buffer_version: u64,
    source_path: Option<PathBuf>,
    git_root: PathBuf,
}

fn insert_mode_in_editor_pane(stoat: &Stoat) -> bool {
    if stoat.focused_mode() != "insert" {
        return false;
    }
    // A modal input (finder, palette, isearch, rename, ...) is an off-pane
    // InputView that leaves `ws.focus` on the editor, so the mode and focus
    // checks below still pass while one is open. It owns the keystream, so
    // completion must not arm from the editor's cursor beneath it.
    if keymap_state::modal_predicate(stoat).is_some() {
        return false;
    }
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return false;
    };
    let pane_id = ws.panes.focus();
    matches!(ws.panes.pane(pane_id).view, View::Editor(_))
}

fn focused_editor_snapshot(stoat: &Stoat) -> Option<EditorSnapshot> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    let pane_id = ws.panes.focus();
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return None;
    };
    let editor = ws.editors.get(editor_id)?;
    let sel = editor.selections.newest_anchor();
    let buffer_id = editor.buffer_id;
    let buffer = ws.buffers.get(buffer_id)?;
    let guard = buffer.read().expect("buffer lock");
    let tail_off = guard.resolve_anchor(&sel.tail());
    let head_off = guard.resolve_anchor(&sel.head());
    let cursor_offset = stoat_text::cursor_offset(guard.rope(), tail_off, head_off);
    let buffer_snapshot = guard.snapshot.clone();
    let buffer_version = guard.version();
    drop(guard);
    let source_path = ws.buffers.path_for(buffer_id).map(Path::to_path_buf);
    Some(EditorSnapshot {
        buffer: buffer_snapshot,
        cursor_offset,
        buffer_id,
        buffer_version,
        source_path,
        git_root: ws.git_root.clone(),
    })
}

fn focused_buffer_signature(stoat: &Stoat) -> Option<(BufferId, u64)> {
    let (buffer_id, version, _) = focused_buffer_probe(stoat)?;
    Some((buffer_id, version))
}

/// The focused editor's `(buffer_id, version, cursor_offset)`.
///
/// What [`focused_editor_snapshot`] reads to decide whether the event is worth
/// answering at all, without the rope clone and two path allocations it also
/// makes. Runs on every insert-mode keystroke, most of which the dedupe it
/// feeds discards.
fn focused_buffer_probe(stoat: &Stoat) -> Option<(BufferId, u64, usize)> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    let pane_id = ws.panes.focus();
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return None;
    };
    let editor = ws.editors.get(editor_id)?;
    let sel = editor.selections.newest_anchor();
    let buffer = ws.buffers.get(editor.buffer_id)?;
    let guard = buffer.read().expect("buffer lock");
    let tail_off = guard.resolve_anchor(&sel.tail());
    let head_off = guard.resolve_anchor(&sel.head());
    Some((
        editor.buffer_id,
        guard.version(),
        stoat_text::cursor_offset(guard.rope(), tail_off, head_off),
    ))
}

/// The context the completion trigger computed for this event, when the buffer
/// and cursor have not moved since.
///
/// Both triggers run on the same event and ask the same question, so the second
/// one walking the rope again is pure repetition.
pub(crate) fn cached_context(
    stoat: &Stoat,
    buffer_id: BufferId,
    version: u64,
    cursor_offset: usize,
) -> Option<ContextOwned> {
    let (key, context) = stoat.completion_context.as_ref()?;
    (*key == (buffer_id, version, cursor_offset)).then(|| context.clone())
}

fn base_dir_for(source_path: Option<&Path>, git_root: &Path) -> PathBuf {
    source_path
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| git_root.to_path_buf())
}

/// Whether `host` declared `ch` among its completion trigger characters.
///
/// Typing one fires completion immediately instead of waiting out the prefix
/// debounce. Each declared trigger is a single character (`.`, `:`), so a
/// longer one matches nothing.
fn declares_trigger(host: &dyn LspHost, ch: char) -> bool {
    host.capabilities()
        .completion_provider
        .as_ref()
        .and_then(|provider| provider.trigger_characters.as_ref())
        .is_some_and(|triggers| {
            triggers
                .iter()
                .any(|trigger| trigger.len() == ch.len_utf8() && trigger.starts_with(ch))
        })
}

/// Why a completion request went out.
///
/// Left unresolved because the answer is per server. The same keystroke is a
/// trigger character to a server that declared it and a plain invocation to one
/// that did not. Telling a server it triggered on a character it never asked
/// for invites an answer it withholds otherwise.
#[derive(Clone, Copy)]
enum LspTrigger {
    /// Nothing a server declared was typed, so every server is asked outright.
    Invoked,
    /// This character was typed and at least one server declared it.
    Typed(char),
    /// A re-ask of the servers that stopped early on the last answer.
    Incomplete,
}

/// The context `host` is asked with, resolved against its own declarations.
fn host_context(host: &dyn LspHost, trigger: LspTrigger) -> LspCompletionContext {
    let invoked = LspCompletionContext {
        trigger_kind: CompletionTriggerKind::INVOKED,
        trigger_character: None,
    };
    match trigger {
        LspTrigger::Invoked => invoked,
        LspTrigger::Incomplete => LspCompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS,
            trigger_character: None,
        },
        LspTrigger::Typed(ch) if declares_trigger(host, ch) => LspCompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(ch.to_string()),
        },
        LspTrigger::Typed(_) => invoked,
    }
}

/// What a completion request needs to build its position, held unbuilt so each
/// host can have it in the encoding that host negotiated.
struct LspRequest {
    source_path: Option<PathBuf>,
    cursor_offset: usize,
    trigger: LspTrigger,
}

fn build_lsp_params(
    source_path: Option<&Path>,
    rope: &Rope,
    cursor_offset: usize,
    encoding: OffsetEncoding,
    context: Option<LspCompletionContext>,
) -> Option<CompletionParams> {
    let path = source_path?;
    let uri = crate::action_handlers::lsp::path_to_uri(path)?;
    let position = util::byte_offset_to_lsp_pos(rope, cursor_offset, encoding);
    Some(CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_request(
    executor: stoat_scheduler::Executor,
    owned: ContextOwned,
    sources: Vec<CompletionSource>,
    completion_hosts: Vec<(String, Arc<dyn LspHost>)>,
    fs_host: Arc<dyn FsHost>,
    buffer: TextBufferSnapshot,
    base_dir: PathBuf,
    home_dir: Option<PathBuf>,
    lsp_request: Option<LspRequest>,
    immediate: bool,
    pending_change: Option<Task<()>>,
) -> RequestOutcome {
    // The server has to have the edit this position was measured after before
    // the request naming that position goes out.
    if let Some(pending_change) = pending_change {
        pending_change.await;
    }
    if !immediate {
        executor.timer(COMPLETION_DEBOUNCE).await;
    }

    // Started before the sources are walked, so the whole-rope scan runs beside
    // the LSP round trips rather than after the last of them has landed. Word is
    // the last source when it applies, so appending its items once the loop is
    // done leaves them exactly where walking in order would have put them.
    //
    // A request the next keystroke replaces may now have started a scan whose
    // answer nobody reads. That is the cost of taking the scan off the critical
    // path of every request that does complete.
    debug_assert!(
        sources.last() == Some(&CompletionSource::Word)
            || !sources.contains(&CompletionSource::Word),
        "word items are appended after the loop, so word has to be the last source",
    );
    let words = sources.contains(&CompletionSource::Word).then(|| {
        executor.spawn_blocking({
            let owned = owned.clone();
            let buffer = buffer.clone();
            move || {
                let ctx = owned.as_borrowed();
                crate::completion::word::fetch(&ctx, &buffer)
            }
        })
    });

    let ctx = owned.as_borrowed();
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut incomplete: Vec<String> = Vec::new();
    for source in &sources {
        match source {
            CompletionSource::Path => {
                items.extend(crate::completion::path::fetch(
                    &ctx,
                    &buffer,
                    fs_host.as_ref(),
                    &base_dir,
                    home_dir.as_deref(),
                ));
            },
            CompletionSource::Lsp => {
                if let Some(request) = &lsp_request {
                    for (name, host) in &completion_hosts {
                        // Built per host: the position and the edits that come
                        // back are both in the encoding this host negotiated.
                        let encoding = host.offset_encoding();
                        let Some(params) = build_lsp_params(
                            request.source_path.as_deref(),
                            &buffer.visible_text,
                            request.cursor_offset,
                            encoding,
                            Some(host_context(host.as_ref(), request.trigger)),
                        ) else {
                            continue;
                        };
                        let (raw, complete) =
                            crate::completion::lsp::fetch(host.as_ref(), params).await;

                        if !complete {
                            incomplete.push(name.clone());
                        }

                        // Translating a large answer costs several string
                        // clones, two anchors, and two rope descents per item.
                        // The scheduler pumps this future on the run-loop
                        // thread, so the walk goes to the pool as the word
                        // source above already does.
                        let server: Arc<str> = Arc::from(name.as_str());
                        items.extend(
                            executor
                                .spawn_blocking({
                                    let owned = owned.clone();
                                    let buffer = buffer.clone();
                                    move || {
                                        let ctx = owned.as_borrowed();
                                        crate::completion::lsp::translate_all(
                                            raw, &ctx, &buffer, &server, encoding,
                                        )
                                    }
                                })
                                .await,
                        );
                    }
                }
            },
            // Spawned ahead of this loop and collected below.
            CompletionSource::Word => {},
        }
    }

    if let Some(words) = words {
        items.extend(words.await);
    }

    // Scores every item against the prefix and sorts, so it follows the size of
    // the whole answer rather than what the popup shows. The last stretch of
    // work here, and the last one that belonged on the run loop.
    let (items, matches) = executor
        .spawn_blocking({
            let prefix = owned.prefix.clone();
            move || {
                let matches = rank_by_prefix(&items, &prefix);
                (items, matches)
            }
        })
        .await;

    RequestOutcome::Replace(CompletionPopup {
        items: items.into(),
        matches,
        selected_idx: 0,
        anchor_offset: owned.prefix_range.start,
        prefix_range: owned.prefix_range,
        prefix: owned.prefix,
        incomplete,
    })
}

/// Order `items` by how well each answers `prefix`.
///
/// Sources are fetched one after another and concatenated, so left alone the
/// popup leads with whichever source happened to run first rather than with
/// whatever best matches what the reader typed.
///
/// Items are scored on their `filterText` where a server gave one and their
/// label otherwise, that field existing precisely for servers that want
/// matching done against something other than what they display.
///
/// Nothing is dropped. A server filters for itself and can offer an item whose
/// label the prefix does not match, so the ones that do not score keep their
/// order behind the ones that do rather than disappearing.
///
/// An empty prefix leaves the order alone, having nothing to rank by.
fn rank_by_prefix(items: &[CompletionItem], prefix: &str) -> Vec<(u32, u32)> {
    let mut matches = all_matches(items, prefix);
    if !prefix.is_empty() {
        rank_scored(items, &mut matches);
    }
    matches
}

/// Order `matches` by their servers' sort text, then by score descending.
///
/// The score rides in the match rows, so scoring happens before the ordering
/// rather than between its two passes. A caller that already knows what its
/// candidates scored does not have to score them again to rank them.
fn rank_scored(items: &[CompletionItem], matches: &mut [(u32, u32)]) {
    sort_by_sort_text(items, matches);

    // Stable, so rows the score cannot separate keep what the sort-text pass
    // left them in, and everything else keeps the order its source produced.
    matches.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
}

/// What each item scores against `prefix`, `None` where it does not match.
///
/// A match scoring zero and no match at all are different answers, and the
/// refine path drops on the second, so they stay apart here rather than
/// collapsing onto zero.
fn scores_for(haystacks: &[&str], prefix: &str) -> Vec<Option<u32>> {
    let Some(scored) = fuzzy::score_only(
        prefix,
        haystacks.iter().enumerate().map(|(idx, hay)| (idx, *hay)),
    ) else {
        // No usable atoms, so nothing is being asked and nothing is rejected.
        return vec![Some(0); haystacks.len()];
    };

    let mut out = vec![None; haystacks.len()];
    for (idx, score) in scored {
        out[idx] = Some(score);
    }
    out
}

/// The text an item is matched against, which is its `filterText` when the
/// server set one and its label otherwise.
fn match_against(item: &CompletionItem) -> &str {
    item.lsp_item
        .as_ref()
        .and_then(|lsp| lsp.filter_text.as_deref())
        .unwrap_or(&item.label)
}

/// Put the rows carrying a `sortText` in that order among themselves, leaving
/// every other row where it is.
///
/// Only LSP items have one, so ordering the whole list by it would have to
/// decide how an item with a sort text ranks against an item without, and any
/// answer to that shuffles items across sources for no reason. Confining the
/// reorder to the rows that have one asks no such question.
fn sort_by_sort_text(items: &[CompletionItem], matches: &mut [(u32, u32)]) {
    let slots: Vec<usize> = (0..matches.len())
        .filter(|&slot| sort_text(&items[matches[slot].0 as usize]).is_some())
        .collect();
    if slots.len() < 2 {
        return;
    }

    let mut sorted: Vec<(u32, u32)> = slots.iter().map(|&slot| matches[slot]).collect();
    sorted.sort_by(|a, b| {
        sort_text(&items[a.0 as usize])
            .cmp(&sort_text(&items[b.0 as usize]))
            .then(a.0.cmp(&b.0))
    });

    for (slot, row) in slots.into_iter().zip(sorted) {
        matches[slot] = row;
    }
}

fn sort_text(item: &CompletionItem) -> Option<&str> {
    item.lsp_item
        .as_ref()
        .and_then(|lsp| lsp.sort_text.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from(s)
    }

    #[test]
    fn empty_buffer_yields_empty_prefix() {
        let r = rope("");
        let ctx = compute_context(&r, 0);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.prefix_range, 0..0);
        assert_eq!(ctx.text_before_cursor, "");
    }

    #[test]
    fn identifier_prefix_at_end_of_line() {
        let r = rope("let foo");
        let ctx = compute_context(&r, 7);
        assert_eq!(ctx.prefix, "foo");
        assert_eq!(ctx.prefix_range, 4..7);
        assert_eq!(
            ctx.text_before_cursor, " foo",
            "the window is the prefix plus the character that ended it",
        );
    }

    /// The character before an empty prefix is the one a caller reads to spot a
    /// trigger character, and trigger characters are exactly the ones a prefix
    /// stops at, so the window has to reach one past it.
    #[test]
    fn a_trigger_character_is_inside_the_window_past_an_empty_prefix() {
        let r = rope("foo(");
        let ctx = compute_context(&r, 4);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.text_before_cursor, "(");
        assert_eq!(ctx.text_before_cursor.chars().last(), Some('('));
    }

    /// The walk stops at the prefix, so the cost of a keystroke does not grow
    /// with how far along the line the cursor sits.
    #[test]
    fn a_long_line_does_not_widen_the_window() {
        let mut line = "x".repeat(10_000);
        line.push_str(" foo");
        let r = rope(&line);
        let ctx = compute_context(&r, r.len());
        assert_eq!(ctx.prefix, "foo");
        assert_eq!(ctx.text_before_cursor, " foo");
    }

    #[test]
    fn path_shaped_prefix_walks_back_through_slashes_and_dots() {
        let r = rope("let p = ./src/lib");
        let ctx = compute_context(&r, r.len());
        assert_eq!(ctx.prefix, "./src/lib");
        assert_eq!(ctx.prefix_range, 8..17);
    }

    #[test]
    fn dot_slash_prefix_is_path_shaped() {
        let r = rope("./");
        let ctx = compute_context(&r, 2);
        assert_eq!(ctx.prefix, "./");
        assert_eq!(ctx.prefix_range, 0..2);
    }

    #[test]
    fn cursor_at_buffer_start_yields_empty_prefix() {
        let r = rope("foo");
        let ctx = compute_context(&r, 0);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.prefix_range, 0..0);
    }

    #[test]
    fn cursor_after_whitespace_yields_empty_prefix() {
        let r = rope("foo ");
        let ctx = compute_context(&r, 4);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.prefix_range, 4..4);
    }

    #[test]
    fn second_line_uses_line_relative_text_before_cursor() {
        let r = rope("first line\nsecond foo");
        let cursor = r.len();
        let ctx = compute_context(&r, cursor);
        assert_eq!(ctx.prefix, "foo");
        assert_eq!(ctx.text_before_cursor, " foo");
        let prefix_byte_len = "foo".len();
        assert_eq!(ctx.prefix_range, (cursor - prefix_byte_len)..cursor);

        // At column zero the window has nowhere on this line to reach, and it
        // must not reach into the line above.
        let start_of_second = "first line\n".len();
        let at_line_start = compute_context(&r, start_of_second);
        assert_eq!(at_line_start.prefix, "");
        assert_eq!(at_line_start.text_before_cursor, "");
    }

    #[test]
    fn multibyte_chars_keep_prefix_on_codepoint_boundaries() {
        let r = rope("résumé");
        let ctx = compute_context(&r, r.len());
        assert_eq!(ctx.prefix, "résumé");
        assert_eq!(ctx.prefix_range, 0..r.len());
    }

    #[test]
    fn cursor_past_end_clamps_to_buffer_length() {
        let r = rope("foo");
        let ctx = compute_context(&r, 99);
        assert_eq!(ctx.cursor_offset, 3);
        assert_eq!(ctx.prefix, "foo");
    }

    #[test]
    fn applicable_sources_picks_path_for_path_shaped_prefix() {
        let r = rope("./");
        let ctx_owned = compute_context(&r, 2);
        let sources = applicable_sources(&ctx_owned.as_borrowed());
        assert_eq!(sources, vec![CompletionSource::Path]);
    }

    #[test]
    fn applicable_sources_picks_lsp_word_for_identifier_prefix() {
        let r = rope("foo");
        let ctx_owned = compute_context(&r, 3);
        let sources = applicable_sources(&ctx_owned.as_borrowed());
        assert_eq!(sources, vec![CompletionSource::Lsp, CompletionSource::Word]);
    }
}

#[cfg(test)]
mod harness_tests {
    use super::*;
    use crate::{
        action_handlers::dispatch, completion::CompletionSource, test_harness::TestHarness,
    };
    use lsp_types::{CompletionOptions, ServerCapabilities};
    use std::path::PathBuf;
    use stoat_action::OpenFile;

    fn enable_completion(h: &TestHarness) {
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        });
    }

    fn enable_completion_with_triggers(h: &TestHarness, triggers: &[&str]) {
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(triggers.iter().map(|t| t.to_string()).collect()),
                ..CompletionOptions::default()
            }),
            ..ServerCapabilities::default()
        });
    }

    fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    /// The labels `matches` names, in the order it names them.
    fn ranked(items: &[CompletionItem], matches: &[(u32, u32)]) -> Vec<String> {
        matches
            .iter()
            .map(|&(index, _)| items[index as usize].label.clone())
            .collect()
    }

    /// The labels the popup shows, in the order it shows them.
    fn shown(popup: &CompletionPopup) -> Vec<String> {
        popup.rows().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn trigger_character_fires_immediately_with_context() {
        let mut h = TestHarness::default();
        enable_completion_with_triggers(&h, &["."]);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text(".");
        // A trigger character skips the prefix debounce, so no clock advance.
        h.settle();

        let observed = h.fake_lsp().observed_completions();
        assert_eq!(
            observed.len(),
            1,
            "trigger char issues an immediate request"
        );
        assert_eq!(
            observed[0].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(".".to_string()),
            }),
        );
    }

    #[test]
    fn a_trigger_character_reaches_the_server_before_the_request_naming_it() {
        use lsp_types::{TextDocumentSyncCapability, TextDocumentSyncKind};
        let mut h = TestHarness::default();
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![".".to_string()]),
                ..CompletionOptions::default()
            }),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::INCREMENTAL,
            )),
            ..ServerCapabilities::default()
        });
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text(".");
        // No clock advance, so the change debounce has not run out on its own.
        h.settle();

        assert_eq!(
            h.fake_lsp().observed_completions().len(),
            1,
            "the trigger character issues its request at once",
        );

        let changes = h.fake_lsp().observed_changes();
        let sent: Vec<String> = changes
            .iter()
            .flat_map(|c| c.content_changes.iter().map(|e| e.text.clone()))
            .collect();
        assert_eq!(
            sent,
            vec![".".to_string()],
            "the request named a position past a character the server was never sent",
        );
    }

    #[test]
    fn each_host_is_asked_in_the_encoding_it_negotiated() {
        use crate::host::OffsetEncoding;

        let mut h = TestHarness::default();
        let utf8 = h.install_lsp_server("rust", "utf8");
        let utf16 = h.install_lsp_server("rust", "utf16");
        for (host, encoding) in [
            (&utf8, OffsetEncoding::Utf8),
            (&utf16, OffsetEncoding::Utf16),
        ] {
            // Capabilities first: setting them replaces the struct the encoding
            // is stored in.
            host.set_capabilities(ServerCapabilities {
                completion_provider: Some(CompletionOptions::default()),
                ..ServerCapabilities::default()
            });
            host.set_offset_encoding(encoding);
        }

        // é is two UTF-8 bytes but one UTF-16 unit, so a cursor after "éab"
        // sits at character 4 for the first server and 3 for the second.
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), "".as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.type_keys("i");
        h.type_text("\u{e9}ab");
        h.advance_clock(COMPLETION_DEBOUNCE);

        let character = |host: &Arc<crate::host::FakeLsp>| {
            host.observed_completions()[0]
                .text_document_position
                .position
                .character
        };
        assert_eq!(character(&utf8), 4, "utf-8 counts e-acute as two");
        assert_eq!(character(&utf16), 3, "utf-16 counts it as one");
    }

    /// Two completion servers on one language, only `second` declaring `:`.
    ///
    /// This is the shape the emoji server arrives in. The language own server
    /// has no interest in the character a second server exists to answer.
    fn install_two_servers_one_declaring_colon(
        h: &mut TestHarness,
    ) -> (Arc<crate::host::FakeLsp>, Arc<crate::host::FakeLsp>) {
        let (first, second) = crate::test_fixture::install_two_servers(
            h,
            ServerCapabilities {
                completion_provider: Some(CompletionOptions::default()),
                ..ServerCapabilities::default()
            },
        );
        second.set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![":".to_string()]),
                ..CompletionOptions::default()
            }),
            ..ServerCapabilities::default()
        });
        (first, second)
    }

    #[test]
    fn a_secondary_servers_trigger_fires_the_immediate_path() {
        let mut h = TestHarness::with_size(80, 24);
        let (first, second) = install_two_servers_one_declaring_colon(&mut h);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text(":");
        h.settle();

        assert_eq!(
            second.observed_completions().len(),
            1,
            "the server that declared the character answers it without the debounce",
        );
        assert_eq!(
            first.observed_completions().len(),
            1,
            "the fan-out still reaches every completion server",
        );
    }

    #[test]
    fn each_server_is_told_the_trigger_kind_it_declared() {
        let mut h = TestHarness::with_size(80, 24);
        let (first, second) = install_two_servers_one_declaring_colon(&mut h);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text(":");
        h.settle();

        assert_eq!(
            second.observed_completions()[0].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(":".to_string()),
            }),
        );
        assert_eq!(
            first.observed_completions()[0].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
            "a server never told the editor about this character did not trigger on it",
        );
    }

    #[test]
    fn plain_letter_keeps_the_debounce_and_sends_invoked() {
        let mut h = TestHarness::default();
        enable_completion_with_triggers(&h, &["."]);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text("f");
        h.settle();
        assert!(
            h.fake_lsp().observed_completions().is_empty(),
            "a plain letter waits out the debounce",
        );

        h.advance_clock(COMPLETION_DEBOUNCE);
        let observed = h.fake_lsp().observed_completions();
        assert_eq!(
            observed.len(),
            1,
            "the request fires after the quiet window"
        );
        assert_eq!(
            observed[0].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
        );
    }

    #[test]
    fn a_modal_open_over_a_mid_word_cursor_does_not_trigger_completion() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "Greeter");

        // Sit the editor cursor inside `Greeter`, then open the finder over it.
        // The finder is an insert-mode input that leaves focus on the editor, so
        // without the modal gate `trigger` would arm a completion for the word.
        h.type_keys("ll");
        h.type_keys("space p");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.settle();

        assert!(
            h.fake_lsp().observed_completions().is_empty(),
            "a modal open must not arm a completion from the editor cursor"
        );
        assert!(
            h.stoat.pending_completion.is_none(),
            "no completion popup shows beneath a modal"
        );
    }

    #[test]
    fn identifier_in_insert_mode_opens_popup_after_debounce() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz"]);

        h.type_keys("i");
        h.type_text("foo");
        assert!(
            h.stoat.pending_completion.is_none(),
            "popup arrives only after debounce"
        );
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h.stoat.pending_completion.clone().expect("popup armed");
        let got = shown(&popup);
        assert!(
            got.iter().any(|l| l == "foobar"),
            "expected foobar in {got:?}",
        );
        assert!(
            got.iter().any(|l| l == "foobaz"),
            "expected foobaz in {got:?}",
        );
    }

    /// A motion moves the cursor without touching the buffer version, so the
    /// signature dedupe would sit the event out. The popup still has to notice
    /// the cursor leaving the range it was filtered for, which is why the range
    /// check runs ahead of that dedupe.
    #[test]
    fn a_motion_out_of_range_dismisses_the_popup() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 7, &["foobar", "foobaz"]);

        // Typed past column zero, so the prefix has somewhere to leave from.
        h.type_keys("i");
        h.type_text("bar foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        let popup = h.stoat.pending_completion.clone().expect("popup armed");
        assert!(popup.prefix_range.start > 0, "the prefix has room to leave");

        let version = |h: &mut TestHarness| {
            let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
            let ws = h.stoat.active_workspace();
            ws.buffers
                .get(buffer_id)
                .expect("buffer")
                .read()
                .expect("poisoned")
                .version()
        };
        let before = version(&mut h);

        // Arrowing back off the prefix moves the cursor and edits nothing.
        h.type_keys("left left left left");
        assert_eq!(version(&mut h), before, "a motion changes no text");
        assert!(
            h.stoat.pending_completion.is_none(),
            "the cursor left the range the popup was filtered for",
        );
    }

    #[test]
    fn a_re_query_bumps_the_generation_while_a_stable_popup_does_not() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz"]);

        assert_eq!(h.stoat.completion_generation, 0, "no completion armed yet");

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some(), "popup armed");
        assert_eq!(
            h.stoat.completion_generation, 1,
            "installing a popup bumps the generation"
        );

        // Settling with no new query leaves the popup in place, so the pool's
        // content version stays put across emits.
        h.settle();
        assert_eq!(
            h.stoat.completion_generation, 1,
            "a stable popup does not bump the generation"
        );

        // Extending the prefix re-queries and installs fresh items.
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 4, &["foobar"]);
        h.type_text("b");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some(), "re-query re-armed");
        assert_eq!(
            h.stoat.completion_generation, 2,
            "a re-query bumps the generation so the pool refills"
        );
    }

    #[test]
    fn path_prefix_in_insert_opens_path_popup() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        h.fake_fs().insert_file("/ws/lib.rs", b"");
        h.fake_fs().insert_file("/ws/main.rs", b"");
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), b"".as_slice())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.type_keys("i");
        h.type_text("./");
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h
            .stoat
            .pending_completion
            .clone()
            .expect("path popup armed");
        let mut got: Vec<String> = shown(&popup)
            .into_iter()
            .filter(|l| l == "lib.rs" || l == "main.rs" || l == "buf.rs")
            .collect();
        got.sort();
        assert_eq!(got, vec!["buf.rs", "lib.rs", "main.rs"]);
        for item in popup.rows() {
            if matches!(item.label.as_str(), "lib.rs" | "main.rs" | "buf.rs") {
                assert_eq!(item.source, CompletionSource::Path);
            }
        }
    }

    #[test]
    fn whitespace_context_leaves_popup_empty() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 0, &["foo"]);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text("   ");
        h.advance_clock(COMPLETION_DEBOUNCE);

        assert!(
            h.stoat.pending_completion.is_none(),
            "whitespace prefix should not arm the popup",
        );
    }

    #[test]
    fn leaving_insert_mode_clears_state() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some());

        // First Esc dismisses the popup but stays in insert; second
        // Esc actually exits insert mode. Trigger fires after each
        // event, sees mode != insert on the second pass, and clears
        // every completion-related field.
        h.type_keys("escape escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
        assert!(h.stoat.pending_completion.is_none());
        assert!(h.stoat.pending_completion_request.is_none());
        assert!(h.stoat.last_completion_signature.is_none());
    }

    #[test]
    fn rapid_typing_cancels_in_flight_request() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_request_delay("textDocument/completion", Duration::from_millis(500));
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("f");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.type_text("o");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.type_text("o");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.advance_clock(Duration::from_millis(500));

        let cancelled = h.fake_lsp().cancelled_requests();
        let completion_cancellations = cancelled
            .iter()
            .filter(|m| m == &"textDocument/completion")
            .count();
        assert!(
            completion_cancellations >= 2,
            "expected at least 2 cancelled completion requests, got {cancelled:?}",
        );
    }

    #[test]
    fn esc_dismisses_popup_and_leaves_insert_in_one_press() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some());

        h.type_keys("escape");
        assert!(
            h.stoat.pending_completion.is_none(),
            "one escape closes the popup"
        );
        assert!(h.stoat.pending_completion_request.is_none());
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the same escape also leaves insert mode",
        );

        h.type_keys("i");
        h.type_text("x");
        assert!(
            h.stoat.pending_completion_request.is_some(),
            "typing after re-entering insert arms a fresh request",
        );
    }

    /// A buffer-word item, which carries no server fields.
    fn word(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            documentation: None,
            replace_range: crate::completion::unused_replace_range(),
            insert_text: label.to_string(),
            is_snippet: false,
            lsp_item: None,
            server: None,
        }
    }

    /// A server item, optionally carrying the two fields a server uses to steer
    /// matching and ordering.
    fn served(label: &str, filter_text: Option<&str>, sort_text: Option<&str>) -> CompletionItem {
        CompletionItem {
            source: CompletionSource::Lsp,
            lsp_item: Some(Box::new(lsp_types::CompletionItem {
                label: label.to_string(),
                filter_text: filter_text.map(str::to_string),
                sort_text: sort_text.map(str::to_string),
                ..Default::default()
            })),
            server: Some(Arc::from("test")),
            ..word(label)
        }
    }

    /// An identifier context asks the server and the buffer both, and the scan
    /// of the second runs beside the first rather than after it has answered.
    /// Both sets still have to reach the popup.
    #[test]
    fn an_identifier_popup_carries_the_servers_items_and_the_buffers_words() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "foxtrot\n");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 10, &["foobar"]);

        // Typed at the end of the line, so the word already on it is a token of
        // its own for the scan to find rather than part of the prefix.
        h.type_keys("A");
        h.type_text(" fo");
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h.stoat.pending_completion.as_ref().expect("popup armed");
        let mut got = shown(popup);
        got.sort();
        assert_eq!(got, ["foobar", "foxtrot"]);
    }

    /// A popup over `prefix`, anchored at the start of the line.
    fn popup_over(items: Vec<CompletionItem>, prefix: &str) -> CompletionPopup {
        CompletionPopup {
            prefix_range: 0..prefix.len(),
            prefix: prefix.to_string(),
            ..CompletionPopup::showing(items)
        }
    }

    /// A re-ask answers for one server, so everything the popup holds from
    /// elsewhere has to come through with it rather than being asked for again.
    /// The re-asked server's own earlier items do not come through however well
    /// they still match, since its new answer supersedes them.
    #[test]
    fn a_landed_re_ask_merges_what_the_popup_still_holds() {
        let mut h = TestHarness::default();
        h.stoat.pending_completion = Some(popup_over(
            vec![
                word("appleseed"),
                served("apply_theme", None, None),
                word("zebra"),
            ],
            "app",
        ));

        install_refill(
            &mut h.stoat,
            popup_over(vec![served("append", None, None)], "app"),
            vec!["test".to_string()],
        );

        let popup = h
            .stoat
            .pending_completion
            .as_ref()
            .expect("popup installed");
        let mut got = shown(popup);
        got.sort();
        assert_eq!(
            got,
            ["append", "appleseed"],
            "the fresh answer joins the surviving word, while the re-asked \
             server's old item and the word the prefix no longer matches both go",
        );
    }

    /// Accepting or dismissing while the re-ask is in flight takes the popup
    /// down, and merging must not put back what it was holding.
    #[test]
    fn a_re_ask_landing_on_a_dismissed_popup_installs_the_fresh_items_alone() {
        let mut h = TestHarness::default();
        assert!(h.stoat.pending_completion.is_none(), "nothing is open");

        install_refill(
            &mut h.stoat,
            popup_over(vec![served("append", None, None)], "app"),
            vec!["test".to_string()],
        );

        let popup = h
            .stoat
            .pending_completion
            .as_ref()
            .expect("popup installed");
        assert_eq!(shown(popup), ["append"]);
    }

    #[test]
    fn ranking_leads_with_what_answers_the_prefix_not_its_source() {
        // Interleaved as the sources produce them, with the run of characters
        // the reader typed scattered through the first and contiguous in the
        // second. The scorer separates those. It does not separate a match from
        // a longer haystack around the same match.
        let items = vec![
            word("a_pretty_pear"),
            served("apply_theme", None, None),
            word("zebra_handler"),
        ];

        let matches = rank_by_prefix(&items, "app");

        assert_eq!(
            ranked(&items, &matches),
            ["apply_theme", "a_pretty_pear", "zebra_handler"],
            "the popup leads with the best answer whichever source produced it"
        );
    }

    #[test]
    fn an_item_the_prefix_cannot_match_stays_in_the_list() {
        let items = vec![served("unrelated", None, None), word("apply")];

        let matches = rank_by_prefix(&items, "app");

        assert_eq!(
            ranked(&items, &matches),
            ["apply", "unrelated"],
            "a server filters for itself, so what it offered is kept rather than dropped"
        );
    }

    #[test]
    fn an_empty_prefix_leaves_the_sources_order_alone() {
        let items = vec![
            word("zebra"),
            served("beta", None, Some("0001")),
            served("alpha", None, Some("0000")),
        ];

        let matches = rank_by_prefix(&items, "");

        assert_eq!(
            ranked(&items, &matches),
            ["zebra", "beta", "alpha"],
            "nothing to rank by, so the order the sources gave stands"
        );
    }

    #[test]
    fn equal_scores_fall_back_to_the_servers_sort_text() {
        // Identical labels score identically, so only the sort text separates
        // them, and the word item has none to be judged by.
        let items = vec![
            served("item", Some("app"), Some("0002")),
            word("app"),
            served("item", Some("app"), Some("0001")),
        ];

        let matches = rank_by_prefix(&items, "app");

        let ordering: Vec<Option<&str>> = matches
            .iter()
            .map(|&(index, _)| sort_text(&items[index as usize]))
            .collect();
        assert_eq!(
            ordering,
            [Some("0001"), None, Some("0002")],
            "the two server items take the server's order between them, and the \
             word item they straddle keeps the position its source gave it"
        );
    }

    #[test]
    fn matching_uses_the_filter_text_a_server_supplies() {
        // The label alone would put the word item first, its characters being
        // findable in order where the server's label's are not.
        let items = vec![
            word("a_p_p_lication"),
            served("unhelpful label", Some("app"), None),
        ];

        let matches = rank_by_prefix(&items, "app");

        assert_eq!(
            ranked(&items, &matches),
            ["unhelpful label", "a_p_p_lication"],
            "a server asking to be matched on something else is matched on it"
        );
    }

    /// A popup open over `foo`, from a server that answered completely.
    fn popup_over_foo(h: &mut TestHarness) -> usize {
        enable_completion(h);
        open_scratch(h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz", "fooqux"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(
            h.stoat.pending_completion.is_some(),
            "the session has to be open for typing on to refine anything"
        );

        h.fake_lsp().observed_completions().len()
    }

    #[test]
    fn typing_on_narrows_the_popup_without_asking_again() {
        let mut h = TestHarness::default();
        let asked = popup_over_foo(&mut h);

        h.type_text("b");

        let popup = h
            .stoat
            .pending_completion
            .clone()
            .expect("the popup survives the keystroke");
        assert_eq!(
            shown(&popup),
            ["foobar", "foobaz"],
            "the rows the longer prefix cannot match are gone"
        );
        assert_eq!(
            h.fake_lsp().observed_completions().len(),
            asked,
            "and the server was not asked again"
        );
    }

    /// A keystroke over an open popup rewrites which rows it shows. The items
    /// themselves are what a large answer makes expensive to move, and nothing
    /// about them changed, so the narrowed popup holds the same list.
    #[test]
    fn narrowing_leaves_the_item_list_where_it_is() {
        let mut h = TestHarness::default();
        popup_over_foo(&mut h);

        let before = h
            .stoat
            .pending_completion
            .as_ref()
            .expect("the popup is open")
            .items
            .clone();

        h.type_text("b");

        let popup = h
            .stoat
            .pending_completion
            .as_ref()
            .expect("the popup survives the keystroke");
        assert!(
            Arc::ptr_eq(&before, &popup.items),
            "the same items answer the longer prefix",
        );
        assert!(
            popup.matches.len() < before.len(),
            "and the narrowing happened in the index",
        );
    }

    #[test]
    fn typing_on_narrows_within_the_keystroke() {
        let mut h = TestHarness::default();
        popup_over_foo(&mut h);

        h.type_text("bar");
        let popup = h.stoat.pending_completion.clone().expect("popup");

        assert_eq!(
            shown(&popup),
            ["foobar"],
            "narrowed before any debounce could have elapsed"
        );
    }

    #[test]
    fn deleting_back_asks_again() {
        let mut h = TestHarness::default();
        let asked = popup_over_foo(&mut h);
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 2, &["fizz", "foobar"]);

        h.type_keys("backspace");
        h.advance_clock(COMPLETION_DEBOUNCE);

        assert!(
            h.fake_lsp().observed_completions().len() > asked,
            "a shorter prefix can match items the longer one never fetched"
        );
    }

    #[test]
    fn a_trigger_character_asks_again_even_though_the_prefix_grew() {
        // A dot is both a trigger character and a prefix character, so typing
        // one extends the prefix and would otherwise be narrowed from what is
        // held, leaving the server that asked to answer it unasked.
        let mut h = TestHarness::default();
        enable_completion_with_triggers(&h, &["."]);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        let asked = h.fake_lsp().observed_completions().len();

        h.type_text(".");
        h.settle();

        assert!(
            h.fake_lsp().observed_completions().len() > asked,
            "the server declared the character its own, so it answers it"
        );
    }

    #[test]
    fn a_server_that_stopped_early_is_asked_again() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar"]);
        h.fake_lsp().set_completions_incomplete("/ws/buf.rs", 0, 3);
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 4, &["foobarred"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        let asked = h.fake_lsp().observed_completions().len();

        h.type_text("b");
        h.advance_clock(COMPLETION_DEBOUNCE);

        assert!(
            h.fake_lsp().observed_completions().len() > asked,
            "the server said it had more to say, so it is asked rather than narrowed"
        );
        let popup = h.stoat.pending_completion.clone().expect("popup");
        assert!(
            shown(&popup).contains(&"foobarred".to_string()),
            "and what it then offered is shown: {:?}",
            shown(&popup)
        );
    }

    #[test]
    fn a_re_ask_says_it_is_refining_an_incomplete_list() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar"]);
        h.fake_lsp().set_completions_incomplete("/ws/buf.rs", 0, 3);
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 4, &["foobarred"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        let asked = h.fake_lsp().observed_completions().len();

        h.type_text("b");
        h.advance_clock(COMPLETION_DEBOUNCE);

        assert_eq!(
            h.fake_lsp().observed_completions()[asked].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS,
                trigger_character: None,
            }),
            "a server narrowing its own unfinished list is told that is what this is",
        );
    }

    /// Re-asking a server replaces the popup only when its answer lands, a
    /// debounce later, so refining has to leave the old one standing rather
    /// than taking it. Taking it blanks the popup for that window, and servers
    /// mark their lists incomplete routinely enough for that to be most
    /// keystrokes.
    #[test]
    fn re_asking_a_server_leaves_the_popup_up_while_it_answers() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar"]);
        h.fake_lsp().set_completions_incomplete("/ws/buf.rs", 0, 3);
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 4, &["foobarred"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(
            h.stoat.pending_completion.is_some(),
            "the popup is up before the keystroke that re-asks"
        );

        h.type_text("b");
        assert!(
            h.stoat.pending_completion.is_some(),
            "and holds while the re-query is in flight, rather than blanking"
        );
    }

    #[test]
    fn the_installed_popup_is_already_ranked() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");

        // Offered worst-first, so arrival order and ranked order disagree.
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["f_o_o_bar", "foobar"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h.stoat.pending_completion.clone().expect("popup armed");
        let got = shown(&popup);
        let contiguous = got
            .iter()
            .position(|l| l == "foobar")
            .expect("the contiguous match is offered");
        let scattered = got
            .iter()
            .position(|l| l == "f_o_o_bar")
            .expect("the scattered match is offered");

        assert!(
            contiguous < scattered,
            "the popup is ranked where it is installed rather than left in arrival order: {got:?}"
        );
    }
}
