//! One buffer's syntax parse, and the token carry that keeps it incremental.
//!
//! A parse runs off the foreground thread and returns a [`ParseJobOutput`] the
//! caller installs. What makes it cheap enough to run per keystroke is that a
//! parse rarely re-queries the whole file. It carries the previous parse's
//! tokens across the edit and re-queries only the byte ranges the edit could
//! have restyled, then splices the two together.
//!
//! Nothing here reads the app, so the subsystem stays testable against a rope
//! and a language registry alone.

use crate::{
    buffer::{BufferId, TextBufferSnapshot},
    display_map::{
        highlights::{
            BufferSemanticTokens, HighlightStyleId, HighlightStyleInterner, SemanticTokenHighlight,
            SemanticTokenSpans, TokenRun,
        },
        syntax_theme::SyntaxStyles,
    },
};
use std::{borrow::Cow, ops::Range, sync::Arc};
use stoat_language::{self as language, Language, SyntaxMapCapture, SyntaxSnapshot, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::{patch::Patch, Bias, Rope};

/// Result of a successful background parse, ready to be installed on the
/// foreground thread.
pub(crate) struct ParseJobOutput {
    pub(crate) buffer_id: BufferId,
    pub(crate) syntax: SyntaxState,
    /// Multi-layer parse state from [`stoat_language::SyntaxMap::reparse`].
    /// Populated alongside [`Self::syntax`] so the legacy single-tree
    /// highlight path and the capture-merging path can run side by side
    /// while consumers migrate.
    pub(crate) syntax_map: stoat_language::SyntaxMap,
    /// The highlight channel, built once here and installed into every editor
    /// viewing the buffer. It owns the token list, so the search index inside
    /// it cannot drift from the tokens it indexes.
    pub(crate) token_channel: BufferSemanticTokens,
    /// The byte range this parse's tokens cover, when it is not the whole
    /// file.
    ///
    /// [`Some`] means the capture walk was narrowed to what was on screen, so
    /// the tokens paint the viewport but describe nothing outside it. The tree
    /// and layer map are whole either way, since only the walk was narrowed.
    /// A caller that retains these tokens as the base for a later incremental
    /// parse must not do so while this is [`Some`], or everything off screen
    /// would be carried forward as unchanged when it was never captured.
    pub(crate) captured: Option<Range<usize>>,
    /// This parse's tokens as raw byte spans, retained so the next parse can
    /// diff against them and report its own changed rows.
    pub(crate) token_spans: SemanticTokenSpans,
    /// Buffer rows whose tokens this parse changed, or `None` when that could
    /// not be determined and every row must be treated as changed.
    ///
    /// Lets a consumer holding per-row derived state, currently the minimap
    /// strip, recolor only what moved instead of the whole file per keystroke.
    pub(crate) changed_token_rows: Option<Range<u32>>,
}

/// Synchronous core of the parse pipeline. When `deadline` is `Some`, every
/// parse it runs aborts if it would exceed it and the function returns `None`,
/// signalling that the caller should fall back to the background path. An
/// abort leaves `prior` and `prior_syntax_map` untouched so that background
/// attempt still has the previous parse to work from. `None` is also returned
/// for ordinary parse failures (unsupported language, etc.); the difference
/// does not matter for the call sites.
///
/// `prior_token_spans` and `prior_token_anchors` are the previous parse's
/// tokens in its own byte coordinates and as anchors in the buffer. They must
/// be index-aligned, which they are when both come from the same parse's
/// output. Supplying both unlocks the incremental highlight path, which
/// re-queries only the byte ranges the edit could have restyled. Spans alone
/// still bound [`ParseJobOutput::changed_token_rows`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_buffer_step(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: &Arc<Language>,
    prior: &mut Option<SyntaxState>,
    prior_syntax_map: &mut Option<stoat_language::SyntaxMap>,
    prior_token_spans: Option<&[(Range<usize>, HighlightStyleId)]>,
    prior_token_anchors: Option<&BufferSemanticTokens>,
    styles: &SyntaxStyles,
    deadline: Option<(std::time::Instant, &Executor)>,
    viewport: Option<Range<u32>>,
) -> Option<ParseJobOutput> {
    let cur_version = snapshot.version;
    let new_rope = snapshot.visible_text.clone();

    // Edit a clone of the prior tree rather than mutating it in place. If
    // the parse aborts (deadline exceeded, etc.) the caller's prior must
    // remain valid for the next attempt; an in-place edit would leave the
    // registry holding a half-edited tree that would double-stamp position
    // offsets when re-edited next call.
    //
    // tree_sitter::Tree::clone is O(1) (refcount bump on the root subtree),
    // and tree.edit goes through ts_subtree_edit which is copy-on-write, so
    // editing the clone leaves the original untouched.
    let edited = prior.as_ref().map(|prev| {
        let mut tree = prev.tree.clone();
        let edits = snapshot.edits_since(prev.version);
        language::edit_tree(&mut tree, edits.edits(), &prev.rope_snapshot, &new_rope);
        (tree, edits)
    });
    let edited_tree = edited.as_ref().map(|(tree, _)| tree);
    let edits = edited.as_ref().map(|(_, edits)| edits);

    let tree = match edited_tree {
        Some(old_tree) => match deadline {
            Some((dl, exec)) => {
                language::parse_rope_within(lang, &new_rope, Some(old_tree), dl, exec)?
            },
            None => language::parse_rope(lang, &new_rope, Some(old_tree))?,
        },
        None => match deadline {
            Some((dl, exec)) => language::parse_rope_within(lang, &new_rope, None, dl, exec)?,
            None => language::parse_rope(lang, &new_rope, None)?,
        },
    };

    // Everything this edit could have restyled, which the injection re-walk
    // and the token recapture are both narrowed to. One union serves both
    // because they are asking the same question of the same pair of trees.
    let invalidated = edited
        .as_ref()
        .map(|(old_tree, edits)| invalidated_ranges(old_tree, &tree, edits, new_rope.len()));

    // Advance the multi-layer SyntaxMap the highlights are read from. With a
    // prior map this rides the interpolate-then-reparse contract. Interpolate
    // replays this version's edits onto every layer, leaving each tree
    // positioned as tree-sitter's `old_tree` and each layer's bounds on the
    // text it still covers. The reparse then re-walks the invalidated rows for
    // injection changes, keeping the layers elsewhere. Working on a clone
    // leaves the caller's map borrowed rather than taken, for a handful of
    // refcount bumps.
    //
    // The root parsed above is handed in rather than parsed a second time, so
    // what remains under the reparse is the injection layers, running on the
    // same budget. A spent budget takes the `?` here rather than the rebuild
    // below, since rebuilding from nothing would parse every one of those
    // layers again only to abort again.
    let incremental = match (prior_syntax_map.as_ref(), prior.as_ref(), edited.as_ref()) {
        (Some(prior_map), Some(prev), Some((_, edits))) => {
            let mut map = prior_map.clone();
            map.interpolate(edits.edits(), &prev.rope_snapshot, &new_rope);

            let layer_changes = map.reparse_within_changed_ranges(
                &new_rope,
                lang.clone(),
                cur_version,
                invalidated.as_deref(),
                Some(&tree),
                deadline,
            )?;
            Some((map, layer_changes))
        },
        _ => None,
    };

    let incremental_reparse = incremental.is_some();
    let (syntax_map, layer_changes) = match incremental {
        Some((map, layer_changes)) => (map, layer_changes),
        None => {
            let mut map = stoat_language::SyntaxMap::default();
            map.reparse(&new_rope, lang.clone(), cur_version, Some(&tree), deadline)?;
            (map, Vec::new())
        },
    };

    // What the re-query has to cover. The root tree names where the host
    // grammar restyled, and the reparse names where the layer set moved. An
    // injected buffer needs that second half, since a layer can restyle its
    // whole span with the host tree identical throughout.
    let recapture_ranges = invalidated.map(|mut ranges| {
        ranges.extend(layer_changes);
        ranges.sort_unstable_by_key(|r| r.start);
        merge_ranges(&mut ranges);
        ranges
    });

    // Re-query only what the edit could have restyled. The prior tokens carry
    // across the edit and keep their anchors, so a keystroke costs a query over
    // the edited region rather than one over the file. A map rebuilt from
    // nothing leaves no prior tree to compare against, so that case alone drops
    // back to the full walk below.
    let recaptured = if incremental_reparse {
        // A carried span takes its anchor by index, so both halves of the prior
        // parse's tokens have to be present and line up one for one.
        prior_token_spans
            .zip(prior_token_anchors)
            .filter(|(spans, channel)| spans.len() == channel.len())
            .zip(edits.zip(recapture_ranges))
            .and_then(|((spans, _), (edits, invalidated))| {
                recapture_edited_ranges(
                    syntax_map.snapshot(),
                    &new_rope,
                    spans,
                    edits,
                    invalidated,
                    styles,
                )
            })
    } else {
        None
    };

    // A re-query that gave up leaves the capture walk below, and that walk
    // honors no clock of its own. A parse with tokens to carry is never handed
    // a viewport to narrow it, so the walk it faces covers the whole rope,
    // which is the stall a deadline exists to prevent. Handing the parse back
    // undone sends it to the pool, where the same walk costs a frame instead.
    if deadline.is_some() && prior_token_spans.is_some() && recaptured.is_none() {
        return None;
    }

    // Past here the parse can no longer abort, so the caller's prior has done
    // its job and every abort above has left it whole for the retry.
    prior.take();
    prior_syntax_map.take();

    // The whole file unless the caller named a viewport to do first, in which
    // case that plus a margin, so an opened file styles what is on screen
    // rather than sorting every capture in it before painting anything.
    let captured = match (&recaptured, viewport) {
        (Some(_), _) | (None, None) => None,
        (None, Some(rows)) => Some(capture_window(rows, &new_rope)),
    };

    // Borrowed rather than moved out, since the recapture still owns the two
    // short lists the changed-row report reads below.
    let styled: Cow<'_, [(Range<usize>, HighlightStyleId)]> = match &recaptured {
        Some(recaptured) => Cow::Borrowed(&recaptured.spans),
        None => Cow::Owned(styled_capture_spans(
            syntax_map.snapshot().captures(
                captured.clone().unwrap_or(0..new_rope.len()),
                &new_rope,
                |l| Some(&l.highlight_query),
            ),
            styles,
        )),
    };

    // The search index is an argmax over resolved token ends, and each anchor
    // below resolves to the offset it was just built from, so the byte ends
    // answer it without resolving anything.
    let ends: Vec<usize> = styled.iter().map(|(range, _)| range.end).collect();
    let token_channel = match (&recaptured, prior_token_anchors) {
        (Some(recaptured), Some(prior)) => {
            recaptured.anchor_against(prior, styles.interner.clone(), &snapshot)
        },
        _ => BufferSemanticTokens::with_resolved_ends(
            anchor_spans(&styled, &ends, &snapshot),
            styles.interner.clone(),
            &ends,
        ),
    };

    // Which rows this parse restained, for consumers that colour per row. It
    // needs the prior parse's spans and the patch that carries them forward;
    // without either, callers must assume the whole file. A recapture compares
    // only the tokens it replaced against the ones it queried, since every
    // other token was carried across the edit and so cannot have moved.
    let changed_token_rows = match (&recaptured, prior_token_spans.zip(edits)) {
        (Some(recaptured), Some((_, edits))) => Some(changed_token_rows(
            &recaptured.replaced,
            &recaptured.fresh,
            edits,
            &new_rope,
        )),
        (_, Some((prior, edits))) => Some(changed_token_rows(prior, &styled, edits, &new_rope)),
        (_, None) => None,
    };

    Some(ParseJobOutput {
        buffer_id,
        syntax: SyntaxState {
            tree,
            version: cur_version,
            rope_snapshot: new_rope,
        },
        syntax_map,
        token_channel,
        captured,
        token_spans: Arc::from(styled),
        changed_token_rows,
    })
}

