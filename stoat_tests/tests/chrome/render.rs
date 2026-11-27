mod themed_blank_tests {
    use ratatui::style::Color;
    use stoat::app::Stoat;

    /// A cell that reaches the terminal at Reset is resolved against the
    /// hosting terminal's theme rather than stoat's, so the ambient screen
    /// stops tracking `:theme`. The modal case is checked separately because a
    /// modal's blank rows come from its own clear, not from the frame fill.
    #[test]
    fn a_painted_frame_leaves_no_reset_cell() {
        let mut h = Stoat::test();

        let buf = h.render_composited();
        assert!(
            buf.content
                .iter()
                .all(|cell| cell.bg != Color::Reset && cell.fg != Color::Reset),
            "every cell of a plain frame carries the theme's colors",
        );

        h.stoat.key_hints_visible = true;
        let buf = h.render_composited();
        assert!(
            buf.content
                .iter()
                .all(|cell| cell.bg != Color::Reset && cell.fg != Color::Reset),
            "and every cell of a frame with a box open over it",
        );
    }
}

mod pane_badge_tests {
    use ratatui::{layout::Rect, style::Color};
    use stoat::{
        render::{paint, pane_badge_colors, pane_badges, BADGE_CHROME},
        test_harness::TestHarness,
    };
    use stoatty_protocol::{
        command,
        command::{Command, PopoverCommand},
    };

    /// The focused pane has no badge, since the chord starts there, and the
    /// other panes keep the numbers their places give them, so a digit names
    /// the same pane wherever the focus sits.
    #[test]
    fn the_focused_pane_has_no_badge_and_the_others_keep_their_numbers() {
        let mut h = TestHarness::with_size(80, 24);
        h.type_action("SplitRight()");
        h.type_action("SplitRight()");
        let digits_with_focus_on = |h: &mut TestHarness, place: usize| {
            let ws = h.stoat.active_workspace_mut();
            let pane = ws.panes.split_pane_ids()[place];
            ws.panes.set_focus(pane);
            pane_badges(ws)
                .iter()
                .map(|badge| badge.digit)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            [
                digits_with_focus_on(&mut h, 2),
                digits_with_focus_on(&mut h, 1)
            ],
            [vec!['1', '2'], vec!['1', '3']],
        );
    }

    /// A badge paints the digit on the accent fill, so the panes a digit
    /// reaches stand out, and the focused pane carries no badge.
    #[test]
    fn a_badge_paints_the_accent_fill() {
        let mut h = badge_chord(true);
        let buf = h.render_composited();

        let accent = accent_bg(&h);
        let ws = h.stoat.active_workspace();
        let badges = pane_badges(ws);
        let [badge] = badges.as_slice() else {
            panic!("one badge, on the pane without the focus");
        };
        let digit_cell = &buf[(
            badge.rect.x + (badge.rect.width - 1) / 2,
            badge.rect.y + (badge.rect.height - 1) / 2,
        )];
        let focused = ws.panes.pane(ws.panes.focus()).area;
        let focused_has_accent = (focused.y..focused.y + focused.height - 1)
            .flat_map(|y| (focused.x..focused.x + focused.width).map(move |x| (x, y)))
            .any(|cell| Some(buf[cell].bg) == accent);

        assert_eq!(
            (digit_cell.symbol(), Some(digit_cell.bg), focused_has_accent),
            (badge.digit.to_string().as_str(), accent, false),
            "the digit sits on the accent fill, and the focused pane above its status row has none",
        );
    }

    /// A live scene floats the rounded badge alone, so no square cell fill
    /// sits under its corners. The one popover carries the board's chrome and
    /// centers a scale 6 digit.
    #[test]
    fn a_live_scene_paints_no_badge_cells() {
        let mut h = badge_chord(true);
        let buf = h.stoat.render();

        let accent = accent_bg(&h);
        let badges = pane_badges(h.stoat.active_workspace());
        let [badge] = badges.as_slice() else {
            panic!("one badge, on the pane without the focus");
        };
        let sent: Vec<_> = popovers(&h)
            .into_iter()
            .map(|p| (p.fill, p.chrome, p.center, p.scale))
            .collect();

        assert_eq!(
            (
                rect_cells(badge.rect).any(|cell| Some(buf[cell].bg) == accent),
                sent
            ),
            (
                false,
                vec![(
                    paint::style_rgb(accent).expect("an RGB accent"),
                    BADGE_CHROME,
                    true,
                    6
                )]
            ),
            "the grid under the badge keeps its cells, and one popover carries the badge",
        );
    }

    /// A badge's box is 9 by 6 in a pane with room, centered on the pane. A
    /// small pane takes the largest box that fits, three cells wide for each
    /// two rows, and a pane with room for no box gets no badge.
    #[test]
    fn a_badge_is_nine_by_six_in_a_pane_with_room_and_shrinks_in_a_small_one() {
        let mut h = badge_chord(false);
        let mut badge_in = |width: u16, height: u16| {
            let ws = h.stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let unfocused = ws
                .panes
                .split_pane_ids()
                .into_iter()
                .find(|&pane| pane != focused)
                .expect("a pane without the focus");
            ws.panes.pane_mut(unfocused).area = Rect::new(2, 3, width, height);
            pane_badges(ws)
                .first()
                .map(|badge| (badge.rect, badge.scale))
        };

        assert_eq!(
            [
                badge_in(40, 23),
                badge_in(5, 3),
                badge_in(3, 1),
                badge_in(1, 1)
            ],
            [
                Some((Rect::new(17, 11, 9, 6), 6)),
                Some((Rect::new(2, 3, 5, 3), 3)),
                Some((Rect::new(2, 3, 2, 1), 1)),
                None,
            ],
        );
    }

    /// A dead scene draws the cell box alone, since no host floats a popover.
    #[test]
    fn a_dead_scene_paints_the_badge_cells() {
        let mut h = badge_chord(false);
        let buf = h.stoat.render();

        let accent = accent_bg(&h);
        let badges = pane_badges(h.stoat.active_workspace());
        let [badge] = badges.as_slice() else {
            panic!("one badge, on the pane without the focus");
        };

        assert_eq!(
            (
                rect_cells(badge.rect).all(|cell| Some(buf[cell].bg) == accent),
                popovers(&h).len()
            ),
            (true, 0),
            "every cell of the badge box takes the fill, and no popover goes out",
        );
    }

    /// Two side-by-side panes in the pane picker chord, rendered for a host
    /// that floats popovers when `stoatty` is set.
    fn badge_chord(stoatty: bool) -> TestHarness {
        let mut h = TestHarness::with_size(80, 24);
        h.type_action("SplitRight()");
        h.stoat.stoatty = stoatty;
        h.stoat.set_focused_mode("space_pane_display".into());
        h
    }

