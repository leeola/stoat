//! The fixtures a test shares with tests in other modules.
//!
//! A subsystem that moved out of app.rs took its tests with it, and those tests
//! still need what app.rs's own tests need: an open modal's real layout, a
//! scratch buffer on screen, the APC batches a frame queued. Leaving them inside
//! app.rs's private test module would mean widening app internals just so a test
//! elsewhere could reach them, so they live here instead, where every module's
//! tests can name them and nothing production-facing changes.
//!
//! Everything here reads the app through state that is already visible, so
//! nothing widened to make the module possible.

use crate::{
    action_handlers,
    app::{modal_split_share, modal_zoom_steps, ModalKind},
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    diff_map::{DiffHunkStatus, GutterMark, MarkState},
    editor_state::EditorId,
    host::{lsp::IncomingRequest, FakeTerminalSession},
    keymap::{KeymapState, ResolvedAction, ResolvedArg, StateValue},
    keymap_state::StoatKeymapState,
    lsp::drain,
    pane::FocusTarget,
    term_screen::TermScreen,
    term_session::TermSession,
    test_harness::TestHarness,
    theme::Theme,
    walkthrough::{self, Location, Point, Walkthrough},
};
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
use lsp_types::{
    notification::{DidChangeWatchedFiles, Notification},
    Diagnostic, DiagnosticSeverity, DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher,
    GlobPattern, NumberOrString, Position, Range, Registration, RegistrationParams,
    ServerCapabilities, SymbolInformation, SymbolKind, WatchKind,
};
use ratatui::layout::Rect;
use std::{path::PathBuf, sync::Arc};
use stoat_action::{OpenFile, OpenFileFinder};
use stoat_config::{MinimapMode, Value};
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{self, LineSummary, MinimapRun};
use tokio::sync::mpsc::UnboundedReceiver;

/// The open help modal's layout, sized from the same content and zoom the
/// renderer would read, so a test rect matches the painted one.
pub fn help_layout(h: &TestHarness) -> crate::render::help::HelpLayout {
    let help = h.stoat.help.as_ref().expect("the help modal is open");
    crate::render::help::help_layout(
        h.stoat.size(),
        crate::render::help::help_content_rows(help),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::Help),
    )
    .expect("the help modal fits the test viewport")
}

/// The open palette's sizing inputs, so a test lays its box out exactly as
/// the renderer would, as the content rows, the zoom steps, the header rows
/// that the last frame drew, and the doc's rows.
pub fn palette_sizing(h: &TestHarness) -> (u16, i8, u16, u16) {
    let palette = h
        .stoat
        .command_palette
        .as_ref()
        .expect("the palette is open");
    (
        palette.list_rows_hint(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::Palette),
        palette.header_rows,
        palette.doc_rows(),
    )
}

/// The open argument picker's layout, sized from the same inputs the renderer
/// reads, so a test rect matches the painted one.
pub fn palette_arg_layout(h: &TestHarness) -> crate::render::command_palette::PaletteArgLayout {
    crate::mouse::open_palette_arg_layout(
        &h.stoat,
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::Palette),
    )
    .expect("the open argument picker fits the test terminal")
}

/// The open finder's layout, sized from the same content and zoom the
/// renderer would read, so a test rect matches the painted one.
pub fn finder_layout(h: &TestHarness) -> crate::render::file_finder::FinderLayout {
    crate::mouse::open_finder_layout(
        &h.stoat,
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
    )
    .expect("the open finder fits the test terminal")
}