/// How many times a recapture may widen its query ranges before giving up and
/// letting the caller walk the whole file.
///
/// Each round re-queries the ranges the previous round's captures spilled out
/// of, so it settles as soon as no capture crosses a range boundary. Two
/// rounds cover the ordinary case of an edit inside one enclosing node. The
/// rest of the budget is for deeply nested captures, and exhausting it means
/// the narrowing was not paying for itself anyway.
const RECAPTURE_ROUNDS: usize = 8;

/// One cover's re-queried tokens, or nothing when no round has asked yet.
///
/// A round leaves this filled for every cover, and empties only the ones whose
/// range the next round changed.
type CoverSpans = Option<Vec<(Range<usize>, HighlightStyleId)>>;

/// A stretch of tokens that survived an edit, in both parses' index spaces.
///
/// Both ranges have the same length. The prior side names where the anchors
/// come from, the current side where they land.
struct CarriedRun {
    prior: Range<usize>,
    current: Range<usize>,
}

/// One parse's tokens, assembled from the previous parse's tokens plus a
/// re-query of the byte ranges an edit could have restyled.
struct Recaptured {
    /// Every token in document order, as byte spans in the new rope.
    spans: Vec<(Range<usize>, HighlightStyleId)>,
    /// The stretches of [`Self::spans`] that came across whole from the prior
    /// parse, each naming the prior indices it was carried from.
    ///
    /// Anchoring reads this rather than a per-token record, because a carried
    /// stretch's anchors are the prior ones untouched and can be taken by
    /// refcount. Stretches are in document order and disjoint, and whatever
    /// falls between them is a token this parse queried afresh.
    carried: Vec<CarriedRun>,
    /// The prior tokens the re-query replaced, in the prior parse's byte
    /// coordinates and document order.
    replaced: Vec<(Range<usize>, HighlightStyleId)>,
    /// The tokens the re-query produced, in document order.
    ///
    /// Every token not in here was carried across the edit unchanged, so this
    /// and [`Self::replaced`] are between them the whole difference between the
    /// two parses. That is what lets the changed-row report be a comparison of
    /// two short lists rather than of the file.
    fresh: Vec<(Range<usize>, HighlightStyleId)>,
}

impl Recaptured {
    /// Anchor the tokens, reusing `prior`'s anchors for every carried one.
    ///
    /// A carried token names the same text as the prior token it came from,
    /// and an anchor already follows text across edits, so re-anchoring it
    /// would only recompute what it already holds. Only the re-queried tokens
    /// are minted, through the same batched pair the full walk uses.
    ///
    /// A carried stretch is taken as whole segments of `prior` wherever its
    /// bounds allow, so most of a large file's anchors reach the new channel by
    /// refcount rather than being copied into a fresh allocation.
    fn anchor_against(
        &self,
        prior: &BufferSemanticTokens,
        interner: Arc<HighlightStyleInterner>,
        snapshot: &TextBufferSnapshot,
    ) -> BufferSemanticTokens {
        // Everything outside a carried stretch this parse queried, so those are
        // the only anchors to mint.
        let mut fresh: Vec<usize> = Vec::new();
        let mut at = 0;
        for run in &self.carried {
            fresh.extend(at..run.current.start);
            at = run.current.end;
        }
        fresh.extend(at..self.spans.len());

        let starts: Vec<usize> = fresh.iter().map(|&ix| self.spans[ix].0.start).collect();
        let ends: Vec<usize> = fresh.iter().map(|&ix| self.spans[ix].0.end).collect();
        let mut minted = snapshot
            .anchors_at_batch(&starts, Bias::Right)
            .into_iter()
            .zip(snapshot.anchors_at_batch(&ends, Bias::Left));

        // A minted stretch is built here. A carried one is taken from the prior
        // channel's segments, whole ones by refcount. Both arrive as runs, so
        // the new channel is segmented along the splice rather than rebuilt.
        let mut runs: Vec<TokenRun> = Vec::new();
        let mut mint_run = |runs: &mut Vec<TokenRun>, range: Range<usize>| {
            if range.is_empty() {
                return;
            }
            let tokens: Arc<[SemanticTokenHighlight]> = range
                .clone()
                .map(|ix| SemanticTokenHighlight {
                    range: {
                        let (start, end) = minted.next().expect("one mint per fresh token");
                        start..end
                    },
                    style: self.spans[ix].1,
                })
                .collect();
            runs.push(TokenRun {
                tokens,
                ends: self.spans[range].iter().map(|(s, _)| s.end).collect(),
            });
        };

        let mut at = 0;
        for run in &self.carried {
            mint_run(&mut runs, at..run.current.start);
            // The prior segmentation decides how many chunks the stretch
            // arrives in, so walk the current side alongside it.
            let mut cursor = run.current.start;
            for tokens in prior.carve(run.prior.clone()) {
                let upto = cursor + tokens.len();
                runs.push(TokenRun {
                    tokens,
                    ends: self.spans[cursor..upto]
                        .iter()
                        .map(|(s, _)| s.end)
                        .collect(),
                });
                cursor = upto;
            }
            at = run.current.end;
        }
        mint_run(&mut runs, at..self.spans.len());

        BufferSemanticTokens::from_runs(runs, interner)
    }
}

