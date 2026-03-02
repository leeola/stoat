use crate::stoat::Stoat;
use gpui::{App, Context};
use text::{Point, SelectionGoal, ToPoint};

impl Stoat {
    pub fn goto_next_diagnostic(&mut self, cx: &mut Context<Self>) {
        let Some(diag) = self.find_diagnostic(true, cx) else {
            self.flash("No diagnostics", cx);
            return;
        };
        self.jump_to_point(diag.position, cx);
        self.flash(diag.message, cx);
    }

    pub fn goto_prev_diagnostic(&mut self, cx: &mut Context<Self>) {
        let Some(diag) = self.find_diagnostic(false, cx) else {
            self.flash("No diagnostics", cx);
            return;
        };
        self.jump_to_point(diag.position, cx);
        self.flash(diag.message, cx);
    }

    /// Move cursor to a specific point, updating both selections and legacy cursor.
    pub(crate) fn jump_to_point(&mut self, position: Point, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: position,
                end: position,
                reversed: false,
                goal: SelectionGoal::None,
            }],
            &snapshot,
        );
        self.cursor.move_to(position);
        self.ensure_cursor_visible(cx);
        cx.notify();
    }

    fn find_diagnostic(&self, forward: bool, cx: &App) -> Option<ResolvedDiagnostic> {
        let buffer_item = self.active_buffer(cx);
        let buffer_item = buffer_item.read(cx);
        let snapshot = buffer_item.buffer_snapshot(cx);
        let cursor = self.cursor.position();

        let mut resolved: Vec<ResolvedDiagnostic> = buffer_item
            .all_diagnostics()
            .map(|d| {
                let pos = d.range.start.to_point(&snapshot);
                ResolvedDiagnostic {
                    position: pos,
                    message: format_diagnostic_message(d),
                }
            })
            .collect();

        if resolved.is_empty() {
            return None;
        }

        resolved.sort_by(|a, b| a.position.cmp(&b.position));
        resolved.dedup_by(|a, b| a.position == b.position);

        if forward {
            resolved
                .iter()
                .find(|d| d.position > cursor)
                .or_else(|| resolved.first())
                .cloned()
        } else {
            resolved
                .iter()
                .rev()
                .find(|d| d.position < cursor)
                .or_else(|| resolved.last())
                .cloned()
        }
    }
}

#[derive(Clone)]
struct ResolvedDiagnostic {
    position: Point,
    message: String,
}

fn format_diagnostic_message(d: &stoat_lsp::BufferDiagnostic) -> String {
    let severity = match d.severity {
        stoat_lsp::DiagnosticSeverity::Error => "error",
        stoat_lsp::DiagnosticSeverity::Warning => "warning",
        stoat_lsp::DiagnosticSeverity::Information => "info",
        stoat_lsp::DiagnosticSeverity::Hint => "hint",
    };
    let first_line = d.message.lines().next().unwrap_or(&d.message);
    format!("[{severity}] {first_line}")
}

#[cfg(test)]
mod tests {
    use crate::stoat::Stoat;
    use gpui::TestAppContext;
    use stoat_lsp::{BufferDiagnostic, DiagnosticSet, DiagnosticSeverity};
    use text::{Bias, Point};

    fn inject_diagnostics(
        stoat: &mut Stoat,
        diags: Vec<(Point, Point, DiagnosticSeverity, &str)>,
        cx: &mut gpui::App,
    ) {
        let buffer_item = stoat.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer_snapshot(cx);
        let mut set = DiagnosticSet::new();
        for (start, end, severity, message) in diags {
            set.insert(BufferDiagnostic {
                range: snapshot.anchor_at(start, Bias::Left)..snapshot.anchor_at(end, Bias::Right),
                severity,
                code: None,
                source: None,
                message: message.to_string(),
                server_id: 0,
            });
        }
        buffer_item.update(cx, |item, cx| {
            item.update_diagnostics(0, set, 1, cx);
        });
    }

    #[gpui::test]
    fn next_wraps_around(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("aaa\nbbb\nccc\n", cx);
        stoat.update(|s, cx| {
            inject_diagnostics(
                s,
                vec![
                    (
                        Point::new(0, 0),
                        Point::new(0, 3),
                        DiagnosticSeverity::Error,
                        "err on 0",
                    ),
                    (
                        Point::new(2, 0),
                        Point::new(2, 3),
                        DiagnosticSeverity::Warning,
                        "warn on 2",
                    ),
                ],
                cx,
            );
            s.set_cursor_position(Point::new(0, 0));
            s.goto_next_diagnostic(cx);
            assert_eq!(s.cursor_position(), Point::new(2, 0));

            s.goto_next_diagnostic(cx);
            assert_eq!(s.cursor_position(), Point::new(0, 0), "wraps to first");
        });
    }

    #[gpui::test]
    fn prev_wraps_around(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("aaa\nbbb\nccc\n", cx);
        stoat.update(|s, cx| {
            inject_diagnostics(
                s,
                vec![
                    (
                        Point::new(0, 0),
                        Point::new(0, 3),
                        DiagnosticSeverity::Error,
                        "err on 0",
                    ),
                    (
                        Point::new(2, 0),
                        Point::new(2, 3),
                        DiagnosticSeverity::Warning,
                        "warn on 2",
                    ),
                ],
                cx,
            );
            s.set_cursor_position(Point::new(2, 0));
            s.goto_prev_diagnostic(cx);
            assert_eq!(s.cursor_position(), Point::new(0, 0));

            s.goto_prev_diagnostic(cx);
            assert_eq!(s.cursor_position(), Point::new(2, 0), "wraps to last");
        });
    }

    #[gpui::test]
    fn no_diagnostics_flashes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("hello", cx);
        stoat.update(|s, cx| {
            s.goto_next_diagnostic(cx);
            assert_eq!(s.cursor_position(), Point::new(0, 0), "cursor unchanged");
        });
    }
}