/// A harness with its file finder open and painted over `files` one-line
/// files, under a stoatty whose finder list lays its rows out by pixels.
///
/// The stoatty speaks the current protocol, with a 9 by 18 px cell and a
/// display scale of 1, so a result row is 20 px high and 21 px from the next.
pub fn pixel_finder(files: usize) -> TestHarness {
    let mut h = TestHarness::default();
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);

    let root = PathBuf::from("/pixel-finder");
    for n in 0..files {
        h.fake_fs()
            .insert_file(root.join(format!("f{n:02}.rs")), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    h.stoat.render();
    h
}

/// The layout of the open target picker of `kind`, sized from the same zoom
/// and split the renderer reads, so a test rect matches the painted one.
pub fn picker_layout(h: &TestHarness, kind: ModalKind) -> crate::render::picker::PickerLayout {
    crate::render::picker::target_picker_layout(
        h.stoat.size(),
        modal_zoom_steps(&h.stoat.modal_zoom, kind),
        modal_split_share(&h.stoat.modal_split, kind),
    )
    .expect("picker laid out")
}

/// The list rows of the open target picker of `kind`.
pub fn target_picker_rows(h: &TestHarness, kind: ModalKind) -> Rect {
    picker_layout(h, kind).list
}

/// Drain every APC batch currently queued on `rx` into one decoded command
/// list. A plain editor pane fills its pages asynchronously, so one
/// `emit_smooth_scroll` pushes the region/scroll batch plus a fill batch per
/// page. Draining folds them together so a test reads the whole emit at once.
pub fn drain_apc(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<command::Command> {
    let mut cmds = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        cmds.extend(command::decode_stream(&batch));
    }
    cmds
}

/// `store` with the lengths and the splices of `commands` to minimap content
/// `content_id` applied, as the terminal applies them.
pub fn apply_minimap(store: &mut Vec<LineSummary>, content_id: u32, commands: &[command::Command]) {
    for cmd in commands {
        match cmd {
            command::Command::MinimapLen(len) if len.content_id == content_id => {
                store.resize(len.lines as usize, LineSummary::default());
            },
            command::Command::MinimapLines(splice) if splice.content_id == content_id => {
                let start = splice.start as usize;
                store.splice(start..start + splice.removed as usize, splice.lines.clone());
            },
            _ => {},
        }
    }
}

/// The minimap strips `commands` declare, as their strip id, left column,
/// width, and content id.
pub fn minimap_declarations(commands: &[command::Command]) -> Vec<(u32, u16, u16, u32)> {
    commands
        .iter()
        .filter_map(|cmd| match cmd {
            command::Command::Minimap(strip) => {
                Some((strip.strip_id, strip.left, strip.width, strip.content_id))
            },
            _ => None,
        })
        .collect()
}

/// The rows of a minimap `store` that carry runs, each beside its index.
pub fn marked_rows(store: &[LineSummary]) -> Vec<(usize, Vec<MinimapRun>)> {
    store
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.is_empty())
        .map(|(at, row)| (at, row.to_vec()))
        .collect()
}

/// A Coverage Rail row's mark in `class`, two of the rail's three columns.
pub fn rail_mark(class: u8) -> Vec<MinimapRun> {
    vec![MinimapRun {
        start_col: 0,
        len: 2,
        class,
        weight: 255,
    }]
}

/// Open a buffer long enough to fill a per-pane minimap strip, with the strip's
/// rect already reserved as a paint leaves it.
pub fn open_with_minimap_strip(h: &mut TestHarness) -> EditorId {
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    let body: String = (0..60)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    open_scratch_file(h, &body);
    let editor_id = h.stoat.focused_editor_ids().expect("editor").0;
    let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
    editor.minimap_rect = Some(Rect::new(72, 0, 8, 10));
    editor.viewport_rows = Some(20);
    editor_id
}

/// Open a buffer holding `contents`, for a test that needs a file on screen
/// rather than a particular one. Returns the path it was written to.
pub fn open_scratch_file(h: &mut TestHarness, contents: &str) -> PathBuf {
    open_scratch_at(h, "/ws/buf.txt", contents)
}

/// Open a buffer holding `contents` at `path`, with the workspace rooted at
/// the path's parent. Returns the path.
///
/// For a test whose subject depends on the file's name, such as a language
/// picked by its extension, or a snapshot that shows the path.
pub fn open_scratch_at(h: &mut TestHarness, path: &str, contents: &str) -> PathBuf {
    let path = PathBuf::from(path);
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
    h.stoat.active_workspace_mut().git_root =
        path.parent().expect("path has a parent").to_path_buf();
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    path
}