    /// The fill a badge takes under the theme the harness loads.
    fn accent_bg(h: &TestHarness) -> Option<Color> {
        let [r, g, b] = pane_badge_colors(&h.stoat.theme).0;
        Some(Color::Rgb(r, g, b))
    }

    /// The popovers the last frame sent to the host.
    fn popovers(h: &TestHarness) -> Vec<PopoverCommand> {
        command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Popover(popover) => Some(popover),
                _ => None,
            })
            .collect()
    }

    /// Every cell position inside `rect`, row by row.
    fn rect_cells(rect: Rect) -> impl Iterator<Item = (u16, u16)> {
        (rect.y..rect.bottom()).flat_map(move |y| (rect.x..rect.right()).map(move |x| (x, y)))
    }
}

mod minimap_chrome_tests {
    use stoat::{render::paint::style_rgb, test_harness::TestHarness, theme::scope};
    use stoatty_protocol::{command, command::Command};

    /// A pane's minimap strip is seven cells wide at six lines a cell, and its
    /// viewport thumb washes the focus accent at eighteen percent with an
    /// outline at 90 percent.
    #[test]
    fn the_strip_is_seven_cells_with_an_accent_thumb() {
        let mut h = TestHarness::with_size(120, 24);
        h.seed_focused_buffer(&"line\n".repeat(60));
        h.stoat.render();

        let strip = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .find_map(|cmd| match cmd {
                Command::Minimap(strip) => Some(strip),
                _ => None,
            })
            .expect("the pane declares a strip");
        let [r, g, b] = style_rgb(h.stoat.theme.get(scope::UI_BORDER_FOCUSED).fg)
            .expect("the theme's focus accent is RGB");
        assert_eq!(
            (
                strip.width,
                strip.lines_per_cell,
                strip.thumb,
                strip.thumb_border
            ),
            (7, 6, [r, g, b, 46], Some([r, g, b, 230]))
        );
    }
}

mod modal_dim_tests {
    use ratatui::style::Color;
    use stoat::{
        render::paint::{dim_rgb, style_rgb},
        test_fixture::open_scratch_file,
        test_harness::TestHarness,
        theme::scope,
    };
    use stoat_config::MinimapMode;
    use stoatty_protocol::{command, command::Command};

    /// A modal that paints its own box fades every pane behind it by
    /// `ui.modal_dim`, the unfocused one past its own lighter fade. The search
    /// prompt paints no box, so the panes keep their fades.
    #[test]
    fn a_boxed_modal_fades_every_pane_behind_it() {
        let mut h = TestHarness::with_size(80, 24);
        let _path = open_scratch_file(&mut h, "abcdef\n");
        h.type_action("SplitRight()");
        h.stoat.settings.ui_inactive_dim = Some(0.1);
        h.stoat.settings.ui_modal_dim = Some(0.65);
        let glyphs = |h: &mut TestHarness| -> Vec<Color> {
            let buf = h.stoat.render();
            (0..80)
                .filter(|&x| buf[(x, 0)].symbol() == "c")
                .map(|x| buf[(x, 0)].fg)
                .collect()
        };

        let plain = glyphs(&mut h);
        h.type_keys("/");
        let searching = glyphs(&mut h);
        h.type_keys("escape");
        h.type_action("SwitchWorkspace()");
        let boxed = glyphs(&mut h);

        let bg = style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg).expect("an RGB ground");
        let Some(&Color::Rgb(r, g, b)) = plain.last() else {
            panic!("the focused pane paints its text in RGB, not {plain:?}");
        };
        let fade = |amount: f32| {
            let [r, g, b] = dim_rgb([r, g, b], bg, amount);
            Color::Rgb(r, g, b)
        };
        assert_eq!(
            [plain, searching, boxed],
            [
                vec![fade(0.1), fade(0.0)],
                vec![fade(0.1), fade(0.0)],
                vec![fade(0.65), fade(0.65)],
            ],
            "the unfocused left pane, then the focused right one"
        );
    }

    /// `ui.modal_dim` clamps to `0.0..=1.0`, and a setting left unset fades no
    /// pane.
    #[test]
    fn the_modal_fade_clamps_its_setting_and_needs_it_set() {
        let mut h = TestHarness::with_size(80, 24);
        h.type_action("SwitchWorkspace()");
        let fades = [None, Some(2.0), Some(-1.0)].map(|setting| {
            h.stoat.settings.ui_modal_dim = setting;
            h.stoat.modal_dim()
        });
        assert_eq!(fades, [0.0, 1.0, 0.0]);
    }

    /// A per-pane minimap strip fades with its pane under a boxed modal, its
    /// thumb and its palette alike.
    #[test]
    fn a_pane_strip_fades_with_its_pane_under_a_boxed_modal() {
        let mut h = TestHarness::with_size(120, 24);
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.stoat.settings.ui_modal_dim = Some(0.65);
        h.seed_focused_buffer(&"line\n".repeat(60));
        let strip = |h: &mut TestHarness| {
            h.stoat.render();
            command::decode_stream(&h.stoat.apc_scene.all_bytes())
                .into_iter()
                .find_map(|cmd| match cmd {
                    Command::Minimap(strip) => Some(strip),
                    _ => None,
                })
                .expect("the pane declares a strip")
        };

        let plain = strip(&mut h);
        h.type_action("SwitchWorkspace()");
        let faded = strip(&mut h);

        let bg = style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg).expect("an RGB ground");
        let fade = |color: [u8; 3]| dim_rgb(color, bg, 0.65);
        let [r, g, b, alpha] = plain.thumb;
        let [r, g, b] = fade([r, g, b]);
        let palette: Vec<[u8; 3]> = plain.palette.iter().map(|&color| fade(color)).collect();
        assert_eq!((faded.thumb, faded.palette), ([r, g, b, alpha], palette));
    }
}

mod dispatch_tests {
    use stoat::{
        action_handlers::dispatch,
        app::Stoat,
        keymap_state::{modal_predicate, Flags, FocusFlags},
        location_picker::LocationEntry,
        render::{hints::FooterKeys, hints_cache_key},
    };
    use stoat_action::Action;

    /// The `modal` a frame painted, read back off the hints cache the painting arm
    /// populated.
    fn painted_modal_hints(h: &stoat::test_harness::TestHarness) -> Option<u64> {
        h.stoat.hints_cache.as_ref().map(|c| c.key)
    }

    fn modal_hints_key(mode: &str, modal: &str, scope: Option<&str>) -> u64 {
        hints_cache_key(
            mode,
            None,
            None,
            &Flags::default(),
            None,
            &FocusFlags::default(),
            Some(modal),
            scope,
            None,
            false,
        )
    }