/// The styled byte spans a merged capture list resolves to under `styles`.
///
/// A capture reaches a theme key through its originating layer's
/// `highlight_map()`, and a DEFAULT id means the active theme has no entry for
/// it, so it carries no style and is dropped along with empty ranges. The
/// captures' document order (start, Reverse(end), depth) is kept, so deeper
/// injection layers land later and win under the display map's endpoint
/// precedence.
fn styled_capture_spans(
    captures: Vec<SyntaxMapCapture<'_>>,
    styles: &SyntaxStyles,
) -> Vec<(Range<usize>, HighlightStyleId)> {
    // highlight_map() clones a locked map, so memoize it per layer language. A
    // parse reaches two or three languages, and scanning that many pointers
    // costs less than hashing a key for every capture in the file.
    let mut highlight_maps: Vec<(*const Language, _)> = Vec::new();
    captures
        .into_iter()
        .filter_map(|cap| {
            let range = cap.node.byte_range();
            if range.start == range.end {
                return None;
            }
            let key = cap.language as *const Language;
            let ix = match highlight_maps.iter().position(|(lang, _)| *lang == key) {
                Some(ix) => ix,
                None => {
                    highlight_maps.push((key, cap.language.highlight_map()));
                    highlight_maps.len() - 1
                },
            };
            let style_id = styles.id_for_highlight(highlight_maps[ix].1.get(cap.index))?;
            Some((range, style_id))
        })
        .collect()
}

/// Anchor a whole span list with two cursor walks rather than a root descent
/// per endpoint.
///
/// The two ends take opposite biases, so an insertion at a token's start
/// attaches to the previous span and one at its end attaches to the next. That
/// keeps a typed character from silently extending a keyword or string into
/// neighboring text.
fn anchor_spans(
    styled: &[(Range<usize>, HighlightStyleId)],
    ends: &[usize],
    snapshot: &TextBufferSnapshot,
) -> Arc<[SemanticTokenHighlight]> {
    let starts: Vec<usize> = styled.iter().map(|(range, _)| range.start).collect();
    styled
        .iter()
        .map(|(_, style)| *style)
        .zip(snapshot.anchors_at_batch(&starts, Bias::Right))
        .zip(snapshot.anchors_at_batch(ends, Bias::Left))
        .map(|((style, start), end)| SemanticTokenHighlight {
            range: start..end,
            style,
        })
        .collect()
}

/// How many rows either side of a viewport the first capture walk covers.
///
/// A scroll of less than this lands on tokens that are already there, which
/// keeps a nudge of the wheel from showing unstyled text before the whole-file
/// pass arrives.
const CAPTURE_MARGIN_ROWS: u32 = 200;

/// The byte range covering `rows` widened by [`CAPTURE_MARGIN_ROWS`].
///
/// `rows` are display rows, so wraps and folds make them an approximation of
/// the buffer rows on screen. That is sound here only because a narrowed walk
/// is always followed by a whole-file one, which makes an under-shoot a beat of
/// unstyled text rather than text that stays wrong.
fn capture_window(rows: Range<u32>, rope: &Rope) -> Range<usize> {
    let last = rope.max_point().row;
    let first_row = rows.start.saturating_sub(CAPTURE_MARGIN_ROWS);
    let last_row = rows
        .end
        .saturating_add(CAPTURE_MARGIN_ROWS)
        .min(last)
        .max(first_row);

    let start = rope.point_to_offset(stoat_text::Point::new(first_row, 0));
    let end = rope.point_to_offset(stoat_text::Point::new(last_row, rope.line_len(last_row)));
    start..end
}

/// The byte ranges of `new_tree` an edit could have restyled.
///
/// Two sources, both in `new_tree`'s coordinates. The edits name the text that
/// actually changed, and `old_tree.changed_ranges(new_tree)` names the regions
/// whose syntax differs, which is what catches a restyle far from the caret
/// such as an unclosed quote swallowing the rest of a file.
///
/// Two consumers narrow themselves to this. The token recapture re-queries it
/// and carries the prior tokens everywhere else, and the injection re-walk
/// treats it as the region it is responsible for re-finding layers in, keeping
/// the layers outside it. Both are only sound if a restyle cannot land outside
/// the union, which is why the second source is not optional.
///
/// Every range is widened by a byte on each side, which is load-bearing rather
/// than slack. A `Bias::Left` end anchor at an insertion point resolves to the
/// insertion point while [`Patch::old_to_new`] carries the same offset past the
/// inserted text, so a token abutting an edit has to be re-queried rather than
/// carried with an anchor that disagrees with its span. The widening is what
/// puts it strictly inside a range.
fn invalidated_ranges(
    old_tree: &language::Tree,
    new_tree: &language::Tree,
    edits: &Patch<usize>,
    len: usize,
) -> Vec<Range<usize>> {
    let widen = |start: usize, end: usize| {
        start.saturating_sub(1)..end.saturating_add(1).min(len).max(start)
    };
    let mut ranges: Vec<Range<usize>> = edits
        .edits()
        .iter()
        .map(|edit| widen(edit.new.start, edit.new.end))
        .chain(
            old_tree
                .changed_ranges(new_tree)
                .map(|r| widen(r.start_byte.min(len), r.end_byte.min(len))),
        )
        .collect();
    ranges.sort_unstable_by_key(|r| r.start);
    merge_ranges(&mut ranges);
    ranges
}

/// Collapse a start-sorted range list in place so no two entries overlap or
/// touch.
///
/// Touching entries merge because the tokens crossing a shared boundary are the
/// same set either way, and one wider query beats two adjacent ones.
fn merge_ranges(ranges: &mut Vec<Range<usize>>) {
    let mut write = 0;
    for read in 0..ranges.len() {
        if write > 0 && ranges[read].start <= ranges[write - 1].end {
            ranges[write - 1].end = ranges[write - 1].end.max(ranges[read].end);
            continue;
        }
        ranges[write] = ranges[read].clone();
        write += 1;
    }
    ranges.truncate(write);
}

