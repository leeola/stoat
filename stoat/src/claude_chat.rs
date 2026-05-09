use crate::{
    host::{ClaudeSessionId, TokenUsage, ToolCallStatus},
    input_view::InputView,
};
use std::{collections::HashSet, time::Instant};

// FIXME: Claude session state not persisted across workspace save/load. On
// restore, panes that held a Claude view are rewritten to a placeholder Label
// by the workspace-persist stale-view sweep.
//
// What's already on disk:
//   - Transcripts at `<stoat_log::log_dir()>/claude-<uuid>.tx.jsonl` and `claude-<uuid>.rx.jsonl`
//     are byte-faithful protocol logs.
//   - `agent/claude_code/launcher.rs` accepts `--resume <session-id>` when an existing UUID is
//     supplied, so session continuation is at least wired on the Claude Code CLI side.
//
// Open design questions for full restore:
//   1. Persist `Vec<ChatMessage>` directly (add serde derives on `ChatMessage`,
//      `ChatMessageContent`, `ChatRole`) for fast read-only rehydration of the scrollback.
//   2. Replay transcripts to reconstruct `ChatMessage` history on load.
//   3. Re-spawn Claude Code with `--resume` to restore a live session; drop `streaming_text` and
//      flush `pending_sends` on reconnect (both are already designed as recovery state).
//
// Suggested v1: do (1) for scrollback restore + start a fresh session on
// next user input. Layer (3) once `--resume` semantics are validated
// end-to-end in the agent crate.
pub struct ClaudeChatState {
    pub session_id: ClaudeSessionId,
    pub(crate) input: InputView,
    pub messages: Vec<ChatMessage>,
    pub streaming_text: Option<String>,
    pub scroll_offset: usize,
    /// Messages the user submitted before the session host was ready.
    /// Drained and sent when the session becomes available.
    pub pending_sends: Vec<String>,
    /// Set when the user submits a message; cleared when the turn
    /// completes (Result) or errors. Drives the activity throbber.
    pub active_since: Option<Instant>,
    /// Protocol-level session UUID delivered by `AgentMessage::Init`. `None`
    /// until the first Init arrives from the Claude Code process. Persisted
    /// as part of workspace state so a future launch can pass it to
    /// `ClaudeCodeHost::resume_session`.
    pub protocol_session_id: Option<String>,
    /// When true, file-oriented tool calls (`Read`/`Edit`) open their
    /// target file in an editor pane and move the cursor to the line Claude
    /// is touching (when known). Toggled via `ClaudeToggleFollow`.
    pub follow: bool,
    /// Running token totals for this session, sourced from
    /// [`AgentMessage::Usage`]'s `accumulated` field. Drives the chat
    /// header counter and is zero until the first usage event arrives.
    pub usage: TokenUsage,
    /// Tool-use ids the user cancelled mid-flight via
    /// [`stoat_action::ClaudeInterrupt`]. Drives the `cancelled` badge
    /// painted on each matching `ToolUse`, regardless of whether a
    /// server-side `ToolResult` arrives later.
    pub cancelled_tool_uses: HashSet<String>,
    /// `ToolUse.id` of the tool card currently focused for keyboard
    /// navigation. `Tab`/`BackTab` move focus across cards within the
    /// active chat; `Enter` toggles `expanded_tool_ids` membership for
    /// the focused id.
    pub focused_tool_id: Option<String>,
    /// `ToolUse.id`s whose card is rendered in expanded form (raw input
    /// + full result body) instead of the collapsed header + preview.
    pub expanded_tool_ids: HashSet<String>,
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
        status: ToolCallStatus,
    },
    Error(String),
    TurnComplete {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
}