    /// A modal that ends in its own key line paints no hints box until the
    /// hints toggle asks for one. The jumplist's key line counts too.
    #[test]
    fn a_footed_modal_paints_its_hints_box_on_request() {
        let paints_box = |open: &dyn Action, modal: &str, scope: Option<&str>, toggle| {
            let mut h = Stoat::test();
            h.seed_focused_buffer("alpha\n");
            dispatch(&mut h.stoat, &stoat_action::SaveSelection);
            h.stoat.key_hints_visible = toggle;
            dispatch(&mut h.stoat, open);
            h.render_composited();
            let mode = h.stoat.focused_mode().to_string();
            painted_modal_hints(&h) == Some(modal_hints_key(&mode, modal, scope))
        };
        let modals = [
            (
                &stoat_action::OpenFileFinder as &dyn Action,
                "finder",
                Some("all"),
            ),
            (&stoat_action::OpenHelp, "help", None),
            (&stoat_action::SwitchWorkspace, "workspace_picker", None),
            (&stoat_action::OpenCommandPalette, "palette", None),
            (&stoat_action::OpenJumplistPicker, "jumplist", None),
        ];

        let shown: Vec<(bool, bool)> = modals
            .iter()
            .map(|&(action, modal, scope)| {
                (
                    paints_box(action, modal, scope, false),
                    paints_box(action, modal, scope, true),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [(false, true); 5],
            "the box unasked and on the toggle, per modal"
        );
    }

    /// A modal box names a footer key only where the key does what the footer
    /// says. The quit confirm shows its box unasked, so the toggle does not
    /// hide it, and the jumplist's and the file finder's boxes wait for the
    /// toggle, since both end in a key line, so they name the toggle's key.
    /// None names a help key, because the modal takes `?` first.
    #[test]
    fn a_modal_box_names_only_the_footer_keys_that_work() {
        let footer_keys = |open: &dyn Action| {
            let mut h = Stoat::test();
            h.seed_focused_buffer("alpha\n");
            dispatch(&mut h.stoat, &stoat_action::SaveSelection);
            h.stoat.key_hints_visible = true;
            dispatch(&mut h.stoat, open);
            h.render_composited();
            h.stoat
                .hints_cache
                .as_ref()
                .map(|cache| cache.footer_keys.clone())
        };

        let toggled = Some(FooterKeys {
            hide: Some("C-?".to_string()),
            help: None,
        });
        assert_eq!(
            [
                footer_keys(&stoat_action::QuitAll),
                footer_keys(&stoat_action::OpenJumplistPicker),
                footer_keys(&stoat_action::OpenFileFinder),
            ],
            [Some(FooterKeys::default()), toggled.clone(), toggled],
        );
    }

    /// The frame used to walk the modal fields in its own order, so a second modal
    /// opened over a first could be the one painted while keys still routed to the
    /// other. Both now derive from the same authority.
    #[test]
    fn the_painted_modal_is_the_one_keys_route_to() {
        let mut h = Stoat::test();
        // The finder paints its hints box on the hints toggle alone.
        h.stoat.key_hints_visible = true;
        dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.render_composited();
        let mode = h.stoat.focused_mode().to_string();
        assert_eq!(
            painted_modal_hints(&h),
            Some(modal_hints_key(&mode, "finder", Some("all"))),
            "the finder alone paints the finder"
        );

        h.stoat.location_picker = Some(stoat::action_handlers::lsp::open_location_picker(
            &mut h.stoat,
            vec![LocationEntry {
                path: std::path::PathBuf::from("/repo/a.rs"),
                offset: 0,
                line: 1,
                column: 1,
                text: "candidate".to_owned(),
                block: None,
            }],
        ));
        h.render_composited();

        assert_eq!(
            modal_predicate(&h.stoat),
            Some("location"),
            "the location picker outranks the finder for keys"
        );
        let mode = h.stoat.focused_mode().to_string();
        assert_eq!(
            painted_modal_hints(&h),
            Some(modal_hints_key(&mode, "location", None)),
            "and so it is what the frame paints"
        );
    }
}

mod lsp_filter_tests {
    use crossterm::event::{Event, KeyCode};
    use std::{path::Path, sync::Arc};
    use stoat::{
        action_handlers::{textobject::TextobjectMode, yank},
        app::UpdateEffect,
        lsp::LspSymbolKind,
        render::hints::FooterKeys,
        test_harness::{keys, TestHarness},
    };
    use stoat_action::registry;

    /// Render one frame and flatten the painted cells into searchable text.
    fn box_text(h: &mut TestHarness) -> String {
        let buf = h.render_composited();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Open "Foo bar" and return its buffer id and focused-editor workspace.
    fn open_foo_bar(h: &mut TestHarness) -> stoat::buffer::BufferId {
        let root = std::path::PathBuf::from("/lsp");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"Foo bar");
        h.stoat.active_workspace_mut().git_root = root;
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        h.settle();
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            stoat::pane::View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    }

    /// The text a hints row shows for the registered action `name`.
    fn short_desc(name: &str) -> &'static str {
        registry::lookup(name)
            .unwrap_or_else(|| panic!("{name} is registered"))
            .def
            .short_desc()
    }

    #[test]
    fn space_lsp_box_filters_rows_by_cursor_symbol_kind() {
        let mut h = TestHarness::with_size(150, 50);
        let buffer_id = open_foo_bar(&mut h);

        {
            let ws = h.stoat.active_workspace_mut();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let snapshot = buffer.read().unwrap().snapshot.clone();
            let start = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Right)[0];
            let end = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Left)[0];
            let kinds = Arc::from(vec![
                (start(0usize)..end(3usize), LspSymbolKind::Trait),
                (start(4usize)..end(7usize), LspSymbolKind::Function),
            ]);
            ws.buffers.store_lsp_symbol_kinds(buffer_id, kinds);
        }

        let goto_rows_list = |text: &str| {
            text.contains(short_desc("GotoTypeDefinition"))
                && text.contains(short_desc("GotoImplementation"))
        };

        // The cursor starts on "Foo", the trait.
        h.type_keys("space l");
        let over_trait = box_text(&mut h);
        assert!(
            over_trait.contains("implementor of the trait"),
            "the implementors row shows over a trait"
        );
        assert!(
            !over_trait.contains("caller of the symbol"),
            "the caller row is hidden over a trait"
        );
        assert!(
            goto_rows_list(&over_trait),
            "the Helix goto rows list over every token kind"
        );