/// Rebuild a parse's tokens by re-querying `invalidated` and carrying `prior`
/// across `edits` everywhere else.
///
/// `prior` is the previous parse's spans in its own byte coordinates, which
/// `edits` carries into `rope`'s. Returns `None` when the re-query never
/// settles, or when the ranges it would query grow past half the rope, leaving
/// the caller the full walk it would otherwise have done.
///
/// A query range is grown until every capture it returns lies inside it, since
/// tree-sitter answers with each capture strictly intersecting the range and a
/// capture may extend well past it. That fixpoint is what makes the splice
/// exact. Once a range equals its cover, the captures it returns are precisely
/// the tokens strictly intersecting that cover, which is precisely the set of
/// carried tokens the cover replaces.
fn recapture_edited_ranges(
    snapshot: &SyntaxSnapshot,
    rope: &Rope,
    prior: &[(Range<usize>, HighlightStyleId)],
    edits: &Patch<usize>,
    invalidated: Vec<Range<usize>>,
    styles: &SyntaxStyles,
) -> Option<Recaptured> {
    let mut covers = invalidated;
    let mut fresh: Vec<CoverSpans> = covers.iter().map(|_| None).collect();

    for _ in 0..RECAPTURE_ROUNDS {
        // Past half the file the narrowing has stopped paying for itself, and
        // the caller's walk answers the same question in one query. Tested
        // every round rather than only on the way in, since a cover that keeps
        // growing gets there by widening rather than by arriving wide.
        if covers.iter().map(|c| c.end - c.start).sum::<usize>() * 2 > rope.len() {
            return None;
        }

        for (cover, spans) in covers.iter().zip(fresh.iter_mut()) {
            if spans.is_none() {
                *spans = Some(styled_capture_spans(
                    snapshot.captures(cover.clone(), rope, |l| Some(&l.highlight_query)),
                    styles,
                ));
            }
        }

        let mut grown: Vec<Range<usize>> = covers
            .iter()
            .zip(&fresh)
            .map(|(cover, spans)| {
                spans
                    .as_ref()
                    .expect("every cover was queried above")
                    .iter()
                    .fold(cover.clone(), |acc, (span, _)| {
                        acc.start.min(span.start)..acc.end.max(span.end)
                    })
            })
            .collect();
        merge_ranges(&mut grown);

        if grown == covers {
            let fresh = fresh
                .into_iter()
                .map(|spans| spans.expect("every cover was queried above"))
                .collect();
            return Some(splice_recaptured(prior, edits, &covers, fresh));
        }

        fresh = carry_settled_covers(&covers, &grown, fresh);
        covers = grown;
    }
    None
}

/// Move each cover's captures onto the next round's cover list, keeping only
/// those whose range came through untouched.
///
/// A cover that neither grew nor absorbed a neighbour asks the same question of
/// the same trees over the same text, so its answer stands and the next round
/// skips the query. Anything else is a different range and starts empty.
///
/// Both lists are start-ordered and disjoint, so one walk pairs them.
fn carry_settled_covers(
    covers: &[Range<usize>],
    grown: &[Range<usize>],
    mut fresh: Vec<CoverSpans>,
) -> Vec<CoverSpans> {
    let mut ix = 0;
    grown
        .iter()
        .map(|range| {
            while covers
                .get(ix)
                .is_some_and(|cover| cover.start < range.start)
            {
                ix += 1;
            }
            match covers.get(ix) {
                Some(cover) if cover == range => fresh[ix].take(),
                _ => None,
            }
        })
        .collect()
}

/// Interleave the carried and re-queried tokens into one document-ordered list.
///
/// `fresh[i]` holds the tokens `covers[i]` returned, and a carried token that
/// strictly intersects any cover is dropped because the fresh list already
/// holds its replacement. What survives never intersects a cover, so flushing
/// each cover's tokens once the walk passes it produces a list already sorted
/// by `(start, Reverse(end))` with no re-sort.
fn splice_recaptured(
    prior: &[(Range<usize>, HighlightStyleId)],
    edits: &Patch<usize>,
    covers: &[Range<usize>],
    fresh: Vec<Vec<(Range<usize>, HighlightStyleId)>>,
) -> Recaptured {
    let total = prior.len() + fresh.iter().map(Vec::len).sum::<usize>();
    let mut spans = Vec::with_capacity(total);
    let mut carried: Vec<CarriedRun> = Vec::new();
    let mut replaced = Vec::new();
    let mut next_cover = 0;

    // Extend the open stretch when this token continues it, and start a new one
    // otherwise. Consecutive prior indices landing at consecutive current ones
    // is the ordinary case, so most of a file becomes one stretch.
    let extend = |carried: &mut Vec<CarriedRun>, prior_ix: usize, current_ix: usize| match carried
        .last_mut()
    {
        Some(run) if run.prior.end == prior_ix && run.current.end == current_ix => {
            run.prior.end += 1;
            run.current.end += 1;
        },
        _ => carried.push(CarriedRun {
            prior: prior_ix..prior_ix + 1,
            current: current_ix..current_ix + 1,
        }),
    };

    for (ix, (span, style)) in prior.iter().enumerate() {
        let moved = edits.old_to_new(span.start)..edits.old_to_new(span.end);

        // A span the edit replaced outright collapses onto a point. The full
        // walk emits no empty token, so neither may this path.
        if moved.start >= moved.end {
            replaced.push((span.clone(), *style));
            continue;
        }

        while next_cover < covers.len() && covers[next_cover].end <= moved.start {
            spans.extend_from_slice(&fresh[next_cover]);
            next_cover += 1;
        }

        // Covers are sorted and disjoint and the loop above passed every one
        // ending at or before this token, so only the next cover can still
        // intersect it.
        let intersects_cover = covers
            .get(next_cover)
            .is_some_and(|cover| cover.start < moved.end);
        if intersects_cover {
            replaced.push((span.clone(), *style));
            continue;
        }

        extend(&mut carried, ix, spans.len());
        spans.push((moved, *style));
    }

    for cover in &fresh[next_cover..] {
        spans.extend_from_slice(cover);
    }

    Recaptured {
        spans,
        carried,
        replaced,
        fresh: fresh.concat(),
    }
}