/// Render-time classification of a tool card. Collapses the
/// protocol-level `Pending` and `InProgress` to a single `Running`
/// label since the chat surface does not distinguish them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCardStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// Derive the card status for `tool_id` from the chat state. The
/// cancelled set takes precedence over server status; absent results
/// imply the tool is still running.
pub(crate) fn tool_card_status(chat: &ClaudeChatState, tool_id: &str) -> ToolCardStatus {
    if chat.cancelled_tool_uses.contains(tool_id) {
        return ToolCardStatus::Cancelled;
    }
    for msg in &chat.messages {
        if let ChatMessageContent::ToolResult { id, status, .. } = &msg.content {
            if id == tool_id {
                return match status {
                    ToolCallStatus::Failed => ToolCardStatus::Failed,
                    _ => ToolCardStatus::Done,
                };
            }
        }
    }
    ToolCardStatus::Running
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: ChatMessageContent,
    /// Sha of a `git stash create`-style commit captured at the moment
    /// this message was recorded. Set on user-submit messages when the
    /// active workspace's git repo had a non-clean working tree at
    /// submit time; `None` for assistant messages, scratch sessions
    /// without a repo, and clean-tree submits. Future per-message
    /// checkpoint restore actions consume this sha to reset the
    /// workspace to the captured state.
    pub checkpoint_sha: Option<String>,
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
    fn user_message_with_checkpoint_uses_restorable_marker_prefix() {
        use crate::action_handlers::dispatch;
        use std::path::PathBuf;
        use stoat_action::ClaudeSubmit;

        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/marker-render");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        let _ = h.claude().open();

        let session_id = h.stoat.active_workspace().claude_chat.expect("chat open");
        let buffer_id = h
            .stoat
            .active_workspace()
            .chats
            .get(&session_id)
            .expect("chat state")
            .input
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("input buffer");
        {
            let len = buffer.read().expect("poisoned").snapshot.visible_text.len();
            buffer
                .write()
                .expect("poisoned")
                .edit(0..len, "with checkpoint");
        }
        dispatch(&mut h.stoat, &ClaudeSubmit);

        let frame = h.snapshot();
        assert!(
            frame.content.contains("o with checkpoint"),
            "expected restorable-marker prefix on user message: {}",
            frame.content,
        );
        assert!(
            !frame.content.contains("> with checkpoint"),
            "non-restorable prefix should not appear: {}",
            frame.content,
        );
    }

    #[test]
    fn user_message_without_checkpoint_keeps_standard_prefix() {
        use crate::action_handlers::dispatch;
        use std::path::PathBuf;
        use stoat_action::ClaudeSubmit;

        let mut h = TestHarness::with_size(80, 20);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/marker-no-checkpoint");
        let _ = h.claude().open();

        let session_id = h.stoat.active_workspace().claude_chat.expect("chat open");
        let buffer_id = h
            .stoat
            .active_workspace()
            .chats
            .get(&session_id)
            .expect("chat state")
            .input
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("input buffer");
        {
            let len = buffer.read().expect("poisoned").snapshot.visible_text.len();
            buffer
                .write()
                .expect("poisoned")
                .edit(0..len, "no checkpoint");
        }
        dispatch(&mut h.stoat, &ClaudeSubmit);

        let frame = h.snapshot();
        assert!(
            frame.content.contains("> no checkpoint"),
            "expected standard prefix on user message: {}",
            frame.content,
        );
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
            theme: None,
            mouse_capture: None,
            mode_badges: std::collections::BTreeMap::new(),
            claude_permissions: std::collections::BTreeMap::new(),
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
            theme: None,
            mouse_capture: None,
            mode_badges: std::collections::BTreeMap::new(),
            claude_permissions: std::collections::BTreeMap::new(),
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
            theme: None,
            mouse_capture: None,
            mode_badges: std::collections::BTreeMap::new(),
            claude_permissions: std::collections::BTreeMap::new(),
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
            theme: None,
            mouse_capture: None,
            mode_badges: std::collections::BTreeMap::new(),
            claude_permissions: std::collections::BTreeMap::new(),
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

    #[test]
    fn snapshot_tool_card_running_status_badge() {
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").pending();
        h.assert_snapshot("tool_card_status_badge_running");
    }

    #[test]
    fn snapshot_tool_card_done_status_badge() {
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").result("a\nb\n");
        h.assert_snapshot("tool_card_status_badge_done");
    }

    #[test]
    fn snapshot_tool_card_failed_status_badge() {
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude()
            .get_session(id)
            .bash("nope")
            .failed("permission denied");
        h.assert_snapshot("tool_card_status_badge_failed");
    }

    #[test]
    fn snapshot_tool_card_cancelled_status_badge() {
        use crate::action_handlers::dispatch;
        use stoat_action::ClaudeInterrupt;
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("sleep 60").pending();
        dispatch(&mut h.stoat, &ClaudeInterrupt);
        h.assert_snapshot("tool_card_status_badge_cancelled");
    }

    #[test]
    fn snapshot_tool_card_expanded_shows_input_and_output() {
        use crate::action_handlers::dispatch;
        use stoat_action::{ClaudeFocusNextToolCard, ClaudeToggleToolCardExpand};
        let mut h = TestHarness::with_size(60, 12);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").result("a\nb\nc\n");
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        dispatch(&mut h.stoat, &ClaudeToggleToolCardExpand);
        h.assert_snapshot("tool_card_expanded");
    }

    #[test]
    fn tool_card_focus_engages_on_first_tab() {
        use crate::action_handlers::dispatch;
        use stoat_action::ClaudeFocusNextToolCard;
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("first").pending();
        h.claude().get_session(id).bash("second").pending();
        let chat_before = h
            .stoat
            .active_workspace()
            .chats
            .get(&id)
            .expect("chat state");
        assert_eq!(chat_before.focused_tool_id, None);
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        let chat_after = h
            .stoat
            .active_workspace()
            .chats
            .get(&id)
            .expect("chat state");
        let tool_ids: Vec<&str> = chat_after
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                crate::claude_chat::ChatMessageContent::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chat_after.focused_tool_id.as_deref(),
            Some(tool_ids[tool_ids.len() - 1]),
            "first Tab focuses the most recent tool card"
        );
    }

    #[test]
    fn tool_card_tab_cycles_to_older_then_wraps() {
        use crate::action_handlers::dispatch;
        use stoat_action::ClaudeFocusNextToolCard;
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("first").pending();
        h.claude().get_session(id).bash("second").pending();
        let tool_ids: Vec<String> = {
            let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
            chat.messages
                .iter()
                .filter_map(|m| match &m.content {
                    crate::claude_chat::ChatMessageContent::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect()
        };
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert_eq!(
            chat.focused_tool_id.as_deref(),
            Some(tool_ids[0].as_str()),
            "second Tab moves to the older card",
        );
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert_eq!(
            chat.focused_tool_id.as_deref(),
            Some(tool_ids[1].as_str()),
            "third Tab wraps back to the most recent card",
        );
    }

    #[test]
    fn tool_card_enter_toggles_expansion_via_prompt_submit() {
        use crate::action_handlers::dispatch;
        use stoat_action::{ClaudeFocusNextToolCard, SubmitPromptInput};
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").result("a");
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        dispatch(&mut h.stoat, &SubmitPromptInput);
        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert_eq!(
            chat.expanded_tool_ids.len(),
            1,
            "Enter expands focused card"
        );
        dispatch(&mut h.stoat, &SubmitPromptInput);
        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert!(chat.expanded_tool_ids.is_empty(), "second Enter collapses");
    }

    #[test]
    fn enter_on_focused_read_card_jumps_to_file_line() {
        use crate::action_handlers::dispatch;
        use stoat_action::{ClaudeFocusNextToolCard, SubmitPromptInput};

        let mut h = TestHarness::with_size(80, 24);
        let (id, path) = seed_follow_scenario(&mut h);
        h.claude().get_session(id).read_at(&path, 50).pending();

        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        dispatch(&mut h.stoat, &SubmitPromptInput);

        let editor_pane = h.editor_pane();
        let editor = h.editor_id_in_pane(editor_pane);
        assert_eq!(
            h.editor_scroll_row(editor),
            47,
            "focused Read card + Enter should jump the editor to line 50",
        );
    }

    #[test]
    fn enter_on_focused_bash_card_still_toggles_expansion() {
        use crate::action_handlers::dispatch;
        use stoat_action::{ClaudeFocusNextToolCard, SubmitPromptInput};

        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").result("a");

        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        dispatch(&mut h.stoat, &SubmitPromptInput);

        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert_eq!(
            chat.expanded_tool_ids.len(),
            1,
            "Bash card has no file_path; Enter should still toggle expansion",
        );
    }

    #[test]
    fn enter_on_focused_read_card_outside_workspace_is_noop() {
        use crate::action_handlers::dispatch;
        use stoat_action::{ClaudeFocusNextToolCard, SubmitPromptInput};

        let mut h = TestHarness::with_size(80, 24);
        let (id, _path) = seed_follow_scenario(&mut h);
        let outside = std::path::PathBuf::from("/etc/passwd");
        h.claude().get_session(id).read_at(&outside, 10).pending();

        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        dispatch(&mut h.stoat, &SubmitPromptInput);

        let editor_rows = h.editor_scroll_rows();
        assert!(
            editor_rows.iter().all(|&r| r == 0),
            "out-of-workspace path must not scroll any editor: {editor_rows:?}",
        );
    }

    #[test]
    fn tool_card_escape_clears_card_focus_without_exiting_prompt() {
        use crate::action_handlers::dispatch;
        use stoat_action::{CancelPromptInput, ClaudeFocusNextToolCard};
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).bash("ls").result("a");
        dispatch(&mut h.stoat, &ClaudeFocusNextToolCard);
        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert!(chat.focused_tool_id.is_some());
        let mode_before = h.stoat.mode.clone();
        dispatch(&mut h.stoat, &CancelPromptInput);
        let chat = h.stoat.active_workspace().chats.get(&id).unwrap();
        assert_eq!(
            chat.focused_tool_id, None,
            "Escape clears focus without exiting prompt mode"
        );
        assert_eq!(h.stoat.mode, mode_before, "prompt mode preserved");
    }

    #[test]
    fn snapshot_claude_header_shows_token_counter() {
        let mut h = TestHarness::with_size(60, 10);
        let id = h.claude().open();
        h.claude().get_session(id).usage(crate::host::TokenUsage {
            input_tokens: 12_000,
            output_tokens: 3_200,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        });
        h.assert_snapshot("claude_header_with_token_counter");
    }

    #[test]
    fn usage_event_updates_chat_state() {
        let mut h = TestHarness::default();
        let id = h.claude().open();
        h.claude().get_session(id).usage(crate::host::TokenUsage {
            input_tokens: 1_500,
            output_tokens: 250,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        });
        let chat = h
            .stoat
            .active_workspace()
            .chats
            .get(&id)
            .expect("chat exists");
        assert_eq!(chat.usage.input_tokens, 1_500);
        assert_eq!(chat.usage.output_tokens, 250);
    }

    #[test]
    fn claude_defaults_to_prompt_mode() {
        let mut h = TestHarness::default();
        let _ = h.claude().open();
        assert_eq!(h.stoat.mode, "prompt");
    }

    #[test]
    fn claude_escape_transitions_to_normal() {
        let mut h = TestHarness::default();
        let _ = h.claude().open();
        assert_eq!(h.stoat.mode, "prompt");
        h.type_keys("escape");
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn snapshot_claude_prompt_vs_normal_modeline() {
        let mut h = TestHarness::with_size(60, 10);
        let _ = h.claude().open();
        h.assert_snapshot("claude_pane_prompt_mode");
        h.type_keys("escape");
        h.assert_snapshot("claude_pane_normal_mode");
    }

    fn seed_follow_scenario(h: &mut TestHarness) -> (ClaudeSessionId, std::path::PathBuf) {
        h.stoat.active_workspace_mut().git_root = std::path::PathBuf::from("/test");
        let path = h.seed_long_file("long.txt", 80);
        h.stoat.open_file(&path);
        h.type_keys("escape");
        h.type_action("SplitRight()");
        let id = h.claude().open();
        (id, path)
    }

    fn toggle_follow(h: &mut TestHarness) {
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClaudeToggleFollow);
    }

    #[test]
    fn claude_follow_opens_file_and_scrolls_to_line() {
        let mut h = TestHarness::with_size(80, 24);
        let (id, path) = seed_follow_scenario(&mut h);

        toggle_follow(&mut h);
        h.claude().get_session(id).read_at(&path, 50);

        h.assert_snapshot("claude_follow_reads_to_line_50");
    }

    #[test]
    fn claude_follow_disabled_ignores_tool_use() {
        let mut h = TestHarness::with_size(80, 24);
        let (id, path) = seed_follow_scenario(&mut h);

        h.claude().get_session(id).read_at(&path, 50);

        let editor_rows = h.editor_scroll_rows();
        assert!(
            editor_rows.iter().all(|&r| r == 0),
            "no editor should have scrolled: {editor_rows:?}"
        );
    }

    #[test]
    fn claude_follow_skips_paths_outside_workspace() {
        let mut h = TestHarness::with_size(80, 24);
        let (id, _path) = seed_follow_scenario(&mut h);
        toggle_follow(&mut h);

        let outside = std::path::PathBuf::from("/etc/passwd");
        h.claude().get_session(id).read_at(&outside, 10);

        let editor_rows = h.editor_scroll_rows();
        assert!(
            editor_rows.iter().all(|&r| r == 0),
            "out-of-workspace path must not scroll editors: {editor_rows:?}"
        );
    }

    #[test]
    fn claude_follow_reuses_editor_on_repeat_reads() {
        let mut h = TestHarness::with_size(80, 24);
        let (id, path) = seed_follow_scenario(&mut h);
        toggle_follow(&mut h);

        let editor_pane = h.editor_pane();

        h.claude().get_session(id).read_at(&path, 30);
        let first = h.editor_id_in_pane(editor_pane);

        h.claude().get_session(id).read_at(&path, 50);
        let second = h.editor_id_in_pane(editor_pane);

        assert_eq!(
            first, second,
            "editor must be reused across repeat reads of the same file"
        );
        assert_eq!(h.editor_scroll_row(second), 47);
    }

    fn seed_chat_only_scenario(h: &mut TestHarness) -> (ClaudeSessionId, std::path::PathBuf) {
        h.stoat.active_workspace_mut().git_root = std::path::PathBuf::from("/test");
        let path = h.seed_long_file("long.txt", 80);
        let id = h.claude().open();
        (id, path)
    }

    #[test]
    fn claude_follow_creates_editor_when_chat_only() {
        let mut h = TestHarness::with_size(80, 24);
        let (id, path) = seed_chat_only_scenario(&mut h);
        assert_eq!(
            h.stoat.active_workspace().panes.pane_count(),
            1,
            "chat-only scenario should start with a single Claude pane"
        );
        toggle_follow(&mut h);

        h.claude().get_session(id).read_at(&path, 50);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.panes.pane_count(),
            2,
            "follow must split to create an editor pane"
        );
        let editor_row = ws
            .panes
            .split_panes()
            .find_map(|(_, p)| match p.view {
                View::Editor(eid) => ws.editors.get(eid).map(|e| e.scroll_row),
                _ => None,
            })
            .expect("split should have produced an editor pane");
        assert_eq!(editor_row, 47);
    }
}
