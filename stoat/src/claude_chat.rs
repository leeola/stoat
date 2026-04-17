use crate::{buffer::BufferId, editor_state::EditorId, host::ClaudeSessionId};
use std::time::Instant;

pub struct ClaudeChatState {
    pub session_id: ClaudeSessionId,
    pub input_editor_id: EditorId,
    pub input_buffer_id: BufferId,
    pub messages: Vec<ChatMessage>,
    pub streaming_text: Option<String>,
    pub scroll_offset: usize,
    /// Messages the user submitted before the session host was ready.
    /// Drained and sent when the session becomes available.
    pub pending_sends: Vec<String>,
    /// Set when the user submits a message; cleared when the turn
    /// completes (Result) or errors. Drives the activity throbber.
    pub active_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub enum ChatMessageContent {
    Text(String),
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        content: String,
    },
    Error(String),
    TurnComplete {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: ChatMessageContent,
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{Stoat, UpdateEffect},
        host::ClaudeSessionId,
        pane::{DockSide, DockVisibility, View},
        test_harness::{claude::ResultSpec, TestHarness},
    };
    use stoat_config::{ClaudePlacement, Settings};

    fn line_index_containing(lines: &[&str], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| {
                panic!(
                    "needle {needle:?} not found in frame:\n{}",
                    lines.join("\n")
                )
            })
    }

    fn claude_panes(stoat: &Stoat) -> Vec<ClaudeSessionId> {
        stoat
            .active_workspace()
            .panes
            .split_panes()
            .filter_map(|(_, p)| match &p.view {
                View::Claude(id) => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn claude_docks(stoat: &Stoat) -> Vec<(DockSide, DockVisibility, u16)> {
        stoat
            .active_workspace()
            .docks
            .iter()
            .filter_map(|(_, d)| match &d.view {
                View::Claude(_) => Some((d.side, d.visibility, d.default_width)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn claude_panel_pairs_tool_use_and_result() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .bash("ls -la")
            .result("file1\nfile2\nfile3");

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("Bash(ls -la)"),
            "expected tool header: {}",
            frame.content
        );
        assert!(
            frame.content.contains("file1 (+2 more lines)"),
            "expected tool result preview: {}",
            frame.content
        );
    }

    #[test]
    fn claude_panel_collapses_thinking() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .thinking("line one\nline two\nline three");

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("Thinking... (3 lines)"),
            "expected collapsed thinking: {}",
            frame.content
        );
    }

    #[test]
    fn claude_panel_clears_throbber_on_result() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.stoat
            .active_workspace_mut()
            .chats
            .get_mut(&id)
            .unwrap()
            .active_since = Some(std::time::Instant::now());

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 100,
            num_turns: 1,
        });

        let chat = &h.stoat.active_workspace().chats[&id];
        assert!(
            chat.active_since.is_none(),
            "throbber state should clear on Result"
        );
    }

    #[test]
    fn claude_panel_preserves_paragraph_breaks() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("Para one here.\n\nPara two here.");

        let frame = h.frames().last().expect("frame");
        let lines: Vec<&str> = frame.content.split('\n').collect();
        let idx_one = line_index_containing(&lines, "Para one here.");
        let idx_two = line_index_containing(&lines, "Para two here.");
        assert!(
            idx_two >= idx_one + 2,
            "expected blank separator row between paragraphs, got adjacent rows: {:?}",
            &lines[idx_one..=idx_two],
        );
    }

    #[test]
    fn claude_panel_preserves_leading_indent() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("normal line\n    indented line");

        let frame = h.frames().last().expect("frame");
        assert!(
            frame.content.contains("    indented line"),
            "expected leading indent preserved: {}",
            frame.content,
        );
    }

    #[test]
    fn claude_panel_separates_tool_from_text() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("hello before tool")
            .bash("ls")
            .pending();

        let frame = h.frames().last().expect("frame");
        let lines: Vec<&str> = frame.content.split('\n').collect();
        let idx_text = line_index_containing(&lines, "hello before tool");
        let idx_tool = line_index_containing(&lines, "Bash(ls)");
        assert!(
            idx_tool >= idx_text + 2,
            "expected blank separator row between assistant text and tool call: {:?}",
            &lines[idx_text..=idx_tool],
        );
    }

    #[test]
    fn claude_panel_tool_use_prefix_distinct_from_user() {
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();

        h.claude().get_session(id).bash("ls").pending();

        let frame = h.frames().last().expect("frame");
        assert!(
            !frame.content.contains("> Bash("),
            "tool-use header must not share the `> ` user prefix: {}",
            frame.content,
        );
    }

    #[test]
    fn chat_replay_real_session_ls_repo() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let final_text = "Here are the top-level files and directories in the \
                          repo:\n\n**Files:** `CLAUDE.md`, `CLAUDE.local.md`, \
                          `Cargo.lock`, `Cargo.toml`, `LICENSE`, `README.md`, \
                          `TODO.md`, `clippy.toml`, `config.stcfg`, `flake.lock`, \
                          `flake.nix`, `log.txt`, `rust-toolchain.toml`, \
                          `rustfmt.toml`, `stoat.log`, `test.csv`\n\n\
                          **Directories:** `action`, `agent`, `bin`, `config`, \
                          `examples`, `language`, `log`, `logs`, `references`, \
                          `scheduler`, `script`, `stoat`, `target`, \
                          `test_workspace`, `text`, `tmp`, `vendor`, `viewport`";
        let ls_output = "CLAUDE.local.md\nCLAUDE.md\nCargo.lock\nCargo.toml\n\
                         LICENSE\nREADME.md\nTODO.md\naction\nagent\nbin\n\
                         clippy.toml\nconfig\nconfig.stcfg\nexamples\n\
                         flake.lock\nflake.nix\nlanguage\nlog\nlog.txt\nlogs\n\
                         references\nrust-toolchain.toml\nrustfmt.toml\n\
                         scheduler\nscript\nstoat\nstoat.log\ntarget\ntest.csv\n\
                         test_workspace\ntext\ntmp\nvendor\nviewport";

        h.claude()
            .get_session(id)
            .text("\n\nWorking.")
            .snap("chat_replay_real_session_ls_repo_step_01_turn1_text_working")
            .result_with(ResultSpec {
                cost_usd: 0.0746,
                duration_ms: 1753,
                num_turns: 1,
            })
            .snap("chat_replay_real_session_ls_repo_step_02_turn1_result")
            .thinking("The user wants to see the files in the repo. Let me list them.")
            .snap("chat_replay_real_session_ls_repo_step_03_turn2_thinking")
            .bash("ls /Users/lee/projects/stoat")
            .snap("chat_replay_real_session_ls_repo_step_04_turn2_tool_use_ls")
            .result(ls_output)
            .snap("chat_replay_real_session_ls_repo_step_05_turn2_tool_result")
            .text(final_text)
            .snap("chat_replay_real_session_ls_repo_step_06_turn2_final_text")
            .result_with(ResultSpec {
                cost_usd: 0.1066,
                duration_ms: 7458,
                num_turns: 2,
            })
            .snap("chat_replay_real_session_ls_repo_step_07_turn2_result");

        h.assert_snapshot("chat_replay_real_session_ls_repo_final");
    }

    #[test]
    fn chat_replay_multiline_paragraphs() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("First paragraph.\n\nSecond paragraph.\n\nThird paragraph.")
            .snap("chat_replay_multiline_paragraphs_step_01_text");

        h.assert_snapshot("chat_replay_multiline_paragraphs_final");
    }

    #[test]
    fn chat_replay_long_line_wraps() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let long = "one two three four five six seven eight nine ten \
                    eleven twelve thirteen fourteen fifteen sixteen \
                    seventeen eighteen nineteen twenty twenty-one";

        h.claude()
            .get_session(id)
            .text(long)
            .snap("chat_replay_long_line_wraps_step_01_long_text");

        h.assert_snapshot("chat_replay_long_line_wraps_final");
    }

    #[test]
    fn chat_replay_indented_lines() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let text = "here is some code:\n    fn foo() {\n        bar();\n    }\nend.";

        h.claude()
            .get_session(id)
            .text(text)
            .snap("chat_replay_indented_lines_step_01_indented_text");

        h.assert_snapshot("chat_replay_indented_lines_final");
    }

    #[test]
    fn chat_replay_thinking_then_text() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .thinking("line one\nline two\nline three")
            .snap("chat_replay_thinking_then_text_step_01_thinking")
            .text("Done thinking.")
            .snap("chat_replay_thinking_then_text_step_02_text");

        h.assert_snapshot("chat_replay_thinking_then_text_final");
    }

    #[test]
    fn chat_replay_tool_use_no_result_yet() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .bash("ls")
            .snap("chat_replay_tool_use_no_result_yet_step_01_tool_use_pending")
            .pending();

        h.assert_snapshot("chat_replay_tool_use_no_result_yet_final");
    }

    #[test]
    fn chat_replay_partial_then_final_text() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .partial("Hello ")
            .snap("chat_replay_partial_then_final_text_step_01_partial_chunk_1")
            .partial("Hello world ")
            .snap("chat_replay_partial_then_final_text_step_02_partial_chunk_2")
            .partial("Hello world from Claude.")
            .snap("chat_replay_partial_then_final_text_step_03_partial_chunk_3")
            .text("Hello world from Claude.")
            .snap("chat_replay_partial_then_final_text_step_04_final_text");

        h.assert_snapshot("chat_replay_partial_then_final_text_final");
    }

    #[test]
    fn chat_replay_narrow_pane_wrap() {
        let mut h = TestHarness::with_size(40, 16);
        let id = h.claude().open();

        h.claude()
            .get_session(id)
            .text("Working on it. This reply should wrap several times in a 40-col pane.")
            .snap("chat_replay_narrow_pane_wrap_step_01_text");

        h.assert_snapshot("chat_replay_narrow_pane_wrap_final");
    }

    #[test]
    fn chat_replay_streamed_text_then_final_and_result() {
        let mut h = TestHarness::with_size(80, 24);
        let id = h.claude().open();
        let part1 = "| Directory | Primary Language |\n|---|---|";
        let part2 = format!("{part1}\n| `action/` | Rust |\n| `agent/` | Rust |");
        let full_text = format!("{part2}\n| `vendor/` | (vendor deps) |\n| `viewport/` | Rust |");

        h.claude()
            .get_session(id)
            .partial(part1)
            .snap("chat_replay_streamed_text_then_final_and_result_step_01_partial_chunk_1")
            .partial(&part2)
            .snap("chat_replay_streamed_text_then_final_and_result_step_02_partial_chunk_2")
            .partial(&full_text)
            .snap("chat_replay_streamed_text_then_final_and_result_step_03_partial_chunk_3")
            .text(&full_text)
            .snap("chat_replay_streamed_text_then_final_and_result_step_04_final_text")
            .result_with(ResultSpec {
                cost_usd: 0.2133,
                duration_ms: 58335,
                num_turns: 3,
            })
            .snap("chat_replay_streamed_text_then_final_and_result_step_05_result");

        h.assert_snapshot("chat_replay_streamed_text_then_final_and_result_final");
    }

    #[test]
    fn open_claude_defaults_to_pane() {
        let mut h = TestHarness::default();
        let id = h.claude().open();
        assert_eq!(claude_panes(&h.stoat), vec![id]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn open_claude_honors_dock_right_setting() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
        });
        let _id = h.claude().open();
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Right, DockVisibility::Open { width: 40 }, 40)]
        );
    }

    #[test]
    fn open_claude_honors_dock_left_setting() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockLeft),
        });
        let _id = h.claude().open();
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Left, DockVisibility::Open { width: 40 }, 40)]
        );
    }

    #[test]
    fn open_claude_twice_focuses_existing_pane() {
        let mut h = TestHarness::default();
        let first = h.claude().open();
        let second = h.claude().open();
        assert_eq!(first, second, "second open should reuse first session");
        assert_eq!(claude_panes(&h.stoat), vec![first]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn claude_to_pane_moves_from_dock() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
        });
        let id = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToPane);
        assert_eq!(claude_panes(&h.stoat), vec![id]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn claude_to_dock_right_moves_from_pane() {
        let mut h = TestHarness::default();
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Right, DockVisibility::Open { width: 40 }, 40)]
        );
        let has_editor = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(_)));
        assert!(
            has_editor,
            "Claude was the only pane; moving to dock should leave a scratch editor in that slot"
        );
    }

    #[test]
    fn claude_to_dock_flips_between_sides() {
        let mut h = TestHarness::with_settings(Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
        });
        let _id = h.claude().open();
        for (_, dock) in &mut h.stoat.active_workspace_mut().docks {
            dock.visibility = DockVisibility::Open { width: 25 };
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockLeft);
        assert_eq!(
            claude_docks(&h.stoat),
            vec![(DockSide::Left, DockVisibility::Open { width: 25 }, 40)]
        );
    }

    #[test]
    fn claude_to_pane_when_no_session_is_noop() {
        let mut h = TestHarness::default();
        let effect = crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToPane);
        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(claude_panes(&h.stoat), vec![]);
        assert_eq!(claude_docks(&h.stoat), vec![]);
    }

    #[test]
    fn claude_to_dock_right_keeps_other_panes_intact() {
        let mut h = TestHarness::default();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let _ = h.claude().open();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToDockRight);
        assert_eq!(claude_panes(&h.stoat), vec![]);
        let editor_pane_count = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .filter(|(_, p)| matches!(p.view, View::Editor(_)))
            .count();
        assert_eq!(editor_pane_count, 1, "Claude's pane should have closed");
    }

    #[test]
    fn snapshot_claude_as_pane_styled() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        h.assert_snapshot("claude_as_pane");
    }
}
