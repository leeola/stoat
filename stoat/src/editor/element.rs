//! Minimal EditorElement for stoat
//!
//! Simplified version that just renders text with syntax highlighting.
//! No gutter, no mouse handling, no complex layout - just get text visible.

use crate::{
    buffer::display::DisplayRow as BufferDisplayRow,
    editor::{style::EditorStyle, view::EditorView},
    git::diff::{BufferDiff, DiffHunkStatus},
    gutter::{DisplayRow, GutterLayout},
    syntax::HighlightedChunks,
};
use gpui::{
    point, px, relative, size, App, Bounds, Element, ElementId, Entity, Font, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, SharedString,
    Style, TextRun, UnderlineStyle, Window,
};
use std::{collections::HashMap, sync::Arc, time::Instant};
use stoat_lsp::BufferDiagnostic;
use text::BufferSnapshot;

pub struct EditorElement {
    view: Entity<EditorView>,
    style: Arc<EditorStyle>,
}

impl EditorElement {
    pub fn new(view: Entity<EditorView>, style: Arc<EditorStyle>) -> Self {
        Self { view, style }
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Request a simple full-size layout for the main editor
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let prepaint_start = Instant::now();

        // Detect if this EditorElement is rendering a minimap (for conditional gutter rendering)
        let is_minimap = self.view.read(cx).stoat.read(cx).is_minimap();

        // Get font and sizing from style (persistent across frames for GPUI's LineLayoutCache)
        let font = self.style.font.clone();
        let font_size = self.style.font_size;
        let line_height = self.style.line_height;

        // Gutter font (reused in paint functions)
        let gutter_font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        let snapshot_start = Instant::now();
        // Get buffer snapshot, token snapshot, display snapshot, diff, and display buffer
        // Batch reads to reduce lock overhead
        let (buffer_snapshot, token_snapshot, is_in_diff_review, display_map_entity, diff) = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let buffer_item_read = buffer_item.read(cx);

            let buffer_snapshot = buffer_item_read.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item_read.token_snapshot();
            let is_in_diff_review = stoat.is_in_diff_review(cx);
            let display_map_entity = stoat.display_map(cx).clone();
            let diff = buffer_item_read.diff().cloned();

            (
                buffer_snapshot,
                token_snapshot,
                is_in_diff_review,
                display_map_entity,
                diff,
            )
        };
        // DisplaySnapshot requires mutable access, done separately
        let display_snapshot = display_map_entity.update(cx, |dm, cx| dm.snapshot(cx));
        // DisplayBuffer for git diff phantom rows (used for metadata only, not coordinates)
        let display_buffer = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            buffer_item.read(cx).display_buffer(cx, is_in_diff_review)
        };
        let snapshot_time = snapshot_start.elapsed();

        // Calculate visible range
        let max_point = buffer_snapshot.max_point();
        let visible_lines_precise = (bounds.size.height - self.style.padding * 2.0) / line_height;
        let max_lines = visible_lines_precise.floor() as u32;

        // Set viewport lines on Stoat and get scroll position
        let stoat_entity = self.view.read(cx).stoat.clone();
        let scroll_y = stoat_entity.update(cx, |stoat, _cx| {
            stoat.set_viewport_lines(visible_lines_precise);
            stoat.update_scroll_animation();
            stoat.scroll_position().y
        });

        // Calculate gutter width (minimap has no gutter)
        let gutter_width = if is_minimap {
            Pixels::ZERO
        } else {
            self.calculate_gutter_width(max_point.row + 1, &gutter_font, window, cx)
        };

        // Calculate visible display row range
        let scroll_offset = scroll_y.floor() as u32;
        let max_display_point = display_snapshot.max_point();
        let max_buffer_point = buffer_snapshot.max_point();

        // In diff review, use display_buffer row count (includes phantom rows)
        let max_row_count = if is_in_diff_review {
            display_buffer.row_count() as u32
        } else {
            max_display_point.row + 1
        };
        let start_display_row = scroll_offset.min(max_row_count.saturating_sub(1));
        let end_display_row = (start_display_row + max_lines).min(max_row_count);

        // DEBUG: Log visible range calculation
        tracing::trace!(
            "EditorElement visible range: buffer_max=({}, {}), display_max=({}, {}), scroll={}, max_lines={}, range={}..{}",
            max_buffer_point.row, max_buffer_point.column,
            max_display_point.row, max_display_point.column,
            scroll_offset, max_lines,
            start_display_row, end_display_row
        );

        // ===== PHASE 1: Collect syntax highlighting for all buffer rows in visible range =====
        // Build a HashMap of buffer_row -> Vec<TextRun> for efficient lookup

        // Determine which buffer rows are in the visible display range
        // Convert each display row to buffer point using DisplaySnapshot
        let mut buffer_rows_in_range = Vec::new();
        for display_row in start_display_row..end_display_row {
            let display_point = stoat_text_transform::DisplayPoint {
                row: display_row,
                column: 0,
            };
            let buffer_point =
                display_snapshot.display_point_to_point(display_point, sum_tree::Bias::Left);
            buffer_rows_in_range.push(buffer_point.row);
        }

        // ===== PHASE 1.5: Query diagnostics for visible buffer rows =====
        let diagnostics_by_row: HashMap<u32, Vec<BufferDiagnostic>> = {
            let buffer_item = {
                let stoat = self.view.read(cx).stoat.read(cx);
                stoat.active_buffer(cx)
            };

            let mut diag_map = HashMap::new();
            for &buffer_row in &buffer_rows_in_range {
                let diags: Vec<BufferDiagnostic> = buffer_item
                    .read(cx)
                    .diagnostics_for_row(buffer_row, &buffer_snapshot)
                    .cloned()
                    .collect();
                if !diags.is_empty() {
                    diag_map.insert(buffer_row, diags);
                }
            }
            diag_map
        };

        // Get min/max buffer rows to create one HighlightedChunks iterator
        let mut highlighted_lines: HashMap<u32, Vec<TextRun>> = HashMap::new();

        if !buffer_rows_in_range.is_empty() {
            let min_buffer_row = *buffer_rows_in_range.iter().min().unwrap();
            let max_buffer_row = *buffer_rows_in_range.iter().max().unwrap();

            let highlight_start = Instant::now();
            // Calculate byte offset range for all visible buffer rows
            let start_offset = buffer_snapshot.point_to_offset(text::Point::new(min_buffer_row, 0));
            let end_offset = if max_buffer_row >= max_point.row {
                buffer_snapshot.len()
            } else {
                buffer_snapshot.point_to_offset(text::Point::new(max_buffer_row + 1, 0))
            };

            // Create one HighlightedChunks iterator for entire visible buffer range
            let chunks = HighlightedChunks::new(
                start_offset..end_offset,
                &buffer_snapshot,
                &token_snapshot,
                &self.style.highlight_map,
            );

            // Process chunks into TextRuns per buffer row
            let mut current_buffer_row = min_buffer_row;
            let mut runs = Vec::new();

            for chunk in chunks {
                // Get color for this chunk
                let color = if let Some(highlight_id) = chunk.highlight_id {
                    self.style
                        .syntax_theme
                        .highlights
                        .get(highlight_id.0 as usize)
                        .map(|(_name, style)| style.color.unwrap_or(self.style.text_color))
                        .unwrap_or(self.style.text_color)
                } else {
                    self.style.text_color
                };

                // Split on newlines to detect row boundaries
                for (split_ix, line_chunk) in chunk.text.split('\n').enumerate() {
                    if split_ix > 0 {
                        // Store runs for completed row
                        highlighted_lines.insert(current_buffer_row, std::mem::take(&mut runs));
                        current_buffer_row += 1;
                    }

                    // Accumulate runs for current row
                    if !line_chunk.is_empty() {
                        runs.push(TextRun {
                            len: line_chunk.len(),
                            font: font.clone(),
                            color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        });
                    }
                }
            }

            // Store final row if we have accumulated runs
            if !runs.is_empty() {
                highlighted_lines.insert(current_buffer_row, runs);
            }
            let highlight_time = highlight_start.elapsed();
            tracing::debug!(
                "prepaint phase1 highlight: {:?} rows={}..{}",
                highlight_time,
                min_buffer_row,
                max_buffer_row
            );
        }

        let shape_start = Instant::now();
        // ===== PHASE 2: Build ShapedLineLayout for each display row =====
        let mut line_layouts = Vec::with_capacity((end_display_row - start_display_row) as usize);
        let mut y = bounds.origin.y + self.style.padding;

        if is_in_diff_review {
            // Diff review path: iterate DisplayBuffer rows (includes phantom rows)
            let range_start = BufferDisplayRow::new(start_display_row);
            let range_end = BufferDisplayRow::new(end_display_row);

            let deleted_text_color = gpui::Hsla {
                h: 0.0,
                s: 0.3,
                l: 0.5,
                a: 0.6,
            };

            for row_info in display_buffer.rows_in_range(range_start..range_end) {
                let display_row = row_info.display_row.row();

                if let Some(buffer_row) = row_info.buffer_row {
                    // Real buffer row: use syntax highlighting from Phase 1
                    let line_text = &row_info.content;
                    let mut runs = Vec::new();

                    if let Some(buffer_runs) = highlighted_lines.get(&buffer_row) {
                        let mut processed_runs = buffer_runs.clone();

                        if matches!(row_info.diff_status, Some(DiffHunkStatus::Modified))
                            && !row_info.modified_ranges.is_empty()
                        {
                            processed_runs = apply_modified_range_backgrounds(
                                processed_runs,
                                &row_info.modified_ranges,
                                &self.style,
                            );
                        }

                        if let Some(row_diagnostics) = diagnostics_by_row.get(&buffer_row) {
                            processed_runs = apply_diagnostic_underlines(
                                processed_runs,
                                buffer_row,
                                row_diagnostics,
                                &buffer_snapshot,
                                &self.style,
                            );
                        }

                        runs.extend(processed_runs);
                    } else {
                        runs.push(TextRun {
                            len: line_text.len().max(1),
                            font: font.clone(),
                            color: self.style.text_color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        });
                    }

                    let shaped = window.text_system().shape_line(
                        SharedString::from(line_text.clone()),
                        font_size,
                        &runs,
                        None,
                    );

                    line_layouts.push(ShapedLineLayout {
                        display_row,
                        buffer_row: Some(buffer_row),
                        shaped,
                        y_position: y,
                        diff_status: row_info.diff_status,
                        is_staged: row_info.is_staged,
                    });
                } else {
                    // Phantom row (deleted content): plain text with deleted color
                    let line_text = &row_info.content;

                    let runs = vec![TextRun {
                        len: line_text.len().max(1),
                        font: font.clone(),
                        color: deleted_text_color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }];

                    let shaped = window.text_system().shape_line(
                        SharedString::from(line_text.clone()),
                        font_size,
                        &runs,
                        None,
                    );

                    line_layouts.push(ShapedLineLayout {
                        display_row,
                        buffer_row: None,
                        shaped,
                        y_position: y,
                        diff_status: row_info.diff_status,
                        is_staged: row_info.is_staged,
                    });
                }

                y += line_height;

                if y > bounds.origin.y + bounds.size.height {
                    break;
                }
            }
        } else {
            // Normal path: iterate display snapshot rows (no phantom rows)
            for display_row in start_display_row..end_display_row {
                let display_point = stoat_text_transform::DisplayPoint {
                    row: display_row,
                    column: 0,
                };
                let buffer_point =
                    display_snapshot.display_point_to_point(display_point, sum_tree::Bias::Left);
                let buffer_row = buffer_point.row;

                let diff_display_row = display_buffer.buffer_row_to_display(buffer_row);
                let diff_row_info = display_buffer.row_at(diff_display_row);

                let mut runs = Vec::new();

                let line_start = text::Point::new(buffer_row, 0);
                let line_end = if buffer_row < max_point.row {
                    text::Point::new(buffer_row + 1, 0)
                } else {
                    buffer_snapshot.max_point()
                };
                let line_text: String = buffer_snapshot
                    .text_for_range(line_start..line_end)
                    .collect();
                let line_text = line_text.trim_end_matches('\n').to_string();

                if let Some(buffer_runs) = highlighted_lines.get(&buffer_row) {
                    let mut processed_runs = buffer_runs.clone();

                    if let Some(row_diagnostics) = diagnostics_by_row.get(&buffer_row) {
                        processed_runs = apply_diagnostic_underlines(
                            processed_runs,
                            buffer_row,
                            row_diagnostics,
                            &buffer_snapshot,
                            &self.style,
                        );
                    }

                    runs.extend(processed_runs);
                } else {
                    runs.push(TextRun {
                        len: line_text.len().max(1),
                        font: font.clone(),
                        color: self.style.text_color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }

                let shaped = window.text_system().shape_line(
                    SharedString::from(line_text),
                    font_size,
                    &runs,
                    None,
                );

                line_layouts.push(ShapedLineLayout {
                    display_row,
                    buffer_row: Some(buffer_row),
                    shaped,
                    y_position: y,
                    diff_status: diff_row_info.and_then(|r| r.diff_status),
                    is_staged: diff_row_info.is_some_and(|r| r.is_staged),
                });

                y += line_height;

                if y > bounds.origin.y + bounds.size.height {
                    break;
                }
            }
        }

        let shape_time = shape_start.elapsed();

        // ===== PHASE 3: Pre-shape line numbers =====
        let line_number_color = gpui::Hsla {
            h: self.style.text_color.h,
            s: self.style.text_color.s,
            l: self.style.text_color.l,
            a: self.style.text_color.a * 0.6,
        };
        let line_number_font_size = font_size * 0.9;

        let shaped_line_numbers: Vec<(gpui::ShapedLine, Pixels, Pixels)> =
            if !is_minimap && self.style.show_line_numbers && gutter_width != Pixels::ZERO {
                line_layouts
                    .iter()
                    .filter_map(|layout| {
                        let buffer_row = layout.buffer_row?;
                        let line_number = format!("{}", buffer_row + 1);
                        let line_number_shared = SharedString::from(line_number);
                        let text_run = TextRun {
                            len: line_number_shared.len(),
                            font: gutter_font.clone(),
                            color: line_number_color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let shaped = window.text_system().shape_line(
                            line_number_shared,
                            line_number_font_size,
                            &[text_run],
                            None,
                        );
                        let x = bounds.origin.x + gutter_width - shaped.width - px(8.0);
                        Some((shaped, x, layout.y_position))
                    })
                    .collect()
            } else {
                Vec::new()
            };

        // ===== PHASE 4: Pre-shape diff symbols =====
        let diff_strip_width = (0.6 * line_height).floor();
        let shaped_diff_symbols: Vec<(gpui::ShapedLine, Pixels, Pixels)> =
            if !is_minimap && is_in_diff_review {
                // Shape the three symbols once, then clone for each line
                let symbol_color = gpui::Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.95,
                    a: 1.0,
                };
                let make_text_run = |s: &str| TextRun {
                    len: s.len(),
                    font: gutter_font.clone(),
                    color: symbol_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped_plus = window.text_system().shape_line(
                    SharedString::from("+"),
                    font_size,
                    &[make_text_run("+")],
                    None,
                );
                let shaped_minus = window.text_system().shape_line(
                    SharedString::from("-"),
                    font_size,
                    &[make_text_run("-")],
                    None,
                );
                let shaped_tilde = window.text_system().shape_line(
                    SharedString::from("~"),
                    font_size,
                    &[make_text_run("~")],
                    None,
                );

                line_layouts
                    .iter()
                    .filter_map(|layout| {
                        let shaped = match layout.diff_status {
                            Some(DiffHunkStatus::Added) => shaped_plus.clone(),
                            Some(DiffHunkStatus::Deleted) => shaped_minus.clone(),
                            Some(DiffHunkStatus::Modified) => shaped_tilde.clone(),
                            None => return None,
                        };
                        let x = bounds.origin.x + (diff_strip_width - shaped.width) / 2.0;
                        Some((shaped, x, layout.y_position))
                    })
                    .collect()
            } else {
                Vec::new()
            };

        // ===== PHASE 4b: Line selection indicators =====
        let (line_select_indicators, line_select_cursor_y) = {
            let stoat_ref = self.view.read(cx).stoat.read(cx);
            let line_selection = stoat_ref.line_selection.as_ref();
            if !is_minimap && is_in_diff_review && line_selection.is_some() {
                let sel = line_selection.unwrap();
                let selected_color = gpui::Hsla {
                    h: 0.35,
                    s: 0.8,
                    l: 0.6,
                    a: 1.0,
                };
                let unselected_color = gpui::Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.5,
                    a: 0.8,
                };
                let make_run = |s: &str, color: gpui::Hsla| TextRun {
                    len: s.len(),
                    font: gutter_font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped_selected = window.text_system().shape_line(
                    SharedString::from("x"),
                    font_size,
                    &[make_run("x", selected_color)],
                    None,
                );
                let shaped_unselected = window.text_system().shape_line(
                    SharedString::from("."),
                    font_size,
                    &[make_run(".", unselected_color)],
                    None,
                );

                // Additions have buffer_row = new_start-1 .. new_start-1+new_lines (0-indexed).
                // Deletions are phantom rows (buffer_row=None) with diff_status=Deleted.
                // We find the hunk by matching addition rows to new_start, then include
                // adjacent deletion rows.
                let new_start_0 = sel.hunk_lines.new_start.saturating_sub(1);
                let new_end_0 = new_start_0 + sel.hunk_lines.new_lines;

                let mut hunk_display_indices: Vec<usize> = Vec::new();
                for (i, layout) in line_layouts.iter().enumerate() {
                    if let Some(DiffHunkStatus::Added | DiffHunkStatus::Modified) =
                        layout.diff_status
                    {
                        if let Some(br) = layout.buffer_row {
                            if br >= new_start_0 && br < new_end_0 {
                                hunk_display_indices.push(i);
                            }
                        }
                    }
                }
                if let Some(&first_add_idx) = hunk_display_indices.first() {
                    let mut j = first_add_idx;
                    while j > 0 {
                        j -= 1;
                        if line_layouts[j].diff_status == Some(DiffHunkStatus::Deleted)
                            && line_layouts[j].buffer_row.is_none()
                        {
                            hunk_display_indices.insert(0, j);
                        } else {
                            break;
                        }
                    }
                }
                if let Some(&last_idx) = hunk_display_indices.last() {
                    let mut j = last_idx + 1;
                    while j < line_layouts.len() {
                        if line_layouts[j].diff_status == Some(DiffHunkStatus::Deleted)
                            && line_layouts[j].buffer_row.is_none()
                        {
                            hunk_display_indices.push(j);
                            j += 1;
                        } else {
                            break;
                        }
                    }
                }

                // Sequential mapping: LineSelection lines and display rows are in the same order
                let mut indicators = Vec::new();
                let mut cursor_y = None;

                for (sel_idx, &disp_idx) in hunk_display_indices.iter().enumerate() {
                    if sel_idx >= sel.selected.len() {
                        break;
                    }
                    let layout = &line_layouts[disp_idx];
                    let shaped = if sel.selected[sel_idx] {
                        shaped_selected.clone()
                    } else {
                        shaped_unselected.clone()
                    };
                    let x = bounds.origin.x + (diff_strip_width - shaped.width) / 2.0;
                    indicators.push((shaped, x, layout.y_position));

                    if sel_idx == sel.cursor_line {
                        cursor_y = Some(layout.y_position);
                    }
                }

                (indicators, cursor_y)
            } else {
                (Vec::new(), None)
            }
        };

        // ===== PHASE 5: Pre-compute cursor layout =====
        let cursor_layout: Option<Bounds<Pixels>> = if !is_minimap {
            let stoat = self.view.read(cx).stoat.read(cx);
            if self.view.read(cx).is_focused(window) {
                let cursor_position = stoat.cursor_position();

                line_layouts.iter().find_map(|layout| {
                    if layout.buffer_row == Some(cursor_position.row) {
                        let x_offset = layout.shaped.x_for_index(cursor_position.column as usize);
                        let cursor_x =
                            bounds.origin.x + gutter_width + self.style.padding + x_offset;
                        Some(Bounds {
                            origin: point(cursor_x, layout.y_position),
                            size: size(px(2.0), line_height),
                        })
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        };

        // ===== PHASE 6: Pre-compute selection bounds =====
        let selection_bounds: Vec<Bounds<Pixels>> = if !is_minimap {
            let stoat = self.view.read(cx).stoat.read(cx);
            let selections = stoat.active_selections(cx);

            selections
                .iter()
                .filter(|s| !s.is_empty())
                .flat_map(|selection| {
                    let start = selection.start;
                    let end = selection.end;
                    let mut bounds_list = Vec::new();

                    if start.row == end.row {
                        // Single-line selection
                        if let Some(layout) = line_layouts
                            .iter()
                            .find(|l| l.buffer_row == Some(start.row))
                        {
                            let start_x = layout.shaped.x_for_index(start.column as usize);
                            let end_x = layout.shaped.x_for_index(end.column as usize);
                            let selection_width = end_x - start_x;

                            bounds_list.push(Bounds {
                                origin: point(
                                    bounds.origin.x + gutter_width + self.style.padding + start_x,
                                    layout.y_position,
                                ),
                                size: size(selection_width, line_height),
                            });
                        }
                    } else {
                        // Multi-line selection
                        for row in start.row..=end.row {
                            if let Some(layout) =
                                line_layouts.iter().find(|l| l.buffer_row == Some(row))
                            {
                                let (col_start, col_end) = if row == start.row {
                                    (start.column, u32::MAX)
                                } else if row == end.row {
                                    (0, end.column)
                                } else {
                                    (0, u32::MAX)
                                };

                                let start_x = layout.shaped.x_for_index(col_start as usize);
                                let end_x = if col_end == u32::MAX {
                                    layout.shaped.width
                                } else {
                                    layout.shaped.x_for_index(col_end as usize)
                                };
                                let selection_width = end_x - start_x;

                                bounds_list.push(Bounds {
                                    origin: point(
                                        bounds.origin.x
                                            + gutter_width
                                            + self.style.padding
                                            + start_x,
                                        layout.y_position,
                                    ),
                                    size: size(selection_width, line_height),
                                });
                            }
                        }
                    }
                    bounds_list
                })
                .collect()
        } else {
            Vec::new()
        };

        // Pre-compute strip width for gutter diff indicators
        let strip_width = if is_in_diff_review {
            (0.6 * line_height).floor()
        } else {
            (0.275 * line_height).floor()
        };

        let total_prepaint = prepaint_start.elapsed();
        if !is_minimap {
            tracing::debug!(
                "prepaint total={:?} (snapshot={:?}, shape={:?}) lines={}",
                total_prepaint,
                snapshot_time,
                shape_time,
                line_layouts.len()
            );
        }

        EditorPrepaintState {
            line_layouts,
            gutter_width,
            diagnostics_by_row,
            gutter_font,
            shaped_line_numbers,
            shaped_diff_symbols,
            cursor_layout,
            selection_bounds,
            diff,
            buffer_snapshot,
            strip_width,
            is_in_diff_review,
            line_select_indicators,
            line_select_cursor_y,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Detect if this EditorElement is rendering a minimap (for conditional gutter rendering)
        let is_minimap = self.view.read(cx).stoat.read(cx).is_minimap();

        // Get line height from style (persistent across frames for cache stability)
        let line_height = self.style.line_height;

        // Paint background
        window.paint_quad(gpui::PaintQuad {
            bounds,
            corner_radii: 0.0.into(),
            background: self.style.background.into(),
            border_color: gpui::transparent_black(),
            border_widths: 0.0.into(),
            border_style: gpui::BorderStyle::default(),
        });

        // Check if scroll animation is in progress (for requesting next frame)
        let stoat_entity = self.view.read(cx).stoat.clone();
        let is_animating = stoat_entity.read(cx).is_scroll_animating();
        if is_animating {
            let view = self.view.clone();
            window.on_next_frame(move |_, cx| {
                view.update(cx, |_, cx| cx.notify());
            });
        }

        // ===== FAST PATH: Just paint the pre-shaped lines from prepaint =====
        // All expensive work (syntax highlighting + text shaping) was done in prepaint()

        // Paint full-width background bars for diff rows (before text)
        self.paint_diff_backgrounds(
            bounds,
            &prepaint.line_layouts,
            prepaint.gutter_width,
            window,
            cx,
        );

        // Paint line selection cursor highlight bar
        if let Some(cursor_y) = prepaint.line_select_cursor_y {
            let highlight_color = gpui::Hsla {
                h: 0.55,
                s: 0.6,
                l: 0.5,
                a: 0.25,
            };
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds {
                    origin: point(bounds.origin.x, cursor_y),
                    size: size(bounds.size.width, line_height),
                },
                corner_radii: 0.0.into(),
                background: highlight_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }

        // Paint selections (before text)
        if !is_minimap {
            self.paint_selections(prepaint, window);
        }

        // Collect buffer line positions for cursor/gutter rendering (only real buffer rows, not
        // phantoms)
        let mut line_positions: Vec<(u32, Pixels)> = Vec::new();

        // Pre-compute content X position (constant for all lines)
        let content_x = bounds.origin.x + prepaint.gutter_width + self.style.padding;

        // Paint all pre-shaped lines (includes both buffer rows and phantom rows)
        for layout in &prepaint.line_layouts {
            // Track buffer row positions for cursor/line numbers
            if let Some(buffer_row) = layout.buffer_row {
                line_positions.push((buffer_row, layout.y_position));
            }

            // Paint backgrounds first
            if let Err(e) = layout.shaped.paint_background(
                point(content_x, layout.y_position),
                line_height,
                window,
                cx,
            ) {
                tracing::error!(
                    "Failed to paint background for display row {} (buffer row {:?}): {:?}",
                    layout.display_row,
                    layout.buffer_row,
                    e
                );
            }

            // Then paint text on top
            if let Err(e) =
                layout
                    .shaped
                    .paint(point(content_x, layout.y_position), line_height, window, cx)
            {
                tracing::error!(
                    "Failed to paint display row {} (buffer row {:?}): {:?}",
                    layout.display_row,
                    layout.buffer_row,
                    e
                );
            }
        }

        // Skip gutter and cursor rendering for minimap
        if !is_minimap {
            // Calculate visible buffer row range for gutter
            let start_line = line_positions.first().map(|(idx, _)| *idx).unwrap_or(0);
            let end_line = line_positions.last().map(|(idx, _)| *idx + 1).unwrap_or(0);

            // Paint git diff indicators in gutter (behind line numbers)
            self.paint_gutter(bounds, start_line..end_line, prepaint, window);

            // Paint diff symbols (+/-) in gutter
            self.paint_diff_symbols(prepaint, window, cx);

            // Paint line selection indicators (overdraws diff symbols for the active hunk)
            for (shaped, x, y) in &prepaint.line_select_indicators {
                let _ = shaped.paint(point(*x, *y), line_height, window, cx);
            }

            // Paint diagnostic icons in gutter
            self.paint_diagnostic_icons(
                bounds,
                &prepaint.line_layouts,
                &prepaint.diagnostics_by_row,
                prepaint.gutter_width,
                window,
            );

            // Paint line numbers in gutter
            self.paint_line_numbers(prepaint, window, cx);

            // Paint cursor on top of text
            self.paint_cursor(prepaint, window);
        }
    }
}

impl EditorElement {
    /// Paint line numbers in the gutter (uses pre-shaped lines from prepaint)
    fn paint_line_numbers(
        &self,
        prepaint: &EditorPrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (shaped, x, y) in &prepaint.shaped_line_numbers {
            if let Err(e) = shaped.paint(point(*x, *y), self.style.line_height, window, cx) {
                tracing::error!("Failed to paint line number: {:?}", e);
            }
        }
    }

    /// Paint diff symbols (+/-) in the gutter (uses pre-shaped symbols from prepaint)
    fn paint_diff_symbols(
        &self,
        prepaint: &EditorPrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (shaped, x, y) in &prepaint.shaped_diff_symbols {
            let _ = shaped.paint(point(*x, *y), self.style.line_height, window, cx);
        }
    }

    /// Calculate gutter width based on line numbers to display
    fn calculate_gutter_width(
        &self,
        max_line_number: u32,
        gutter_font: &Font,
        window: &mut Window,
        cx: &mut App,
    ) -> Pixels {
        if !self.style.show_line_numbers {
            return Pixels::ZERO;
        }

        // Format the maximum line number to measure its width
        let max_line_text = format!("{max_line_number}");

        // Measure the width of the maximum line number
        let line_number_shared = SharedString::from(max_line_text);
        let text_run = TextRun {
            len: line_number_shared.len(),
            font: gutter_font.clone(),
            color: self.style.text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let shaped = window.text_system().shape_line(
            line_number_shared,
            self.style.font_size * 0.9, // Slightly smaller font for line numbers
            &[text_run],
            None,
        );

        // Layout: [diff strip with +/- overlaid][line numbers]
        // Strip width: wider during diff review for better visibility
        let stoat = self.view.read(cx).stoat.read(cx);
        let strip_width = if stoat.is_in_diff_review(cx) {
            (0.6 * self.style.line_height).floor() // Wider in review mode
        } else {
            (0.275 * self.style.line_height).floor() // Normal width
        };
        strip_width + shaped.width + px(16.0)
    }

    /// Paint the cursor (uses pre-computed bounds from prepaint)
    fn paint_cursor(&self, prepaint: &EditorPrepaintState, window: &mut Window) {
        if let Some(cursor_bounds) = prepaint.cursor_layout {
            window.paint_quad(gpui::PaintQuad {
                bounds: cursor_bounds,
                corner_radii: 0.0.into(),
                background: self.style.text_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }
    }

    /// Paint selections (uses pre-computed bounds from prepaint)
    fn paint_selections(&self, prepaint: &EditorPrepaintState, window: &mut Window) {
        let selection_color = gpui::Hsla {
            h: 210.0 / 360.0,
            s: 0.7,
            l: 0.5,
            a: 0.3,
        };

        for bounds in &prepaint.selection_bounds {
            window.paint_quad(gpui::PaintQuad {
                bounds: *bounds,
                corner_radii: 0.0.into(),
                background: selection_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }
    }

    /// Paint git diff indicators in the gutter
    fn paint_gutter(
        &self,
        bounds: Bounds<Pixels>,
        visible_rows: std::ops::Range<u32>,
        prepaint: &EditorPrepaintState,
        window: &mut Window,
    ) {
        if !self.style.show_diff_indicators || prepaint.gutter_width == Pixels::ZERO {
            return;
        }

        // Create gutter bounds (left portion of editor)
        let gutter_bounds = Bounds {
            origin: bounds.origin,
            size: size(prepaint.gutter_width, bounds.size.height),
        };

        // Create gutter layout with diff indicators (uses precomputed values)
        let gutter_layout = GutterLayout::new(
            gutter_bounds,
            visible_rows,
            &prepaint.line_layouts,
            prepaint.diff.as_ref(),
            &prepaint.buffer_snapshot,
            prepaint.gutter_width,
            self.style.padding,
            self.style.line_height,
            prepaint.strip_width,
            prepaint.is_in_diff_review,
        );

        // Paint diff indicators
        for indicator in &gutter_layout.diff_indicators {
            let diff_color = match (indicator.status, indicator.is_staged) {
                (DiffHunkStatus::Added, true) => self.style.diff_staged_added_color,
                (DiffHunkStatus::Modified, true) => self.style.diff_staged_modified_color,
                (DiffHunkStatus::Deleted, true) => self.style.diff_staged_deleted_color,
                (DiffHunkStatus::Added, false) => self.style.diff_added_color,
                (DiffHunkStatus::Modified, false) => self.style.diff_modified_color,
                (DiffHunkStatus::Deleted, false) => self.style.diff_deleted_color,
            };

            // Blend with background for subtle appearance (60% opacity)
            let blended_color = gpui::Hsla {
                h: diff_color.h,
                s: diff_color.s,
                l: diff_color.l,
                a: diff_color.a * 0.6,
            };

            window.paint_quad(gpui::PaintQuad {
                bounds: indicator.bounds,
                corner_radii: indicator.corner_radii,
                background: blended_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }
    }

    /// Paint diagnostic icons in the gutter.
    ///
    /// Displays colored circles in the gutter for lines with diagnostics.
    /// Uses the most severe diagnostic color when multiple diagnostics exist on one line.
    fn paint_diagnostic_icons(
        &self,
        bounds: Bounds<Pixels>,
        line_layouts: &[ShapedLineLayout],
        diagnostics_by_row: &HashMap<u32, Vec<BufferDiagnostic>>,
        gutter_width: Pixels,
        window: &mut Window,
    ) {
        if gutter_width == Pixels::ZERO {
            return;
        }

        let icon_radius = (self.style.line_height * 0.15).floor();
        let icon_x = bounds.origin.x + gutter_width - icon_radius * 2.5;

        for layout in line_layouts {
            if let Some(buffer_row) = layout.buffer_row {
                if let Some(row_diags) = diagnostics_by_row.get(&buffer_row) {
                    if let Some(most_severe) = row_diags.iter().min_by_key(|d| d.severity) {
                        let color = self.style.diagnostic_color(most_severe.severity);
                        let icon_y = layout.y_position + self.style.line_height / 2.0;

                        window.paint_quad(gpui::PaintQuad {
                            bounds: Bounds {
                                origin: point(icon_x, icon_y - icon_radius),
                                size: size(icon_radius * 2.0, icon_radius * 2.0),
                            },
                            corner_radii: icon_radius.into(),
                            background: color.into(),
                            border_color: gpui::transparent_black(),
                            border_widths: 0.0.into(),
                            border_style: gpui::BorderStyle::default(),
                        });
                    }
                }
            }
        }
    }

    /// Paint full-width background bars for diff rows.
    ///
    /// Paints subtle colored rectangles spanning from gutter edge to right edge
    /// for all rows with diff status. These provide visual context for changes.
    fn paint_diff_backgrounds(
        &self,
        bounds: Bounds<Pixels>,
        line_layouts: &[ShapedLineLayout],
        _gutter_width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let stoat = self.view.read(cx).stoat.read(cx);
        let is_in_diff_review = stoat.is_in_diff_review(cx);

        // Only paint backgrounds in review mode
        if !is_in_diff_review {
            return;
        }

        for layout in line_layouts {
            if let Some(status) = layout.diff_status {
                let base_color = match (status, layout.is_staged) {
                    (DiffHunkStatus::Added, true) => self.style.diff_staged_added_color,
                    (DiffHunkStatus::Modified, true) => self.style.diff_staged_modified_color,
                    (DiffHunkStatus::Deleted, true) => self.style.diff_staged_deleted_color,
                    (DiffHunkStatus::Added, false) => self.style.diff_added_color,
                    (DiffHunkStatus::Modified, false) => self.style.diff_modified_color,
                    (DiffHunkStatus::Deleted, false) => self.style.diff_deleted_color,
                };

                // Make it very subtle for full-width backgrounds (15% opacity in review mode)
                let background_color = gpui::Hsla {
                    h: base_color.h,
                    s: base_color.s,
                    l: base_color.l,
                    a: 0.15,
                };

                // Paint full-width background bar
                let bar_bounds = Bounds {
                    origin: point(bounds.origin.x, layout.y_position),
                    size: size(bounds.size.width, self.style.line_height),
                };

                window.paint_quad(gpui::PaintQuad {
                    bounds: bar_bounds,
                    corner_radii: 0.0.into(),
                    background: background_color.into(),
                    border_color: gpui::transparent_black(),
                    border_widths: 0.0.into(),
                    border_style: gpui::BorderStyle::default(),
                });
            }
        }
    }
}

/// Apply stronger background highlighting to modified ranges within a line.
///
/// Splits existing TextRuns at the boundaries of modified_ranges and applies
/// a more opaque background color to runs within those ranges.
///
/// # Arguments
///
/// * `runs` - Original syntax-highlighted text runs
/// * `modified_ranges` - Byte ranges that were modified (from word diff)
/// * `style` - Editor style for getting diff colors
///
/// # Returns
///
/// New list of TextRuns with backgrounds applied to modified regions
fn apply_modified_range_backgrounds(
    runs: Vec<TextRun>,
    modified_ranges: &[std::ops::Range<usize>],
    _style: &EditorStyle,
) -> Vec<TextRun> {
    if modified_ranges.is_empty() {
        return runs;
    }

    let mut result = Vec::new();
    let mut byte_offset = 0;

    // Subtle background for modified words (like GitHub)
    // Use a desaturated version of the diff color for subtlety
    let modified_bg = gpui::Hsla {
        h: _style.diff_modified_color.h,
        s: _style.diff_modified_color.s * 0.5, // Reduced saturation for subtlety
        l: 0.4,                                // Slightly lighter
        a: 0.2,                                // 20% opacity for subtle appearance
    };

    for run in runs {
        let run_start = byte_offset;
        let run_end = byte_offset + run.len;

        // Find which modified ranges overlap with this run
        let mut splits = Vec::new();
        splits.push(run_start); // Always include run start

        for range in modified_ranges {
            // Add split points where modified ranges intersect this run
            if range.start > run_start && range.start < run_end {
                splits.push(range.start);
            }
            if range.end > run_start && range.end < run_end {
                splits.push(range.end);
            }
        }

        splits.push(run_end); // Always include run end
        splits.sort_unstable();
        splits.dedup();

        // Create sub-runs for each split segment
        for i in 0..splits.len() - 1 {
            let segment_start = splits[i];
            let segment_end = splits[i + 1];
            let segment_len = segment_end - segment_start;

            if segment_len == 0 {
                continue;
            }

            // Check if this segment is within any modified range
            let is_modified = modified_ranges
                .iter()
                .any(|range| segment_start >= range.start && segment_end <= range.end);

            result.push(TextRun {
                len: segment_len,
                font: run.font.clone(),
                color: run.color,
                background_color: if is_modified {
                    Some(modified_bg)
                } else {
                    run.background_color
                },
                underline: run.underline,
                strikethrough: run.strikethrough,
            });
        }

        byte_offset = run_end;
    }

    result
}

/// Apply diagnostic underlines to text runs based on diagnostic ranges.
///
/// Converts diagnostic anchor ranges to column ranges, then splits text runs
/// to apply wavy underlines to segments that overlap diagnostic ranges.
fn apply_diagnostic_underlines(
    runs: Vec<TextRun>,
    buffer_row: u32,
    diagnostics: &[BufferDiagnostic],
    snapshot: &text::BufferSnapshot,
    style: &EditorStyle,
) -> Vec<TextRun> {
    if diagnostics.is_empty() {
        return runs;
    }

    // Convert diagnostic ranges from anchors to column ranges for this row
    let mut diagnostic_ranges: Vec<(std::ops::Range<u32>, gpui::Hsla)> = Vec::new();
    for diag in diagnostics {
        use text::ToPoint;
        let start_point = diag.range.start.to_point(snapshot);
        let end_point = diag.range.end.to_point(snapshot);

        // Only include diagnostics that overlap this row
        if start_point.row <= buffer_row && end_point.row >= buffer_row {
            let start_col = if start_point.row == buffer_row {
                start_point.column
            } else {
                0
            };
            let end_col = if end_point.row == buffer_row {
                end_point.column
            } else {
                u32::MAX // Extend to end of line
            };

            let color = style.diagnostic_color(diag.severity);
            diagnostic_ranges.push((start_col..end_col, color));
        }
    }

    if diagnostic_ranges.is_empty() {
        return runs;
    }

    let mut result = Vec::new();
    let mut byte_offset = 0u32;

    for run in runs {
        let run_byte_len = run.len as u32;
        let run_start = byte_offset;
        let run_end = byte_offset + run_byte_len;

        // Find which diagnostic ranges overlap with this run
        let mut splits = Vec::new();
        splits.push(run_start);

        for (range, _) in &diagnostic_ranges {
            if range.start > run_start && range.start < run_end {
                splits.push(range.start);
            }
            if range.end > run_start && range.end < run_end {
                splits.push(range.end);
            }
        }

        splits.push(run_end);
        splits.sort_unstable();
        splits.dedup();

        // Create sub-runs for each split segment
        for i in 0..splits.len() - 1 {
            let segment_start = splits[i];
            let segment_end = splits[i + 1];
            let segment_len = (segment_end - segment_start) as usize;

            if segment_len == 0 {
                continue;
            }

            // Find the most severe diagnostic overlapping this segment
            let underline = diagnostic_ranges
                .iter()
                .find(|(range, _)| segment_start >= range.start && segment_end <= range.end)
                .map(|(_, color)| UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(*color),
                    wavy: true,
                });

            result.push(TextRun {
                len: segment_len,
                font: run.font.clone(),
                color: run.color,
                background_color: run.background_color,
                underline,
                strikethrough: run.strikethrough,
            });
        }

        byte_offset = run_end;
    }

    result
}

/// Prepaint state for editor rendering (following Zed's architecture).
///
/// Caches expensive computations (syntax highlighting, text shaping) done in prepaint
/// so that paint() can be fast and just draw the pre-computed results.
pub struct EditorPrepaintState {
    /// Pre-shaped line layouts for visible lines (includes phantom rows)
    pub line_layouts: Vec<ShapedLineLayout>,
    /// Gutter width for positioning
    pub gutter_width: Pixels,
    /// Diagnostics by buffer row (for gutter icons)
    pub diagnostics_by_row: HashMap<u32, Vec<BufferDiagnostic>>,
    /// Gutter font (reused across paint functions)
    pub gutter_font: Font,
    /// Pre-shaped line numbers: (shaped_line, x_position, y_position)
    pub shaped_line_numbers: Vec<(gpui::ShapedLine, Pixels, Pixels)>,
    /// Pre-shaped diff symbols: (shaped_line, x_position, y_position)
    pub shaped_diff_symbols: Vec<(gpui::ShapedLine, Pixels, Pixels)>,
    /// Pre-computed cursor bounds (if visible)
    pub cursor_layout: Option<Bounds<Pixels>>,
    /// Pre-computed selection bounds
    pub selection_bounds: Vec<Bounds<Pixels>>,
    /// Git diff for gutter indicators
    pub diff: Option<BufferDiff>,
    /// Buffer snapshot for gutter diff position calculation
    pub buffer_snapshot: BufferSnapshot,
    /// Width of diff indicator strip (wider in diff review mode)
    pub strip_width: Pixels,
    /// Whether editor is in diff review mode
    pub is_in_diff_review: bool,
    /// Line selection checkbox indicators: (shaped_line, x_position, y_position)
    pub line_select_indicators: Vec<(gpui::ShapedLine, Pixels, Pixels)>,
    /// Y position of the cursor line during line_select mode
    pub line_select_cursor_y: Option<Pixels>,
}

/// A single line that has been shaped and is ready to paint.
///
/// Represents either a real buffer line or a phantom line (for git diffs).
/// Contains display/buffer indices, pre-shaped text, and Y position for fast painting.
pub struct ShapedLineLayout {
    /// Display row index (includes phantom rows)
    pub display_row: u32,
    /// Buffer row index (None for phantom rows)
    pub buffer_row: Option<u32>,
    /// Pre-shaped text from GPUI (already has syntax highlighting colors)
    pub shaped: gpui::ShapedLine,
    /// Y position where this line should be painted
    pub y_position: Pixels,
    /// Diff status for gutter symbol rendering
    pub diff_status: Option<DiffHunkStatus>,
    /// Whether this row's change is staged in the git index
    pub is_staged: bool,
}

impl DisplayRow for ShapedLineLayout {
    fn y_position(&self) -> Pixels {
        self.y_position
    }

    fn diff_status(&self) -> Option<DiffHunkStatus> {
        self.diff_status
    }

    fn is_staged(&self) -> bool {
        self.is_staged
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