/// Write `files` under a fake git root and make that root the active
/// workspace's, for a test whose subject is which file the editor picked.
/// Returns the root the relative names hang off.
///
/// Nothing is opened. The name of the root is arbitrary, and is kept as it is
/// only so the LSP tests that grew this helper go on writing the same paths.
pub fn seed(h: &mut TestHarness, files: &[(&str, &str)]) -> PathBuf {
    let root = PathBuf::from("/lsp-did-open-test");
    h.fake_fs().insert_files(
        files
            .iter()
            .map(|(rel, content)| (root.join(rel), content.as_bytes())),
    );
    h.stoat.active_workspace_mut().git_root = root.clone();
    root
}

/// Open `path` in the focused pane and settle, so whatever the open armed has
/// resolved before the test asserts.
pub fn open_buffer(h: &mut TestHarness, path: PathBuf) {
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

/// Bring up the in-process stcfg server against an empty `config.stcfg`.
///
/// Resets the registry so no injected sole host suppresses auto-spawn, opens
/// the file (queuing the in-process spawn), then drives one `update` so the
/// parked host installs. Returns the file path.
pub fn open_stcfg_with_server(h: &mut TestHarness) -> PathBuf {
    h.stoat.lsp_registry = crate::lsp::registry::LspRegistry::new();
    h.stoat.set_lsp_auto_spawn(true);

    let root = PathBuf::from("/cfg");
    let path = root.join("config.stcfg");
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), b"".as_slice())));
    h.stoat.active_workspace_mut().git_root = root;

    open_buffer(h, path.clone());
    h.type_keys("i");
    path
}

/// Open `name` holding `contents` and drive the parse to completion, for a test
/// whose subject reads the syntax tree rather than the text.
pub fn open_indent_buffer(h: &mut TestHarness, name: &str, contents: &[u8]) {
    let root = PathBuf::from("/indent");
    let path = root.join(name);
    h.fake_fs().insert_file(&path, contents);
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    // Three beats, because a parse is spawned and installed by different
    // passes. The first drive spawns the job, the settle runs it on the
    // pool, and the second drive polls its output into the registry, which
    // is where auto-indent reads the tree from.
    h.stoat.drive_background();
    h.settle();
    h.stoat.drive_background();
}

/// An error diagnostic one column wide at `(line, col)`, for a test whose
/// subject is what the editor does with a diagnostic rather than what is in it.
pub fn diag(line: u32, col: u32, message: &str) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: None,
        message: message.to_string(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// The screen rect of whatever holds focus, split pane or dock, so a test aims
/// a click at the same cells the paint used.
pub fn focused_editor_pane_area(h: &TestHarness) -> Rect {
    let ws = h.stoat.active_workspace();
    match ws.focus {
        FocusTarget::SplitPane => ws.panes.pane(ws.panes.focus()).area,
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).expect("dock").area,
    }
}

/// A synthetic mouse event at a screen cell, for a test driving the router
/// through the same entry the terminal uses.
pub fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// Advertise document symbols on the fake server, for a test whose subject is
/// what the symbol picker or finder does with them.
pub fn enable_document_symbols(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        document_symbol_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

/// [`enable_document_symbols`] with hover as well, for the finder tests that
/// read a symbol's documentation beside it.
pub fn enable_document_symbols_and_hover(h: &TestHarness) {
    use lsp_types::{HoverProviderCapability, OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    });
}