/// The buffer rows whose tokens differ between the parse that produced `prior`
/// and the one that produced `current`.
///
/// `prior` is in the previous parse's byte coordinates, so `edits` carries it
/// into `rope`'s before anything is compared. Both lists arrive in document
/// order, so trimming the matching head and tail leaves only what moved, and
/// the answer is the row span covering both sides of that difference.
///
/// The rows `edits` itself touched are always included. Carrying an offset
/// through a patch and resolving an anchor disagree where an insertion lands
/// exactly on a token's end, which could otherwise make a genuinely-changed
/// token compare equal. Those rows are re-summarized on the buffer-edit path
/// regardless, so covering them again costs a comparison and changes nothing.
fn changed_token_rows(
    prior: &[(Range<usize>, HighlightStyleId)],
    current: &[(Range<usize>, HighlightStyleId)],
    edits: &Patch<usize>,
    rope: &Rope,
) -> Range<u32> {
    let carried = |span: &Range<usize>| edits.old_to_new(span.start)..edits.old_to_new(span.end);
    let same = |a: &(Range<usize>, HighlightStyleId), b: &(Range<usize>, HighlightStyleId)| {
        carried(&a.0) == b.0 && a.1 == b.1
    };

    let head = prior
        .iter()
        .zip(current)
        .take_while(|(a, b)| same(a, b))
        .count();
    let tail = prior[head..]
        .iter()
        .rev()
        .zip(current[head.min(current.len())..].iter().rev())
        .take_while(|(a, b)| same(a, b))
        .count();

    let mut span: Option<Range<usize>> = None;
    let mut cover = |range: Range<usize>| {
        span = Some(match span.take() {
            Some(have) => have.start.min(range.start)..have.end.max(range.end),
            None => range,
        });
    };
    for (range, _) in &prior[head..prior.len() - tail] {
        cover(carried(range));
    }
    for (range, _) in &current[head.min(current.len())..current.len() - tail] {
        cover(range.clone());
    }
    for edit in edits.edits() {
        cover(edit.new.clone());
    }

    match span {
        Some(span) => {
            // Spans are half-open, so the last row they occupy holds `end - 1`.
            // Converting `end` itself would pull in the following row every
            // time a span stops at a line break.
            let last = span.end.max(span.start + 1) - 1;
            let start = rope.offset_to_point(span.start.min(rope.len())).row;
            let end = rope.offset_to_point(last.min(rope.len())).row;
            start..end + 1
        },
        None => 0..0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::install_highlight_maps, buffer::TextBuffer};
    use std::path::Path;
    use stoat_language::LanguageRegistry;
    use stoat_text::Anchor;

    /// A named viewport narrows the fallback walk to it, and the same buffer
    /// with none named still walks the whole file. The narrowing has to be
    /// visible in the tokens, not just in what the parse reports.
    #[test]
    fn a_named_viewport_narrows_the_fallback_capture() {
        let (styles, lang) = carried_parse_fixture("a.rs");
        let buffer_id = BufferId::new(1);

        // Long enough that the margin cannot reach the end.
        let rows = 4_000;
        let text = "fn a() {}\n".repeat(rows);
        let buf = TextBuffer::with_text(buffer_id, &text);

        let parse = |viewport: Option<Range<u32>>| {
            let mut prior = None;
            let mut prior_map = None;
            parse_buffer_step(
                buffer_id,
                buf.snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                None,
                None,
                &styles,
                None,
                viewport,
            )
            .expect("parse should succeed")
        };

        let whole = parse(None);
        assert_eq!(whole.captured, None, "no viewport walks the whole file");

        let narrowed = parse(Some(0..40));
        let window = narrowed
            .captured
            .clone()
            .expect("a named viewport is reported as a partial capture");
        assert!(
            window.end < buf.snapshot.visible_text.len(),
            "the window must stop short of the file, got {window:?}",
        );

        assert!(
            narrowed.token_spans.len() < whole.token_spans.len(),
            "narrowing must produce fewer tokens: {} vs {}",
            narrowed.token_spans.len(),
            whole.token_spans.len(),
        );
        assert!(
            !narrowed.token_spans.is_empty(),
            "the viewport itself must still be styled",
        );
        assert!(
            narrowed
                .token_spans
                .iter()
                .all(|(r, _)| r.start < window.end),
            "no token may fall outside the captured window",
        );
        assert!(
            whole.token_spans.iter().any(|(r, _)| r.start >= window.end),
            "the whole-file walk must reach past the window",
        );
    }

    /// When `parse_buffer_step` aborts on the deadline, the prior state
    /// passed via `&mut Option<_>` must remain populated so the caller
    /// can hand it to a follow-up parse without losing incrementality.
    #[test]
    fn parse_buffer_step_preserves_prior_on_deadline_abort() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let executor = scheduler.executor();
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .unwrap();
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let buffer_id = BufferId::new(1);

        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        let mut prior: Option<SyntaxState> = None;
        let mut prior_map: Option<stoat_language::SyntaxMap> = None;
        let out = parse_buffer_step(
            buffer_id,
            snap1,
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            None,
            None,
        )
        .expect("first parse should succeed");
        let initial_version = out.syntax.version;

        let mut prior: Option<SyntaxState> = Some(out.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(out.syntax_map);
        buf.edit(0..0, "// edit\n");
        let snap2 = buf.snapshot.clone();

        let deadline = executor.now();
        let result = parse_buffer_step(
            buffer_id,
            snap2.clone(),
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            Some((deadline, &executor)),
            None,
        );
        assert!(result.is_none(), "expected deadline abort to return None");
        let prior_state = prior
            .as_ref()
            .expect("prior must survive deadline abort, was consumed");
        assert_eq!(
            prior_state.version, initial_version,
            "prior version must be unchanged",
        );
        assert!(
            prior_map.is_some(),
            "prior_syntax_map must survive deadline abort",
        );

        let recovery = parse_buffer_step(
            buffer_id,
            snap2,
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            None,
            None,
        )
        .expect("recovery parse should succeed");
        assert!(recovery.syntax.version > initial_version);
        assert!(prior.is_none(), "successful parse must consume the prior");
        assert!(prior_map.is_none());
    }

    /// The deadline check inside `parse_rope_within` reads time from the
    /// `Executor`, not the wall clock, so `TestScheduler::advance_clock`
    /// drives the timeout deterministically.
    #[test]
    fn parse_buffer_step_deadline_uses_executor_clock() {
        use std::time::Duration;
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let executor = scheduler.executor();
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .unwrap();
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let buffer_id = BufferId::new(1);

        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        let mut prior: Option<SyntaxState> = None;
        let mut prior_map: Option<stoat_language::SyntaxMap> = None;
        let out = parse_buffer_step(
            buffer_id,
            snap1,
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            None,
            None,
        )
        .expect("first parse should succeed");

        let mut prior: Option<SyntaxState> = Some(out.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(out.syntax_map);
        buf.edit(0..0, "// edit\n");
        let snap2 = buf.snapshot.clone();

        let deadline = executor.now() + Duration::from_secs(3600);
        let succeeded = parse_buffer_step(
            buffer_id,
            snap2,
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            Some((deadline, &executor)),
            None,
        )
        .expect("deadline far in the future should not abort");

        let mut prior: Option<SyntaxState> = Some(succeeded.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(succeeded.syntax_map);
        buf.edit(0..0, "// edit2\n");
        let snap3 = buf.snapshot.clone();

        scheduler.advance_clock(Duration::from_secs(7200));

        let aborted = parse_buffer_step(
            buffer_id,
            snap3,
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            Some((deadline, &executor)),
            None,
        );
        assert!(
            aborted.is_none(),
            "after advance_clock past the deadline, parse must abort",
        );
    }

    /// Parse `path`'s `original`, apply one edit, then reparse both ways:
    /// once carrying the first parse's state forward, once from nothing.
    /// Returns `(incremental, from_scratch)` token lists as resolved
    /// `(start, end, style)` triples against the post-edit snapshot.
    #[allow(clippy::type_complexity)]
    fn tokens_incremental_vs_fresh(
        path: &str,
        original: &str,
        edit_at: usize,
        inserted: &str,
    ) -> (
        Vec<(usize, usize, HighlightStyleId)>,
        Vec<(usize, usize, HighlightStyleId)>,
    ) {
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let registry = LanguageRegistry::standard();
        // Without this every capture resolves to HighlightId::DEFAULT and both
        // token lists come back empty, comparing equal for the wrong reason.
        install_highlight_maps(&registry, &styles);
        let lang = registry.for_path(Path::new(path)).unwrap();
        let buffer_id = BufferId::new(1);

        let mut buf = TextBuffer::with_text(buffer_id, original);
        let first = {
            let mut prior = None;
            let mut prior_map = None;
            parse_buffer_step(
                buffer_id,
                buf.snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("first parse should succeed")
        };

        buf.edit(edit_at..edit_at, inserted);
        let edited = buf.snapshot.clone();

        let incremental = {
            let mut prior = Some(first.syntax);
            let mut prior_map = Some(first.syntax_map);
            parse_buffer_step(
                buffer_id,
                edited.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("incremental parse should succeed")
        };
        let fresh = {
            let mut prior = None;
            let mut prior_map = None;
            parse_buffer_step(
                buffer_id,
                edited.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("from-scratch parse should succeed")
        };

        let resolve = |out: &ParseJobOutput| {
            out.token_channel
                .iter()
                .map(|t| {
                    (
                        edited.resolve_anchor(&t.range.start),
                        edited.resolve_anchor(&t.range.end),
                        t.style,
                    )
                })
                .collect()
        };
        (resolve(&incremental), resolve(&fresh))
    }

    /// Reusing the prior [`stoat_language::SyntaxMap`] is only sound if it
    /// produces the highlights a full parse would. The incremental path
    /// re-walks just the edited rows for injection changes, so a divergence
    /// here means highlights outside that window were lost or went stale.
    #[test]
    fn incremental_parse_tokens_match_a_fresh_parse() {
        let original = "fn a() -> u32 {\n    let x = 1;\n    x\n}\n";
        let (incremental, fresh) =
            tokens_incremental_vs_fresh("a.rs", original, original.len(), "// tail\n");
        assert_eq!(
            incremental, fresh,
            "incremental tokens must equal a from-scratch parse",
        );
        assert!(!fresh.is_empty(), "fixture must produce tokens at all");
    }

    /// The injection layers are what the old drop-and-rebuild protected.
    /// A filtered reparse never re-walks the fence, so its rust highlights
    /// survive only if the layer is carried across the edit.
    #[test]
    fn incremental_parse_keeps_fenced_injection_tokens() {
        let original = "# Title\n\n```rust\nfn a() -> u32 { 1 }\n```\n\ntail\n";
        let (incremental, fresh) =
            tokens_incremental_vs_fresh("a.md", original, original.len(), "more tail\n");
        assert_eq!(
            incremental, fresh,
            "incremental tokens must equal a from-scratch parse across an injection",
        );

        let fence_body = original.find("fn a()").unwrap();
        assert!(
            fresh
                .iter()
                .any(|&(start, _, _)| start >= fence_body && start < fence_body + 20),
            "fixture must produce tokens inside the rust fence, got {fresh:?}",
        );
    }

    /// The state one parse hands the next so it can carry tokens across an
    /// edit rather than re-querying the file.
    struct CarriedParse {
        buffer_id: BufferId,
        syntax: Option<SyntaxState>,
        map: Option<stoat_language::SyntaxMap>,
        spans: Option<SemanticTokenSpans>,
        anchors: Option<BufferSemanticTokens>,
    }

    impl CarriedParse {
        fn new(buffer_id: BufferId) -> Self {
            Self {
                buffer_id,
                syntax: None,
                map: None,
                spans: None,
                anchors: None,
            }
        }
    }

    /// Parse `snapshot` twice, once carrying `state` forward and once from
    /// nothing, and assert the two agree on both the styled byte spans and the
    /// offsets every anchor resolves to. Leaves `state` holding the carried
    /// parse, ready for the next edit. Returns its layer count, which is the
    /// guard that decides whether the carry could narrow its query at all.
    fn assert_carried_parse_matches_fresh(
        state: &mut CarriedParse,
        snapshot: &TextBufferSnapshot,
        lang: &Arc<Language>,
        styles: &SyntaxStyles,
        step: &str,
    ) -> usize {
        let buffer_id = state.buffer_id;
        let carried = parse_buffer_step(
            buffer_id,
            snapshot.clone(),
            lang,
            &mut state.syntax,
            &mut state.map,
            state.spans.as_deref(),
            state.anchors.as_ref(),
            styles,
            None,
            None,
        )
        .expect("carried parse should succeed");

        let fresh = {
            let mut syntax = None;
            let mut map = None;
            parse_buffer_step(
                buffer_id,
                snapshot.clone(),
                lang,
                &mut syntax,
                &mut map,
                None,
                None,
                styles,
                None,
                None,
            )
            .expect("from-scratch parse should succeed")
        };

        assert_eq!(
            carried.token_spans, fresh.token_spans,
            "{step}: carried spans must equal a from-scratch parse",
        );
        let resolved = |out: &ParseJobOutput| -> Vec<(usize, usize, HighlightStyleId)> {
            out.token_channel
                .iter()
                .map(|t| {
                    (
                        snapshot.resolve_anchor(&t.range.start),
                        snapshot.resolve_anchor(&t.range.end),
                        t.style,
                    )
                })
                .collect()
        };
        assert_eq!(
            resolved(&carried),
            resolved(&fresh),
            "{step}: carried anchors must resolve where a from-scratch parse anchored",
        );

        let layers = carried.syntax_map.snapshot().layer_count();
        state.syntax = Some(carried.syntax);
        state.map = Some(carried.syntax_map);
        state.spans = Some(carried.token_spans);
        state.anchors = Some(carried.token_channel);
        layers
    }

    /// The theme-installed styles and language every carried-parse test parses
    /// against. Without the highlight maps every capture resolves to
    /// `HighlightId::DEFAULT` and both token lists come back empty, comparing
    /// equal for the wrong reason.
    fn carried_parse_fixture(path: &str) -> (SyntaxStyles, Arc<Language>) {
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let registry = LanguageRegistry::standard();
        install_highlight_maps(&registry, &styles);
        let lang = registry.for_path(Path::new(path)).unwrap();
        (styles, lang)
    }

    /// Narrowing the capture walk to the edited ranges is only sound if the
    /// result is what walking the file would have produced. Randomized edits
    /// are the check, because the ways a narrowing goes wrong (a capture
    /// reaching past its query range, a token abutting an insertion, a token
    /// deleted outright) are all boundary cases no hand-picked edit hits by
    /// accident.
    #[test]
    fn a_carried_parse_tracks_a_fresh_parse_through_random_edits() {
        // Snippets chosen to move token boundaries rather than plain text:
        // quotes and comment markers restyle text far from where they land.
        let layers = random_edits_tracking_a_fresh_parse(
            "a.rs",
            "fn main() {\n    let name = \"hello\";\n    let n = 1 + 2;\n\
             \n    println!(\"{name} {n}\");\n}\n\nfn other(x: u32) -> u32 {\n    x * 2\n}\n",
            &[
                "\"", "//", "let ", "fn ", "*/", "/*", "x", " ", "\n", "'", "}", "{",
            ],
        );
        assert!(
            layers.iter().all(|&l| l == 1),
            "a rust buffer with no doc comment stays single-layer, got {layers:?}",
        );
    }

    /// The same randomized check over a buffer whose tokens come from several
    /// layers at once.
    ///
    /// A combined injection is one layer over several host ranges, so an edit
    /// that reaches it has to leave the layer covering every one of them. Edits
    /// that open, rename, and delete fences all move which ranges those are.
    #[test]
    fn a_carried_parse_tracks_a_fresh_parse_across_injection_layers() {
        // Fence markers and info strings are in the snippet set because an
        // edit that opens or renames a fence is what moves the layer set.
        let layers = random_edits_tracking_a_fresh_parse(
            "a.md",
            "# Title\n\nSome **bold** prose.\n\n```rust\nfn a() -> u32 { 1 }\n\
             let s = \"text\";\n```\n\nmore *prose* here\n\n```\nplain\n```\n",
            &[
                "```", "```rust", "\"", "*", "`", "#", "fn ", "x", " ", "\n", "-", "]",
            ],
        );
        assert!(
            layers.iter().any(|&l| l > 1),
            "the fixture must actually inject layers, got {layers:?}",
        );
    }

    /// The same randomized check over the one injection that merges several
    /// host ranges into a single tree.
    ///
    /// Rust doc comments are it. Every `///` line is its own host node and the
    /// markdown parsed over them is one document, so a snippet set that writes
    /// and deletes `///` markers moves the range set that tree was built over.
    /// That is the case where carrying the tree forward can disagree with a
    /// parse that never saw the old ranges.
    #[test]
    fn a_carried_parse_tracks_a_fresh_parse_across_doc_comments() {
        let layers = random_edits_tracking_a_fresh_parse(
            "a.rs",
            "/// Adds **two** numbers.\n\
             ///\n\
             /// Returns `a + b`, which is the *whole* contract.\n\
             fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\n\
             /// Doubles `x`.\n\
             fn double(x: u32) -> u32 {\n    x * 2\n}\n",
            &["///", "/// ", "**", "`", "*", "\n", "x", " ", "//", "-"],
        );
        assert!(
            layers.iter().any(|&l| l > 1),
            "the fixture must keep a doc-comment layer alive, got {layers:?}",
        );
    }

    /// Drive randomized edits over `source`, asserting after each one that the
    /// carried parse still equals a from-scratch parse of the same snapshot.
    ///
    /// Returns the layer count each step's parse produced, so a caller can pin
    /// that the fixture exercised the shape it was chosen for.
    fn random_edits_tracking_a_fresh_parse(
        path: &str,
        source: &str,
        snippets: &[&str],
    ) -> Vec<usize> {
        let (styles, lang) = carried_parse_fixture(path);
        let buffer_id = BufferId::new(1);
        let mut buf = TextBuffer::with_text(buffer_id, source);

        let mut state = CarriedParse::new(buffer_id);
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        (0..80)
            .map(|step| {
                let len = buf.snapshot.visible_text.len();
                let at = (next() as usize) % (len + 1);
                let at = buf.snapshot.visible_text.clip_offset(at, Bias::Left);
                if next() % 3 == 0 {
                    let end = buf
                        .snapshot
                        .visible_text
                        .clip_offset((at + 1 + (next() as usize) % 4).min(len), Bias::Right);
                    buf.edit(at..end, "");
                } else {
                    buf.edit(at..at, snippets[(next() as usize) % snippets.len()]);
                }

                assert_carried_parse_matches_fresh(
                    &mut state,
                    &buf.snapshot.clone(),
                    &lang,
                    &styles,
                    &format!("step {step}"),
                )
            })
            .collect()
    }

    /// An opened quote restyles every line after it until it closes, which no
    /// edit range names. Only the tree's own changed ranges reach that far, so
    /// this is the case a narrowing built on edit ranges alone gets wrong.
    #[test]
    fn a_carried_parse_follows_a_quote_opened_far_from_the_restyled_text() {
        let (styles, lang) = carried_parse_fixture("a.rs");
        let buffer_id = BufferId::new(1);
        let body = "fn a() { let x = 1; }\n".repeat(20);
        let mut buf = TextBuffer::with_text(buffer_id, &body);

        let mut state = CarriedParse::new(buffer_id);
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "first parse",
        );

        // A lone quote near the top swallows the rest of the file into one
        // string, restaining every row below an edit that touched one byte.
        buf.edit(9..9, "\"");
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "quote opened",
        );

        // Closing it hands all that text back to the code highlighting, the
        // same restyle in reverse.
        buf.edit(30..30, "\"");
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "quote closed",
        );
    }

    /// A capture can reach far outside the range that returned it, and the
    /// tokens nested inside it between there and the edit were never queried.
    /// Widening the query to what came back and asking again is what keeps
    /// them, so this is the case a single-pass narrowing silently drops.
    #[test]
    fn a_carried_parse_keeps_tokens_nested_inside_a_capture_the_edit_reached() {
        let (styles, lang) = carried_parse_fixture("a.rs");
        let buffer_id = BufferId::new(1);
        // Escape sequences are captured inside the string literal enclosing
        // them, so an edit at the string's tail pulls the whole literal into
        // the query's answer while leaving every escape before it unqueried.
        let source = "fn a() {\n    let s = \"\\n\\t\\n\\t\\n\\t padding tail\";\n}\n";
        let mut buf = TextBuffer::with_text(buffer_id, source);

        let mut state = CarriedParse::new(buffer_id);
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "first parse",
        );

        let at = source.find("tail").expect("fixture has a tail");
        buf.edit(at..at, "x");
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "edit inside the string's tail",
        );
    }

    /// An injection layer can restyle its whole range without the host tree
    /// changing at all, so the narrowing has nothing to key on and the parse
    /// has to walk the file. The fallback is what keeps a fenced buffer
    /// correct, and it must stay correct across edits like any other.
    #[test]
    fn a_multi_layer_buffer_still_matches_a_fresh_parse() {
        let (styles, lang) = carried_parse_fixture("a.md");
        let buffer_id = BufferId::new(1);
        let source = "# Title\n\n```rust\nfn a() -> u32 { 1 }\n```\n\ntail text\n";
        let mut buf = TextBuffer::with_text(buffer_id, source);

        let mut state = CarriedParse::new(buffer_id);
        let layers = assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "first parse",
        );
        assert!(
            layers > 1,
            "fixture must inject a layer so the fallback is what runs, got {layers}",
        );

        let tail = source.find("tail").expect("fixture has a tail");
        buf.edit(tail..tail, "more ");
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "edit outside the fence",
        );
    }

    /// An edit restyles text nowhere near itself when it opens a construct that
    /// runs to end of file. The injection walk only re-queries where it is told
    /// to look, and keeps every layer it could not have re-found, so a filter
    /// built from the edit span alone carries the invalidated layer forward
    /// stale.
    #[test]
    fn an_edit_that_swallows_a_distant_fence_drops_its_layer() {
        let (styles, lang) = carried_parse_fixture("a.md");
        let buffer_id = BufferId::new(1);
        let source = "# Title\n\nprose\n\n```rust\nfn a() -> u32 { 1 }\n```\n\ntail text\n";
        let mut buf = TextBuffer::with_text(buffer_id, source);

        let mut state = CarriedParse::new(buffer_id);
        let layers = assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "first parse",
        );
        assert!(
            layers > 1,
            "fixture must start with the rust fence injected, got {layers} layers",
        );

        // A bare fence above the rust one, rows away from it. Everything below
        // becomes one unnamed fenced block, so the rust layer has to go.
        let prose = source.find("prose").expect("fixture has prose");
        buf.edit(prose..prose, "```\n");
        assert_carried_parse_matches_fresh(
            &mut state,
            &buf.snapshot.clone(),
            &lang,
            &styles,
            "an opening fence above swallows the rust fence",
        );
    }

    /// The narrowing has to actually engage, not just agree with the full walk
    /// by falling back to it. This drives it directly, so a guard that quietly
    /// stopped admitting ordinary edits would show up here rather than as a
    /// silent return to per-keystroke full walks.
    #[test]
    fn a_local_edit_recaptures_rather_than_walking_the_file() {
        let (styles, lang) = carried_parse_fixture("a.rs");
        let buffer_id = BufferId::new(1);
        let body = "fn a() { let x = 1; }\n".repeat(20);
        let mut buf = TextBuffer::with_text(buffer_id, &body);

        let first = {
            let mut syntax = None;
            let mut map = None;
            parse_buffer_step(
                buffer_id,
                buf.snapshot.clone(),
                &lang,
                &mut syntax,
                &mut map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("first parse should succeed")
        };

        let at = body.find("let x").expect("fixture has a binding");
        buf.edit(at..at, "mut ");
        let snapshot = buf.snapshot.clone();
        let rope = snapshot.visible_text.clone();
        let edits = snapshot.edits_since(first.syntax.version);

        let mut edited_tree = first.syntax.tree.clone();
        language::edit_tree(
            &mut edited_tree,
            edits.edits(),
            &first.syntax.rope_snapshot,
            &rope,
        );
        let mut map = first.syntax_map.clone();
        map.interpolate(edits.edits(), &first.syntax.rope_snapshot, &rope);
        map.reparse(&rope, lang.clone(), snapshot.version, None, None)
            .expect("reparse should succeed");
        let tree = map
            .snapshot()
            .iter_layers()
            .next()
            .expect("a root layer")
            .tree
            .clone();

        let invalidated = invalidated_ranges(&edited_tree, &tree, &edits, rope.len());
        assert!(
            invalidated.iter().map(|r| r.end - r.start).sum::<usize>() < rope.len() / 2,
            "an edit to one line must invalidate a fraction of the file, got {invalidated:?}",
        );

        let recaptured = recapture_edited_ranges(
            map.snapshot(),
            &rope,
            &first.token_spans,
            &edits,
            invalidated,
            &styles,
        )
        .expect("a single-layer local edit must recapture");
        assert!(
            !recaptured.carried.is_empty(),
            "a local edit must carry tokens rather than re-query them all",
        );
        assert_eq!(
            recaptured.spans,
            styled_capture_spans(
                map.snapshot()
                    .captures(0..rope.len(), &rope, |l| Some(&l.highlight_query)),
                &styles,
            ),
            "recaptured spans must equal the full capture walk",
        );
    }

    /// A cover keeps its captures only when the next round asks the same
    /// question, which means the same bounds rather than merely overlapping
    /// ones.
    #[test]
    fn only_an_unchanged_cover_keeps_its_captures() {
        use crate::display_map::highlights::{HighlightStyle, HighlightStyleInterner};
        let style = HighlightStyleInterner::default().intern(HighlightStyle::default());
        let spans = |tag: usize| Some(vec![(tag..tag + 1, style)]);

        let covers = vec![0..10, 20..30, 40..50, 60..70];
        // The second grew, the third and fourth merged into one range covering
        // both, and only the first came through as it was.
        let grown = vec![0..10, 15..35, 40..70];
        assert_eq!(
            carry_settled_covers(
                &covers,
                &grown,
                vec![spans(1), spans(2), spans(3), spans(4)],
            ),
            vec![spans(1), None, None],
            "a grown or merged range is a different question and starts empty",
        );
    }

    /// Once the ranges to re-query reach half the file, the walk the caller
    /// would otherwise do answers the same question in one query, so the
    /// narrowing declines rather than spending its rounds widening toward it.
    #[test]
    fn a_wide_invalidation_falls_through_to_the_full_walk() {
        let (styles, lang) = carried_parse_fixture("a.rs");
        let buffer_id = BufferId::new(1);
        let body = "fn a() { let x = 1; }\n".repeat(20);
        let mut buf = TextBuffer::with_text(buffer_id, &body);

        let first = {
            let mut syntax = None;
            let mut map = None;
            parse_buffer_step(
                buffer_id,
                buf.snapshot.clone(),
                &lang,
                &mut syntax,
                &mut map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("first parse should succeed")
        };

        let at = body.find("let x").expect("fixture has a binding");
        buf.edit(at..at, "mut ");
        let snapshot = buf.snapshot.clone();
        let rope = snapshot.visible_text.clone();
        let edits = snapshot.edits_since(first.syntax.version);

        let mut map = first.syntax_map.clone();
        map.interpolate(edits.edits(), &first.syntax.rope_snapshot, &rope);
        map.reparse(&rope, lang.clone(), snapshot.version, None, None)
            .expect("reparse should succeed");

        let recapture = |cover: Range<usize>| {
            recapture_edited_ranges(
                map.snapshot(),
                &rope,
                &first.token_spans,
                &edits,
                vec![cover],
                &styles,
            )
        };
        // Whole lines, so the narrow cover has no capture straddling its end to
        // grow it back over the threshold.
        let half = rope.len() / 2;
        let under = rope.point_to_offset(stoat_text::Point::new(rope.offset_to_point(half).row, 0));
        assert!(
            recapture(0..under).is_some(),
            "a cover under half the rope must still recapture, got none at {under}",
        );
        assert!(
            recapture(0..half + 1).is_none(),
            "a cover past half the rope must leave the walk to the caller",
        );
    }

    /// Parse `original`, apply one edit, parse again carrying the first
    /// parse's spans forward, and return the second parse's changed rows.
    fn changed_rows_after_edit(
        path: &str,
        original: &str,
        edit_at: usize,
        inserted: &str,
    ) -> Option<Range<u32>> {
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let registry = LanguageRegistry::standard();
        install_highlight_maps(&registry, &styles);
        let lang = registry.for_path(Path::new(path)).unwrap();
        let buffer_id = BufferId::new(1);

        let mut buf = TextBuffer::with_text(buffer_id, original);
        let first = {
            let mut prior = None;
            let mut prior_map = None;
            parse_buffer_step(
                buffer_id,
                buf.snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("first parse should succeed")
        };

        buf.edit(edit_at..edit_at, inserted);
        let mut prior = Some(first.syntax);
        let mut prior_map = Some(first.syntax_map);
        parse_buffer_step(
            buffer_id,
            buf.snapshot.clone(),
            &lang,
            &mut prior,
            &mut prior_map,
            Some(&first.token_spans),
            Some(&first.token_channel),
            &styles,
            None,
            None,
        )
        .expect("second parse should succeed")
        .changed_token_rows
    }

    /// The minimap recolors the rows a parse reports, so a keystroke that
    /// restains one line must not name the rest of the file.
    #[test]
    fn a_local_edit_reports_only_its_own_rows() {
        let original = "fn a() {}\n".repeat(40);
        let row_20 = original
            .match_indices('\n')
            .nth(19)
            .map(|(i, _)| i + 1)
            .unwrap();

        let rows = changed_rows_after_edit("a.rs", &original, row_20, "let x = 1;\n")
            .expect("a second parse with prior spans reports its rows");
        assert_eq!(
            rows,
            20..21,
            "an inserted line restains itself and nothing above or below",
        );
    }

    /// An edit leaving every token identical still covers the row it touched,
    /// because carrying a span through the patch and resolving an anchor
    /// disagree exactly there. It must not widen past that row.
    #[test]
    fn an_edit_leaving_tokens_alone_reports_only_the_edited_row() {
        let original = "fn a() {}\n".repeat(40);
        let row_20 = original
            .match_indices('\n')
            .nth(19)
            .map(|(i, _)| i + 1)
            .unwrap();

        let rows = changed_rows_after_edit("a.rs", &original, row_20, "\n")
            .expect("a second parse with prior spans reports its rows");
        assert_eq!(
            rows,
            20..21,
            "a blank line adds no tokens, so only the row it split is suspect",
        );
    }

    /// Without a prior parse to diff against there is no row information, and
    /// the strip has to fall back to recoloring everything.
    #[test]
    fn a_first_parse_reports_no_rows_at_all() {
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let registry = LanguageRegistry::standard();
        install_highlight_maps(&registry, &styles);
        let lang = registry.for_path(Path::new("a.rs")).unwrap();
        let buffer_id = BufferId::new(1);

        let buf = TextBuffer::with_text(buffer_id, "fn a() {}\n");
        let mut prior = None;
        let mut prior_map = None;
        let out = parse_buffer_step(
            buffer_id,
            buf.snapshot.clone(),
            &lang,
            &mut prior,
            &mut prior_map,
            None,
            None,
            &styles,
            None,
            None,
        )
        .expect("parse should succeed");

        assert_eq!(
            out.changed_token_rows, None,
            "a first parse cannot bound what it changed",
        );
    }

    /// The parse builds its token anchors and search index in batch, from byte
    /// offsets rather than per-anchor tree seeks. The result has to be the same
    /// channel the per-anchor construction produces, or viewport queries would
    /// bracket a different set of tokens than the highlights they paint.
    #[test]
    fn batched_parse_channel_matches_per_anchor_construction() {
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let registry = LanguageRegistry::standard();
        install_highlight_maps(&registry, &styles);
        let lang = registry.for_path(Path::new("a.rs")).unwrap();
        let buffer_id = BufferId::new(1);

        // Nested captures give the search index overlapping, out-of-order ends
        // to bracket, which a flat token stream would not exercise.
        let text =
            "fn outer() -> u32 {\n    let s = \"a string\";\n    if true { 1 } else { 2 }\n}\n";
        let buf = TextBuffer::with_text(buffer_id, text);
        let snapshot = buf.snapshot.clone();

        let out = {
            let mut prior = None;
            let mut prior_map = None;
            parse_buffer_step(
                buffer_id,
                snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                None,
                None,
                &styles,
                None,
                None,
            )
            .expect("parse should succeed")
        };
        assert!(
            out.token_channel.len() > 5,
            "fixture must produce a token stream worth indexing",
        );

        let resolve = |a: &Anchor| snapshot.resolve_anchor(a);
        let per_anchor = BufferSemanticTokens::new(
            out.token_channel.iter().cloned().collect::<Arc<[_]>>(),
            styles.interner.clone(),
            resolve,
        );

        for start in 0..=text.len() {
            for end in [start, start + 1, start + 40, text.len()] {
                let end = end.min(text.len());
                assert_eq!(
                    out.token_channel.overlap_bounds(&(start..end), resolve),
                    per_anchor.overlap_bounds(&(start..end), resolve),
                    "bounds must agree for {start}..{end}",
                );
            }
        }
    }
}