        // Move onto "bar", the function.
        h.type_keys("escape");
        h.type_keys("l l l l");
        h.type_keys("space l");
        let over_function = box_text(&mut h);
        assert!(
            over_function.contains("caller of the symbol"),
            "the caller row shows over a function"
        );
        assert!(
            !over_function.contains("implementor of the trait"),
            "the implementors row is hidden over a function"
        );
        assert!(
            goto_rows_list(&over_function),
            "the Helix goto rows list over every token kind"
        );
    }

    /// The normal-mode box, forced open, lists the chords that lead on from
    /// here, not the whole editor keymap that the help screen holds.
    #[test]
    fn the_forced_normal_box_lists_chords_not_the_editor_keys() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.stoat.key_hints_visible = true;

        let text = box_text(&mut h);
        let move_down = registry::lookup("MoveDown")
            .expect("MoveDown is registered")
            .def
            .short_desc();
        assert_eq!(
            (
                text.contains("+space"),
                text.contains("+goto"),
                text.contains(move_down),
            ),
            (true, true, false),
            "the box lists the chord entries and leaves out the editor keys:\n{text}",
        );
    }

    #[test]
    fn the_forced_normal_box_lists_the_editor_pane_keys() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.stoat.key_hints_visible = true;

        let text = box_text(&mut h);
        assert_eq!(
            (
                text.contains(short_desc("ToggleComments")),
                text.contains(short_desc("RunInterrupt")),
            ),
            (true, false),
            "the box lists the editor pane's Ctrl-c and not the run pane's:\n{text}",
        );
    }

    #[test]
    fn the_forced_insert_box_over_a_run_pane_lists_the_run_keys() {
        let mut h = TestHarness::with_size(150, 50);
        h.open_run();
        h.stoat.key_hints_visible = true;

        let text = box_text(&mut h);
        let listed = [
            "RunSubmit",
            "RunHistoryPrev",
            "RunHistoryNext",
            "RunInterrupt",
        ]
        .map(|name| text.contains(short_desc(name)));
        assert_eq!(
            listed, [true; 4],
            "the box lists every run pane key:\n{text}"
        );
    }

    /// A run pane and a label pane in normal mode give the box the same mode,
    /// view, token, and focus flags. Only the pane kind tells their lists apart.
    #[test]
    fn the_box_follows_focus_to_a_pane_of_another_kind() {
        let mut h = TestHarness::with_size(150, 50);
        h.open_run();
        h.type_keys("escape");
        h.stoat.key_hints_visible = true;
        let over_run = box_text(&mut h);

        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).view = stoat::pane::View::Label("label".into());
        let over_label = box_text(&mut h);

        let interrupt = short_desc("RunInterrupt");
        assert_eq!(
            (over_run.contains(interrupt), over_label.contains(interrupt)),
            (true, false),
            "the run pane's Ctrl-c leaves the box with the run pane:\n{over_label}",
        );
    }

    /// The select, goto, view, bracket, and match chords act on an editor, and
    /// so do the space menus' position save, clipboard, buffer close, and symbol
    /// hops. A run pane lists none of them.
    #[test]
    fn the_editor_keys_list_only_over_an_editor() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.key_hints_visible = true;
        let chords = [
            "select mode",
            "+goto",
            "+z",
            "+bracket next",
            "+bracket prev",
            "+match",
        ];
        let menu_rows = [
            ("space", "save selection"),
            ("space", "+clipboard"),
            ("space b", short_desc("CloseBuffer")),
            ("space l", short_desc("GotoReferences")),
            ("space l", short_desc("GotoImplementors")),
            ("space l", short_desc("GotoCaller")),
            ("space l", short_desc("GotoCallee")),
        ];
        let listed = |h: &mut TestHarness| {
            let normal = box_text(h);
            let menus = menu_rows.map(|(keys, row)| chord_rows(h, keys).iter().any(|r| r == row));
            (chords.map(|row| normal.contains(row)), menus)
        };
        let over_editor = listed(&mut h);

        h.open_run();
        h.type_keys("escape");
        assert_eq!(
            [over_editor, listed(&mut h)],
            [([true; 6], [true; 7]), ([false; 6], [false; 7])],
            "a scratch editor, then a run pane",
        );
    }

    /// The buffer close refuses unsaved edits, so `space b` lists it only while
    /// the focused buffer is clean.
    #[test]
    fn the_buffer_box_lists_the_close_only_for_a_clean_buffer() {
        let mut h = TestHarness::with_size(150, 50);
        let close = short_desc("CloseBuffer");
        let lists_close = |rows: Vec<String>| rows.iter().any(|row| row == close);
        let clean = lists_close(chord_rows(&mut h, "space b"));

        h.type_keys("i x escape");
        assert_eq!(
            (clean, lists_close(chord_rows(&mut h, "space b"))),
            (true, false),
            "a clean scratch buffer, then one with an unsaved edit",
        );
    }

    #[test]
    fn the_git_chord_box_lists_the_pair_diff_only_with_a_pair() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.set_focused_mode("space_git".into());
        let alone = box_text(&mut h);
        h.stoat.set_focused_mode("normal".into());

        h.open_side_by_side(("/pair/a.txt", "a\n"), ("/pair/b.txt", "b\n"));
        h.stoat.set_focused_mode("space_git".into());
        let paired = box_text(&mut h);

        let row = short_desc("DiffPair");
        assert_eq!(
            (alone.contains(row), paired.contains(row)),
            (false, true),
            "the pair diff row follows the layout:\n{paired}",
        );
    }

    /// The space box lists the keys the space mode binds, `?` among them, and
    /// leaves out the root keys every mode answers and the Escape that leaves
    /// the mode. A submenu row reads `+name`.
    #[test]
    fn a_chord_mode_box_omits_root_keys() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("space");
        h.render_composited();
        let rows: Vec<(String, String)> = h
            .stoat
            .hints_cache
            .as_ref()
            .expect("the space box laid out its rows")
            .rows
            .iter()
            .map(|row| (row.keys.clone(), row.action.clone()))
            .collect();
        let lists_key = |key: &str| {
            rows.iter()
                .any(|(keys, _)| keys.split(", ").any(|listed| listed == key))
        };

        assert_eq!(
            (
                ["C-?", "C-:", "C-;", "C-+", "C-_", "Esc"].map(lists_key),
                lists_key("?"),
                rows.contains(&("b".to_string(), "+buffer".to_string())),
            ),
            ([false; 6], true, true),
            "the box rows: {rows:?}",
        );
    }

    /// The `space d` box over the conflict view lists the chord's keys, as it
    /// does over the two diff views. The chunk footer stays on the conflict
    /// screen's own box, so the chord's frame shows the chunk in the pane's
    /// status row alone.
    #[test]
    fn a_chord_over_the_conflict_view_lists_the_chord_s_keys() {
        let mut h = TestHarness::with_size(150, 50);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root.clone()).conflicted_file(
            "f.txt",
            Some("base\n"),
            Some("ours\n"),
            Some("theirs\n"),
        );
        // The root resolved as no repository before the seed, and that miss
        // holds until REPO_RETRY passes. The diff chord binds only in a
        // repository.
        h.stoat.active_workspace_mut().repo_root = Some(git_root);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::Conflict);
        h.settle();
        let own_box_chunks = box_text(&mut h).matches("chunk 1 / 1").count();

        h.type_keys("space d");
        let chord_chunks = box_text(&mut h).matches("chunk 1 / 1").count();
        let keys: Vec<String> = h
            .stoat
            .hints_cache
            .as_ref()
            .expect("the space d box laid out its rows")
            .rows
            .iter()
            .map(|row| row.keys.clone())
            .collect();
        let lists_key = |key: &str| {
            keys.iter()
                .any(|keys| keys.split(", ").any(|listed| listed == key))
        };

        assert_eq!(
            (["f", "c", "x"].map(lists_key), own_box_chunks, chord_chunks),
            ([true; 3], 2, 1),
            "the box rows: {keys:?}",
        );
    }

    /// The text of each row the box of the chord `keys` lists.
    fn chord_rows(h: &mut TestHarness, keys: &str) -> Vec<String> {
        h.type_keys(keys);
        h.render_composited();
        let rows = h
            .stoat
            .hints_cache
            .as_ref()
            .unwrap_or_else(|| panic!("{keys} laid out its box"))
            .rows
            .iter()
            .map(|row| row.action.clone())
            .collect();
        h.type_keys("escape");
        rows
    }

    #[test]
    fn the_git_chord_box_lists_git_rows_only_inside_a_repository() {
        let mut h = TestHarness::with_size(150, 50);
        let scratch = chord_rows(&mut h, "space h");

        h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
        h.type_keys("escape");
        let staged = chord_rows(&mut h, "space h");

        let listed = |rows: &[String], names: &[&str]| -> Vec<bool> {
            names
                .iter()
                .map(|name| rows.iter().any(|row| row == short_desc(name)))
                .collect()
        };
        assert_eq!(
            (
                listed(
                    &scratch,
                    &["ForceReloadAll", "LiveReload", "ChangeWalkWrap"]
                ),
                listed(&scratch, &["GitLs", "ToggleStageHunk", "GotoNextChange"]),
                listed(&staged, &["GitLs"]),
            ),
            (vec![true; 3], vec![false; 3], vec![true]),
            "a scratch root lists only the ungated rows, and a repository brings GitLs back",
        );
    }

    #[test]
    fn the_git_chord_box_lists_its_pin() {
        let mut h = TestHarness::with_size(150, 50);
        let pin = short_desc("PinMode");
        assert!(
            chord_rows(&mut h, "space h").iter().any(|row| row == pin),
            "the space h box lists the ~ pin",
        );
    }

    /// The chord waits on one char in a primary mode, so its box lists the
    /// chars the chord takes in place of the keymap's rows.
    #[test]
    fn the_textobject_chord_box_lists_its_chars() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "m i");
        h.type_keys("m i");
        let text = box_text(&mut h);

        let labels = [
            "word",
            "WORD",
            "paragraph",
            "function",
            "class",
            "parameter",
            "comment",
            "test",
            "entry",
            "xml element",
            "change",
            "closest pair",
            "pair",
            "cancel",
        ];
        assert_eq!(
            (rows, text.contains("inside")),
            (labels.map(String::from).to_vec(), true),
            "the box lists every type, the pair chars on one row, under its title",
        );
    }

    #[test]
    fn the_textobject_chord_box_leaves_with_the_chord() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("m a");
        let armed = box_text(&mut h);
        h.type_keys("w");
        let resolved = box_text(&mut h);

        assert_eq!(
            (
                armed.contains("around"),
                resolved.contains("around"),
                resolved.contains("paragraph"),
            ),
            (true, false, false),
            "the box shows while the chord waits and leaves with its char",
        );
    }

    /// The forced box caches the keymap's rows under the same primary mode the
    /// chord waits in, so the chord's part of the cache key is what brings its
    /// table in. The chord arms by hand, since a typed `m` repaints the match
    /// chord's box between the two frames.
    #[test]
    fn the_textobject_chord_box_replaces_the_forced_box() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.key_hints_visible = true;
        h.render_composited();
        h.stoat.pending_textobject_select = Some((TextobjectMode::Inner, 1));
        h.render_composited();

        let first = h
            .stoat
            .hints_cache
            .as_ref()
            .and_then(|cache| cache.rows.first())
            .map(|row| row.action.as_str());
        assert_eq!(first, Some("word"), "the chord's table takes the box");
    }

    #[test]
    fn the_surround_add_chord_box_lists_the_pair_chars() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.type_keys("w");
        let rows = chord_rows(&mut h, "m s");
        h.type_keys("m s");
        let text = box_text(&mut h);

        let labels = [
            "pair",
            "quote",
            "own line",
            "that char both sides",
            "cancel",
        ];
        assert_eq!(
            (rows, text.contains("surround")),
            (labels.map(String::from).to_vec(), true),
            "the brackets and the quotes fold into a row each, under the box title",
        );
    }

    #[test]
    fn the_surround_replace_chord_box_follows_its_two_stages() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = (chord_rows(&mut h, "m r"), chord_rows(&mut h, "m r ("));
        h.type_keys("m r");
        let from = box_text(&mut h);
        h.type_keys("(");
        let to = box_text(&mut h);
        h.type_keys("[");
        let done = box_text(&mut h);

        let titles = |text: &str| (text.contains("replace pair"), text.contains("replace with"));
        let from_rows = ["closest pair", "pair", "cancel"];
        let to_rows = ["pair", "quote", "that char both sides", "cancel"];
        assert_eq!(
            (rows, titles(&from), titles(&to), titles(&done)),
            (
                (
                    from_rows.map(String::from).to_vec(),
                    to_rows.map(String::from).to_vec(),
                ),
                (true, false),
                (false, true),
                (false, false),
            ),
            "each stage shows its own box, and the box leaves with the second char",
        );
    }

    #[test]
    fn the_surround_delete_chord_box_lists_the_pair_chars() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "m d");
        h.type_keys("m d");
        let text = box_text(&mut h);

        let labels = ["closest pair", "pair", "cancel"];
        assert_eq!(
            (rows, text.contains("delete pair")),
            (labels.map(String::from).to_vec(), true),
            "the pair chars fold into one row under the box title",
        );
    }

    #[test]
    fn the_replace_chord_box_lists_its_keys() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "r");
        h.type_keys("r");
        let text = box_text(&mut h);

        let labels = [
            "replace each selected char",
            "replace with newline",
            "replace with tab",
            "cancel",
        ];
        assert_eq!(
            (rows, has_box_titled(&text, "replace with char")),
            (labels.map(String::from).to_vec(), true),
            "the box lists a char, Enter, Tab, and Esc under its title",
        );
    }

    #[test]
    fn the_find_chord_box_names_its_direction() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "f");
        h.type_keys("f");
        let find = box_text(&mut h);
        h.type_keys("escape T");
        let till = box_text(&mut h);

        let labels = ["that char", "line ending", "tab", "cancel"];
        assert_eq!(
            (
                rows,
                find.contains("find next"),
                till.contains("till prev"),
                till.contains("find next"),
            ),
            (labels.map(String::from).to_vec(), true, true, false),
            "each find kind titles the box with its own direction",
        );
    }

    /// Whether `text` holds the top border of a box titled `title`, which a
    /// row label holding the same word never matches.
    fn has_box_titled(text: &str, title: &str) -> bool {
        text.contains(&format!("\u{250c} {title} \u{2500}"))
    }

    /// The top and footer rows of the box titled `title`, each from the box's
    /// left edge on, with every run of blanks or of `─` read as one space.
    fn box_edges(h: &mut TestHarness, title: &str) -> Option<[String; 2]> {
        let text = box_text(h);
        let lines: Vec<&str> = text.lines().collect();
        let corner = format!("\u{250c} {title} ");
        let (top, col) = lines.iter().enumerate().find_map(|(y, line)| {
            let at = line.find(&corner)?;
            Some((y, line[..at].chars().count()))
        })?;
        let bottom = (top..lines.len()).find(|&y| lines[y].chars().nth(col) == Some('\u{2514}'))?;

        let words = |y: usize| {
            lines[y]
                .chars()
                .skip(col)
                .collect::<String>()
                .split(|c: char| c.is_whitespace() || c == '\u{2500}')
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        };
        Some([words(top), words(bottom - 1)])
    }

    /// The diff view keeps its box up with the toggle off, and the toggle hides
    /// the box and brings it back. The flag that forces a box on elsewhere
    /// shows none here. The box names the view and says that it stays, and its
    /// footer reads the hunk under the cursor, `-` above the first one. A chord
    /// over the view shows its own box, whose footer names no hunk.
    #[test]
    fn the_diff_view_keeps_its_box_up_with_the_hunk_position() {
        let mut h = TestHarness::with_size(150, 50);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        stoat::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .set_diff_view(true);
        let above = box_edges(&mut h, "diff");
        h.type_keys("j j j");
        let on = box_edges(&mut h, "diff");
        let toggle = |h: &mut TestHarness| {
            stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
            box_edges(h, "diff")
        };
        let hidden = toggle(&mut h);
        h.stoat.key_hints_visible = true;
        let forced = box_edges(&mut h, "diff");
        let shown = toggle(&mut h);
        h.type_keys("space");
        let chord_footer = box_edges(&mut h, "space").map(|[_, footer]| footer);

        let edges = |at: &str| {
            Some([
                "\u{250c} diff stays up on this view \u{2510}".to_string(),
                format!("\u{2502} hunk {at}/1 C-? hide \u{b7} ? help \u{2502}"),
            ])
        };
        assert_eq!(
            (above, on, hidden, forced, shown, chord_footer),
            (
                edges("-"),
                edges("1"),
                None,
                None,
                edges("1"),
                Some("\u{2502} Esc ? help \u{2502}".to_string()),
            ),
        );
    }

    /// The changes view keeps the diff view's box up with the toggle off, and
    /// the toggle hides it. The footer reads the hunk under the cursor in the
    /// focused excerpt, `-` above the first one.
    #[test]
    fn the_changes_view_keeps_the_diff_box_up_with_the_hunk_position() {
        let mut h = TestHarness::with_size(150, 50);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle_diff_jobs();
        let above = box_edges(&mut h, "diff");
        h.type_keys("j j j");
        let on = box_edges(&mut h, "diff");
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
        let hidden = box_edges(&mut h, "diff");

        let edges = |at: &str| {
            Some([
                "\u{250c} diff stays up on this view \u{2510}".to_string(),
                format!("\u{2502} hunk {at}/1 C-? hide \u{b7} ? help \u{2502}"),
            ])
        };
        assert_eq!(
            (h.stoat.current_view(), above, on, hidden),
            (Some("diff_changes"), edges("-"), edges("1"), None),
        );
    }

    /// The diff views' own box lists the keys their view blocks bind, with a
    /// short label on each row, and none of the editor's other context keys.
    /// Keys with one label share a row. A wheel notch is no key to press, so
    /// the Alt notches stay off the box.
    #[test]
    fn the_diff_box_lists_the_view_s_own_keys() {
        let box_rows = |h: &mut TestHarness| {
            h.render_composited();
            h.stoat
                .hints_cache
                .as_ref()
                .expect("the view keeps its box up")
                .rows
                .iter()
                .map(|row| [row.keys.clone(), row.action.clone()])
                .collect::<Vec<_>>()
        };
        let mut h = TestHarness::with_size(150, 50);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        stoat::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .set_diff_view(true);
        let split = box_rows(&mut h);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle_diff_jobs();
        let changes = box_rows(&mut h);

        let rows = |groups: &[&[[&str; 2]]]| -> Vec<[String; 2]> {
            groups
                .concat()
                .into_iter()
                .map(|row| row.map(str::to_owned))
                .collect()
        };
        let dials = [
            ["C-6", "underline changes"],
            ["C-7", "bold changes"],
            ["C-8", "syntax colors"],
            ["C-9", "tint down"],
            ["C-0", "tint up"],
        ];
        let walk = [["MBk", "next change"], ["MFw", "prev change"]];
        let hunks = [["S-MBk", "next hunk"], ["S-MFw", "prev hunk"]];
        assert_eq!(
            (split, changes),
            (
                rows(&[
                    &dials[..],
                    &walk[..],
                    &hunks[..],
                    &[["C-t", "files / changes"]],
                ]),
                rows(&[
                    &dials[..],
                    &[
                        ["q", "close"],
                        ["C-t", "files / changes"],
                        ["Ret", "open"],
                        ["j, Dn", "next row"],
                        ["k, Up", "prev row"],
                        ["C-f, PgDn", "page down"],
                        ["C-b, PgUp", "page up"],
                        ["C-d", "half page down"],
                        ["C-u", "half page up"],
                    ],
                    &walk[..],
                    &hunks[..],
                    &[[":", "commands"]],
                ]),
            ),
        );
    }

    /// The conflict screen's box footer names the chunk at the cursor, `-`
    /// on a line outside every chunk.
    #[test]
    fn the_conflict_box_footer_names_the_chunk_at_the_cursor() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.active_workspace_mut().git_root = std::path::PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("a\nb\nc\nd\ne\n"),
                Some("a\nB\nc\nD\ne\n"),
                Some("a\nX\nc\nY\ne\n"),
            );
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::Conflict);
        h.settle();
        let footer = |h: &mut TestHarness| box_edges(h, "conflict").map(|[_, footer]| footer);

        let first = footer(&mut h);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ConflictNextChunk);
        h.settle();
        let second = footer(&mut h);
        h.type_keys("g g");
        let outside = footer(&mut h);

        assert_eq!(
            [first, second, outside],
            ["1", "2", "-"].map(|at| Some(format!(
                "\u{2502} chunk {at} / 2 C-? hide \u{b7} ? help \u{2502}"
            ))),
        );
    }

    /// A box's footer names a key only where the key does what the footer
    /// says. An armed chord takes both keys as its char, so its box ends at
    /// its last row. A chord mode's box shows whatever the toggle holds, so it
    /// names no hide key, and its help key is the chord's own way to the help.
    /// A pin leaves the box to the toggle, so a pinned chord's box names the
    /// hide key again.
    #[test]
    fn a_hints_footer_names_only_the_keys_that_work() {
        let mut h = TestHarness::with_size(150, 50);
        let footer = |h: &mut TestHarness| {
            h.render_composited();
            h.stoat
                .hints_cache
                .as_ref()
                .expect("a box laid out")
                .footer_keys
                .clone()
        };
        h.type_keys("r");
        let chord = (footer(&mut h), box_edges(&mut h, "replace with char"));
        h.type_keys("escape space");
        let space = footer(&mut h);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::PinMode);
        h.stoat.key_hints_visible = true;
        let pinned = footer(&mut h);
        h.type_keys("escape z");
        let help_chord = footer(&mut h);
        h.type_keys("escape");
        let forced = footer(&mut h);

        let keys = |hide: Option<&str>, help: Option<&str>| FooterKeys {
            hide: hide.map(String::from),
            help: help.map(String::from),
        };
        assert_eq!(
            (chord, [space, pinned, help_chord, forced]),
            (
                (
                    keys(None, None),
                    Some([
                        "\u{250c} replace with char waiting on a char \u{2510}".to_string(),
                        "\u{2502} Esc cancel \u{2502}".to_string(),
                    ]),
                ),
                [
                    keys(None, Some("Esc ?")),
                    keys(Some("C-?"), Some("Esc ?")),
                    keys(None, Some("?")),
                    keys(Some("C-?"), Some("?")),
                ],
            ),
        );
    }

    #[test]
    fn the_register_chord_box_lists_the_special_registers() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "\"");
        h.type_keys("\"");
        let text = box_text(&mut h);

        let labels = [
            "unnamed",
            "system clipboard",
            "primary clipboard",
            "search",
            "blackhole",
            "selection index",
            "selection contents",
            "document path",
            "command",
            "named register",
            "cancel",
        ];
        assert_eq!(
            (rows, has_box_titled(&text, "register")),
            (labels.map(String::from).to_vec(), true),
            "the box lists each special register by char under its title",
        );
    }

    /// The insert-mode chord waits in insert mode, where no keymap box shows,
    /// and names its register through the same table.
    #[test]
    fn the_insert_register_chord_shows_the_same_box() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("i ctrl-r");
        let armed = box_text(&mut h);
        h.type_keys("\"");
        let resolved = box_text(&mut h);

        let shows = |text: &str| (has_box_titled(text, "register"), text.contains("blackhole"));
        assert_eq!(
            (shows(&armed), shows(&resolved)),
            ((true, true), (false, false)),
            "the box shows while the chord waits and leaves with its char",
        );
    }

    #[test]
    fn the_macro_replay_chord_box_lists_the_registers() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "q");
        h.type_keys("q");
        let text = box_text(&mut h);

        let labels: Vec<String> = yank::REGISTER_HINTS
            .iter()
            .map(|&(_, label)| label)
            .chain(["cancel"])
            .map(String::from)
            .collect();
        assert_eq!(
            (rows, has_box_titled(&text, "replay macro")),
            (labels, true),
            "the replay chord names its register through the register table",
        );
    }

    /// The mark actions have no default binding, so each one arms through
    /// dispatch, as the command palette arms it.
    #[test]
    fn the_mark_chord_box_names_its_request() {
        let mut h = TestHarness::with_size(150, 50);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::SetMark);
        let set = box_text(&mut h);
        let rows: Vec<String> = h
            .stoat
            .hints_cache
            .as_ref()
            .map(|cache| cache.rows.iter().map(|row| row.action.clone()).collect())
            .unwrap_or_default();
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoMark);
        let line = box_text(&mut h);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoMarkExact);
        let exact = box_text(&mut h);

        assert_eq!(
            (
                rows,
                has_box_titled(&set, "set mark"),
                has_box_titled(&line, "goto mark line"),
                has_box_titled(&exact, "goto mark"),
            ),
            (
                ["buffer mark", "global mark", "cancel"]
                    .map(String::from)
                    .to_vec(),
                true,
                true,
                true,
            ),
            "the box lists the mark letters under the title of each request",
        );
    }

    /// A chord whose char finds nothing edits no text and moves no cursor, so
    /// the box leaving is the one change that frame has to paint.
    #[test]
    fn a_chord_key_that_finds_nothing_still_repaints() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.type_keys("m i");
        let effect = h.stoat.update(Event::Key(keys::key(KeyCode::Char('('))));
        assert_eq!(
            effect,
            UpdateEffect::Redraw,
            "the frame takes the inside box down"
        );
    }

    #[test]
    fn the_pane_chord_box_lists_layout_keys_only_with_a_layout() {
        let mut h = TestHarness::with_size(150, 50);
        let lists = |rows: &[String], text: &str| rows.iter().any(|row| row == text);
        let ungated = |rows: &[String]| {
            [
                "SplitRight",
                "SplitDown",
                "OpenFileFinderHSplit",
                "OpenFileFinderVSplit",
                "OpenPaneFinder",
            ]
            .map(short_desc)
            .into_iter()
            .chain(["+pane nav new", "+pane display"])
            .all(|text| lists(rows, text))
        };
        let gated = |rows: &[String]| {
            [
                "ClosePane",
                "CloseOtherPanes",
                "TogglePaneWiden",
                "FocusNext",
                "DetachPane",
                "ReattachPane",
            ]
            .map(|name| lists(rows, short_desc(name)))
        };
        let one_pane = chord_rows(&mut h, "space a");

        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let split = chord_rows(&mut h, "space a");

        h.stoat.window_ipc_connected = true;
        let linked = chord_rows(&mut h, "space a");

        // A third pane leaves two split panes beside the detached one, so only
        // the detach gates the layout rows.
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DetachPane);
        let detached = chord_rows(&mut h, "space a");

        assert_eq!(
            [one_pane, split, linked, detached].map(|rows| (ungated(&rows), gated(&rows))),
            [
                (true, [false, false, false, false, false, false]),
                (true, [true, true, true, true, false, false]),
                (true, [true, true, true, true, true, false]),
                (true, [false, false, false, false, false, true]),
            ],
            "close, close others, widen, focus, detach, and reattach rows on one pane, \
             a split, a split with the window link, and a detached pane",
        );
    }

    /// The workspace picker lists every published diagnostic, so its row lists
    /// only once one exists, whether or not the focused buffer holds it.
    #[test]
    fn the_space_lsp_box_lists_the_workspace_picker_only_with_a_diagnostic() {
        let mut h = TestHarness::with_size(150, 50);
        let picker = short_desc("OpenWorkspaceDiagnosticsPicker");
        let lists_picker = |rows: Vec<String>| rows.iter().any(|row| row == picker);
        let clean = lists_picker(chord_rows(&mut h, "space l"));

        h.seed_diagnostics("/lsp/a.rs", vec![lsp_types::Diagnostic::default()]);
        assert_eq!(
            (clean, lists_picker(chord_rows(&mut h, "space l"))),
            (false, true),
        );
    }

    /// The implementation jump asks a language server about the token under
    /// the cursor. Its row lists only in a served buffer, and once the buffer
    /// has a token index, only over a token.
    #[test]
    fn the_goto_box_lists_the_implementation_jump_only_where_a_server_answers() {
        let mut h = TestHarness::with_size(150, 50);
        let jump = short_desc("GotoImplementation");
        let lists_jump = |rows: Vec<String>| rows.iter().any(|row| row == jump);
        let scratch = lists_jump(chord_rows(&mut h, "g"));

        let buffer_id = open_foo_bar(&mut h);
        let served = lists_jump(chord_rows(&mut h, "g"));

        h.stoat
            .active_workspace_mut()
            .buffers
            .store_lsp_symbol_kinds(buffer_id, Arc::from(Vec::new()));
        assert_eq!(
            [scratch, served, lists_jump(chord_rows(&mut h, "g"))],
            [false, true, false],
            "a scratch buffer, a served one, then one whose index puts no token \
             under the cursor",
        );
    }

    /// The recall key reopens the last picker, so its row lists only once a
    /// picker has opened.
    #[test]
    fn the_space_box_lists_the_recall_only_after_a_picker_opened() {
        let mut h = TestHarness::with_size(150, 50);
        let recall = "last picker";
        let lists_recall = |rows: Vec<String>| rows.iter().any(|row| row == recall);
        let fresh = lists_recall(chord_rows(&mut h, "space"));

        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.type_keys("escape");
        assert_eq!(
            (fresh, lists_recall(chord_rows(&mut h, "space"))),
            (false, true),
        );
    }

    /// The space box names each key with the config's short label, and a key
    /// that enters a submenu with the submenu's name, as the HintBox board
    /// lists them.
    #[test]
    fn the_space_box_names_its_keys_with_their_short_labels() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("space");
        h.render_composited();
        let rows: Vec<(String, String)> = h
            .stoat
            .hints_cache
            .as_ref()
            .expect("the space chord laid out its box")
            .rows
            .iter()
            .map(|row| (row.keys.clone(), row.action.clone()))
            .collect();

        let expected = [
            ("q", "quit"),
            ("a", "+pane nav"),
            ("t", "run"),
            ("J", "+job"),
            ("c", "claude"),
            ("w", "+workspace"),
            ("b", "+buffer"),
            ("l", "+lsp"),
            ("\"", "+clipboard"),
            ("C", "block comment"),
            ("A-c", "line comment"),
            ("p", "files"),
            ("r", "reload all"),
            ("g", "+goto"),
            ("h", "+git"),
            ("W", "walkthroughs"),
            ("s", "save selection"),
            ("/", "code search"),
            ("?", "toggle hints"),
        ];
        assert_eq!(
            rows,
            expected.map(|(keys, label)| (keys.to_string(), label.to_string())),
        );
    }

    /// The `Space J` box names its keys with the config's short labels.
    #[test]
    fn the_job_box_names_its_keys() {
        let mut h = TestHarness::with_size(150, 50);
        assert_eq!(
            chord_rows(&mut h, "space J"),
            ["jobs", "glance", "run again"]
        );
    }

    /// Closing the only workspace is refused, so its row lists only once a
    /// second workspace gives the close somewhere to land.
    #[test]
    fn the_workspace_chord_box_lists_close_only_with_another_workspace() {
        let mut h = TestHarness::with_size(150, 50);
        let close = short_desc("CloseWorkspace");
        let lists_close = |rows: Vec<String>| rows.iter().any(|row| row == close);
        let alone = lists_close(chord_rows(&mut h, "space w"));

        h.type_action("NewWorkspace()");
        assert_eq!(
            (alone, lists_close(chord_rows(&mut h, "space w"))),
            (false, true),
        );
    }

    #[test]
    fn space_lsp_box_shows_all_rows_without_an_index() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);

        h.type_keys("space l");
        let text = box_text(&mut h);
        assert!(
            text.contains("implementor of the trait") && text.contains("caller of the symbol"),
            "a buffer with no symbol-kind index shows every row"
        );
    }

    #[test]
    fn space_lsp_box_gates_the_lsp_and_diagnostic_rows() {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

        // The whole text, since the workspace picker's row starts the same way.
        let picker_row = "open the diagnostics picker for the focused buffer";
        let steps = |text: &str| {
            [
                text.contains("jump to the next diagnostic"),
                text.contains("jump to the previous diagnostic"),
            ]
        };

        let mut h = TestHarness::with_size(150, 50);

        // The initial scratch buffer has no language and no path, so no server
        // serves it and it carries no diagnostics.
        h.type_keys("space l");
        let scratch = box_text(&mut h);
        assert!(
            scratch.contains("caller of the symbol"),
            "token rows fail open without an index"
        );
        assert!(
            !scratch.contains("format document via LSP"),
            "the format row is hidden without a language server"
        );
        assert_eq!(
            steps(&scratch),
            [false, false],
            "the diagnostic steps hide without a diagnostic"
        );
        assert!(
            !scratch.contains(picker_row),
            "the picker row is hidden without a diagnostic"
        );

        // The sole fake client serves the rust buffer, so lsp rows appear.
        h.type_keys("escape");
        open_foo_bar(&mut h);
        h.type_keys("space l");
        let served = box_text(&mut h);
        assert!(
            served.contains("format document via LSP"),
            "the format row shows once a server serves the buffer"
        );
        assert_eq!(
            steps(&served),
            [false, false],
            "the diagnostic steps hide in a served buffer without a diagnostic"
        );
        assert!(
            !served.contains(picker_row),
            "the picker row stays hidden until a diagnostic exists"
        );

        h.type_keys("escape");
        h.seed_diagnostics(
            std::path::PathBuf::from("/lsp/a.rs"),
            vec![Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                severity: Some(DiagnosticSeverity::ERROR),
                code: None,
                code_description: None,
                source: None,
                message: String::new(),
                related_information: None,
                tags: None,
                data: None,
            }],
        );
        h.type_keys("space l");
        let with_diag = box_text(&mut h);
        assert!(
            with_diag.contains(picker_row),
            "the picker row shows once a diagnostic is seeded"
        );
        assert_eq!(
            steps(&with_diag),
            [true, true],
            "the diagnostic steps list once a diagnostic is seeded"
        );
    }
}