/// Advertise workspace symbols on the fake server, for a test whose subject is
/// the workspace-scope finder.
pub fn enable_workspace_symbols(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        workspace_symbol_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

/// A one-character function symbol at `(line, col)` in `file`, in the flat
/// shape a server that reports no nesting sends.
pub fn flat_symbol(name: &str, file: &str, line: u32, col: u32) -> SymbolInformation {
    use lsp_types::{Location, Uri};
    use std::str::FromStr;
    #[allow(deprecated)]
    SymbolInformation {
        name: name.to_string(),
        kind: SymbolKind::FUNCTION,
        tags: None,
        deprecated: None,
        location: Location {
            uri: Uri::from_str(&format!("file://{file}")).expect("uri"),
            range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
        },
        container_name: None,
    }
}

/// Two fake servers registered under one language, both advertising `caps`, for
/// a test whose subject is what happens when a buffer has more than one.
pub fn install_two_servers(
    h: &mut TestHarness,
    caps: ServerCapabilities,
) -> (Arc<crate::host::FakeLsp>, Arc<crate::host::FakeLsp>) {
    use crate::lsp::registry::ServerSelector;
    let primary = Arc::new(crate::host::FakeLsp::new());
    primary.set_capabilities(caps.clone());
    let secondary = Arc::new(crate::host::FakeLsp::new());
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
    (primary, secondary)
}

/// A `workspace/didChangeWatchedFiles` registration of one watcher, in the
/// shape a server sends it.
pub fn watched_files_registration(
    id: &str,
    glob_pattern: GlobPattern,
    kind: Option<WatchKind>,
) -> Registration {
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: vec![FileSystemWatcher { glob_pattern, kind }],
    };
    Registration {
        id: id.to_string(),
        method: DidChangeWatchedFiles::METHOD.to_string(),
        register_options: Some(serde_json::to_value(options).expect("watcher options serialize")),
    }
}

/// Deliver `registration` from the harness's server, as its
/// `client/registerCapability` request lands after startup.
pub fn register_capability(h: &mut TestHarness, registration: Registration) {
    h.fake_lsp()
        .push_incoming_request(IncomingRequest::RegisterCapability {
            id: NumberOrString::Number(1),
            params: RegistrationParams {
                registrations: vec![registration],
            },
        });
    drain::drain_lsp_incoming_requests(&mut h.stoat);
}

/// A terminal session over a fake PTY, with a `rows` by `cols` screen.
///
/// The fake comes back beside the session, for a test that reads what the
/// session wrote to its PTY or scripts what the PTY reports.
pub fn fake_term_session(rows: u16, cols: u16) -> (TermSession, Arc<FakeTerminalSession>) {
    let fake = Arc::new(FakeTerminalSession::new());
    let session = TermSession::new(
        TermScreen::new(rows, cols),
        fake.clone(),
        TermSession::next_token(),
    );
    (session, fake)
}

/// Store two tours under `/repo`, with the file their stops read.
///
/// `alpha` has a description and one titled stop. `zebra` has two untitled
/// stops and no description, so the pair covers both preview forms and two
/// stop counts.
pub fn store_two_tours(h: &mut TestHarness) {
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs()
        .insert_file("/repo/a.rs", "fn one() {}\nfn two() {}\n");

    let stop = |line: u32, snippet: &str| Location {
        path: PathBuf::from("a.rs"),
        range: walkthrough::Range {
            start: Point { line, col: 1 },
            end: Point { line, col: 11 },
        },
        snippet: snippet.to_owned(),
    };
    let alpha = {
        let mut tour = Walkthrough::new("alpha".to_owned(), "Alpha tour".to_owned(), None);
        tour.description = "Where it **starts**.".to_owned();
        let focus = stop(1, "fn one() {}");
        tour.add_stop(Some("entry".to_owned()), String::new(), focus, None, None)
            .expect("append");
        tour
    };
    let zebra = {
        let mut tour = Walkthrough::new("zebra".to_owned(), "Zebra tour".to_owned(), None);
        for (line, snippet) in [(1, "fn one() {}"), (2, "fn two() {}")] {
            tour.add_stop(None, String::new(), stop(line, snippet), None, None)
                .expect("append");
        }
        tour
    };

    for tour in [zebra, alpha] {
        h.fake_fs().insert_file(
            format!("/repo/.stoat/walkthroughs/{}.json", tour.slug),
            serde_json::to_string(&tour).expect("serialize"),
        );
    }
}

/// The Islands of a modal plate that `scene` declares, as each one's cells and
/// edges in declaration order.
///
/// An Island is a backdrop panel in the theme's editor surface, which sets it
/// apart from the plate's own panels. Without the plate look there are none.
pub fn plate_islands(theme: &Theme, scene: &ApcScene) -> Vec<(Rect, [command::Edge; 4])> {
    let Some(plate) = crate::render::chrome::plate_colors(theme, scene) else {
        return Vec::new();
    };
    scene
        .panels()
        .filter(|panel| panel.backdrop && panel.fill == Some(plate.surface))
        .map(|panel| {
            let cells = Rect::new(panel.left, panel.top, panel.width, panel.height);
            (cells, panel.edges)
        })
        .collect()
}

/// The hairline bars that `scene` declares down the middle of column `x`, as a
/// separator column's `chrome::vline` draws them.
pub fn column_rules(scene: &ApcScene, x: u16) -> usize {
    let middle = i16::try_from(x).expect("a column on screen") * 16 + 8;
    command::decode_stream(scene.bytes())
        .into_iter()
        .filter(
            |cmd| matches!(cmd, command::Command::Bar(bar) if bar.x == middle && bar.width == 1),
        )
        .count()
}

/// The hairline bars that `scene` declares across the middle of row `y`, as a
/// separator row's `chrome::hline` draws them.
pub fn row_rules(scene: &ApcScene, y: u16) -> usize {
    let middle = i16::try_from(y).expect("a row on screen") * 16 + 8;
    command::decode_stream(scene.bytes())
        .into_iter()
        .filter(
            |cmd| matches!(cmd, command::Command::Bar(bar) if bar.y == middle && bar.height == 1),
        )
        .count()
}

/// The gutter mark of an unstaged row of kind `status`.
pub fn mark(status: DiffHunkStatus) -> GutterMark {
    GutterMark {
        status,
        state: MarkState::Unstaged,
    }
}

pub fn field(state: &StoatKeymapState<'_>, name: &str) -> Option<String> {
    match state.get(name) {
        Some(StateValue::String(s)) => Some(s.to_string()),
        _ => None,
    }
}
pub fn resolved(name: &str, args: Vec<Value>) -> ResolvedAction {
    ResolvedAction {
        name: name.to_string(),
        args: args
            .into_iter()
            .map(|value| ResolvedArg { name: None, value })
            .collect(),
    }
}

pub fn corner_badge(label: &str) -> Badge {
    Badge {
        source: BadgeSource::Walkthrough,
        anchor: Anchor::BottomRight,
        state: BadgeState::Complete,
        label: label.to_owned(),
        detail: None,
    }
}

pub fn action(name: &str) -> ResolvedAction {
    ResolvedAction {
        name: name.to_string(),
        args: Vec::new(),
    }
}

/// The diff editors, cell readers, and RGB diff themes that the tests of `render::review` in
/// The diff editors, cell readers, and RGB diff themes that the tests of `render::review` in
/// `stoat` and in `stoat_tests` share.
pub mod render_review {
    use crate::{
        buffer::{BufferId, TextBuffer},
        diff_map::{DiffHunk, DiffHunkStatus, DiffMap, TokenDetail},
        editor_state::EditorState,
        render::review::{
            fold_row_placeholder, render_diff_view, BufferNumbering, DiffColumns, DiffDials,
            DiffLayout,
        },
        theme::Theme,
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};
    use std::sync::{Arc, RwLock};
    use stoat_language::structural_diff;
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::Point;

    pub fn buffer_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.x + buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The cell columns a diff view paints over `area` for files under a
    /// hundred lines, whose gutters take two digits.
    pub fn two_digit_columns(area: Rect) -> DiffColumns {
        DiffColumns::compute(area, DiffLayout::diff_view(2, None))
    }

    /// The cell columns of a 120-wide diff view of files under a hundred
    /// lines, as the 120-wide renders with no scene lay it out.
    pub fn wide_columns() -> DiffColumns {
        two_digit_columns(Rect::new(0, 0, 120, 8))
    }

    /// The left text cells, up to the mid-divider in the two-column layout.
    pub fn left_text(cols: &DiffColumns) -> std::ops::Range<u16> {
        cols.left_text_x..cols.left_text_x + cols.left_content_w as u16
    }

    /// The right text cells.
    pub fn right_text(cols: &DiffColumns) -> std::ops::Range<u16> {
        cols.right_text_x..cols.right_text_x + cols.right_content_w as u16
    }

    /// The cells the left line number shows in.
    pub fn left_num(cols: &DiffColumns) -> std::ops::Range<u16> {
        number_cells(cols, cols.left_x, cols.left_num_x)
    }

    /// The cells a line number shows in on the side that starts at `side_x`.
    ///
    /// A cell gutter writes the number into its field at `num_x`. A rich
    /// gutter's run ends at its sixteenth offset and composites from the cell
    /// it starts in, so a number of up to two digits lands in the cells before
    /// the offset's own cell. The first of those cells also holds the lanes.
    pub fn number_cells(cols: &DiffColumns, side_x: u16, num_x: u16) -> std::ops::Range<u16> {
        match cols.rich {
            Some(rich) => side_x..side_x + rich.number_right / 16,
            None => num_x..num_x + (cols.status_left_x - cols.left_num_x),
        }
    }

    /// A diff-view editor over `text`, diffed against `base`, with the view and
    /// its deleted-block splicing enabled.
    pub fn diff_editor(base: &str, text: &str) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    /// A diff-view editor over `text` with a hand-built diff map, for hunk shapes
    /// the structural differ will not synthesize from plain text (e.g. moves).
    pub fn diff_editor_with_map(text: &str, dm: DiffMap) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(dm);
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    /// A diff-view editor over the lines `1` to `20`, with one refined
    /// `Modified` hunk over the rows `live`.
    ///
    /// The hunk's base holds `removed` lines named `b<n>` from the hunk's first
    /// line on, and its first and last rows are the marked runs. The rows above
    /// and below the hunk match the base.
    pub fn hunk_editor(live: std::ops::Range<u32>, removed: u32) -> EditorState {
        let text: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let head: String = (1..=live.start).map(|n| format!("{n}\n")).collect();
        let lines: String = (live.start + 1..=live.start + removed)
            .map(|n| format!("b{n}\n"))
            .collect();
        let tail: String = (live.end + 1..=20).map(|n| format!("{n}\n")).collect();
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(live.clone()).collect(),
                marked_rows: vec![live.start..live.start + 1, live.end - 1..live.end],
                relocated_rows: Vec::new(),
                buffer_start_line: live.start,
                buffer_line_range: live,
                base_byte_range: head.len()..head.len() + lines.len() - 1,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans: Vec::new(),
                    base_spans: Vec::new(),
                    tree: false,
                })),
            }],
            Some(Arc::new(format!("{head}{lines}{tail}"))),
        );
        diff_editor_with_map(&text, dm)
    }

    /// A [`hunk_editor`] whose hunk over rows 4 to 15 has ten base lines, with
    /// a fold row over its rows 6 to 13.
    pub fn hunk_fold_row_editor() -> EditorState {
        let mut editor = hunk_editor(4..16, 10);
        editor.display_map.fold_with(vec![(
            Point::new(6, 0)..Point::new(13, 2),
            fold_row_placeholder(8),
        )]);
        editor
    }

    /// `editor` painted in cells as a diff view over `area`, from its scroll row.
    pub fn paint_cells(editor: &mut EditorState, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        render_diff_view(
            editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            BufferNumbering::Absolute,
            &[],
            false,
        );
        buf
    }

    /// A minimal theme whose diff colors resolve to RGB, so the change washes
    /// engage when rendering a hand-built diff editor off the harness.
    pub fn rgb_diff_theme() -> Theme {
        rgb_diff_theme_with("")
    }

    /// [`rgb_diff_theme`] with the settings in `extra` added to its block.
    pub fn rgb_diff_theme_with(extra: &str) -> Theme {
        let src = format!(
            r##"theme rgbtest {{
            diff.context.fg  = "#808080";
            diff.added.fg    = "#00ff00";
            diff.modified.fg = "#ffff00";
            diff.deleted.fg  = "#ff0000";
            diff.moved.fg    = "#0000ff";
            diff.staged.fg   = "#00ffff";
            diff.unstaged.fg = "#ff00ff";
            diff.approved.fg = "#ff8000";
            diff.unapproved.fg = "#8000ff";
            ui.background.bg = "#282c34";
            {extra}
        }}"##
        );
        let (config, _) = stoat_config::parse(&src);
        Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    /// The glyphs of row `y` across `cols`, for locating a rendered line.
    pub fn line_text(buf: &Buffer, y: u16, cols: std::ops::Range<u16>) -> String {
        cols.map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The first row whose glyphs across `cols` start with `needle`.
    pub fn row_holding(buf: &Buffer, cols: std::ops::Range<u16>, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|&y| line_text(buf, y, cols.clone()).starts_with(needle))
            .unwrap_or_else(|| panic!("no row across {cols:?} starts with {needle:?}"))
    }

    /// `a` a's, a space, then `b` b's: a line that wraps after the space when
    /// the a's and the space fit the width and the whole line does not.
    pub fn long_line(a: usize, b: usize) -> String {
        format!("{} {}", "a".repeat(a), "b".repeat(b))
    }

    /// The trimmed glyphs of a continuation row whose text is `text`, which
    /// open with the Wrap Mark and the space after it.
    pub fn continued(text: String) -> String {
        format!("\u{21b3} {text}")
    }
}

/// The span prose that the tests of `workspace::diff` in `stoat` and in `stoat_tests` share.
pub mod workspace_diff {
    use crate::diff_map::{DiffMap, ProseChange};

    /// The prose marks of every span in `dm`, buffer side then base side, with
    /// each run of equal marks merged into one.
    pub fn span_prose(dm: &DiffMap) -> Vec<ProseChange> {
        let mut prose: Vec<ProseChange> = dm
            .hunks()
            .filter_map(|hunk| hunk.token_detail.as_deref())
            .flat_map(|detail| detail.buffer_spans.iter().chain(&detail.base_spans))
            .map(|span| span.prose)
            .collect();
        prose.dedup();
        prose
    }
}

/// The diff maps and scope text that the tests of `diff_changes` in `stoat` and in `stoat_tests`
/// The diff maps and scope text that the tests of `diff_changes` in `stoat` and in `stoat_tests`
/// The diff maps and scope text that the tests of `diff_changes` in `stoat` and in `stoat_tests`
/// share.
pub mod diff_changes {
    use crate::{
        diff_changes::scope_parts, diff_map::DiffMap, display_map::FoldScope,
        theme::FULL_CELL_SCALE,
    };
    use std::sync::Arc;
    use stoat_language::structural_diff;

    /// The map a structural diff of `base` against `text` gives.
    pub fn map_of(base: &str, text: &str) -> DiffMap {
        DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        )
    }

    /// Lines `1` to `n`, one per line.
    pub fn numbered(n: u32) -> String {
        (1..=n).map(|line| format!("{line}\n")).collect()
    }

    /// The text of every run of `scope`, with room for all of it.
    pub fn scope_text(scope: &FoldScope) -> String {
        scope_parts(scope, u16::MAX, FULL_CELL_SCALE)
            .iter()
            .map(|part| part.text.as_str())
            .collect()
    }
}

/// The palette, area, and bar shape that the tests of `render::conflict_view::grid` in `stoat` and
/// The palette and area that the tests of `render::conflict_view::grid` in `stoat` and in
/// `stoat_tests` draw with.
pub mod render_conflict_view_grid {
    use ratatui::{layout::Rect, style::Color};

    pub const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];

    pub const SPECIAL: [u8; 3] = [0xc5, 0x9a, 0xf0];

    pub const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];

    pub const MUTED: [u8; 3] = [0x83, 0x8a, 0x98];

    pub const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];

    pub const DIVIDER: [u8; 3] = [0x22, 0x25, 0x2b];

    pub const CHIP_CONFLICT: [u8; 3] = [0x33, 0x20, 0x1f];

    pub const CHIP_OURS: [u8; 3] = [0x1d, 0x2a, 0x1f];

    pub const CHIP_THEIRS: [u8; 3] = [0x2a, 0x22, 0x36];

    /// The paint area, wide enough that the merged column ends past its chip, and
    /// tall enough to hold the eleven rows of the one-sided edits test under the
    /// two header rows.
    pub const AREA: Rect = Rect::new(0, 0, 158, 13);

    pub fn rgb([r, g, b]: [u8; 3]) -> Color {
        Color::Rgb(r, g, b)
    }
}
