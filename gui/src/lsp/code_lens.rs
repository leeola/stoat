use crate::{
    editor::{Editor, EditorEvent},
    globals::{LanguageRegistry, LspHostGlobal},
};
use gpui::{Context, Entity, Subscription, Task, WeakEntity};
use lsp_types::{CodeLensParams, Command, ExecuteCommandParams, TextDocumentIdentifier, Uri};
use std::{collections::HashMap, path::Path, str::FromStr, sync::Arc};
use stoat::{
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    host::LspHost,
};
use stoat_language::Language;

/// Drives `textDocument/codeLens` requests for the active buffer and
/// renders each lens as a one-row block above the lens range's start
/// row through the display map's block layer. Owned per-editor;
/// subscribes to [`EditorEvent::Changed`], gates re-requests by
/// `(uri, buffer_version)`, and removes the prior lens blocks before
/// inserting the new set on every apply.
///
/// `textDocument/codeLens` is a whole-document request, so the entire
/// lens set is materialized as blocks; the display map clips to the
/// viewport, so off-screen blocks cost nothing to paint.
///
/// Each request launches a fresh language server through the global
/// [`LspHostGlobal`] factory, matching [`crate::lsp::InlayHintsManager`].
///
/// FIXME: route through a per-language `LspServer` cache instead of
/// launching a new child process per request.
pub struct CodeLensManager {
    editor: WeakEntity<Editor>,
    current_block_ids: Vec<CustomBlockId>,
    commands_by_block: HashMap<CustomBlockId, Command>,
    last_signature: Option<RequestSignature>,
    pending_task: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestSignature {
    uri: Uri,
    buffer_version: u64,
}

impl CodeLensManager {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak_editor = editor.downgrade();
        let subscription = cx.subscribe(&editor, |this, _editor, _event: &EditorEvent, cx| {
            this.reconcile(cx);
        });
        Self {
            editor: weak_editor,
            current_block_ids: Vec::new(),
            commands_by_block: HashMap::new(),
            last_signature: None,
            pending_task: None,
            _subscription: subscription,
        }
    }

    pub fn current_block_count(&self) -> usize {
        self.current_block_ids.len()
    }

    /// Execute the command of the lens block identified by `id`,
    /// returning whether `id` named a known lens. The
    /// `workspace/executeCommand` request runs in the background; the
    /// editor's click handler calls this when a click lands on a lens
    /// block row.
    pub fn dispatch_block(&self, id: CustomBlockId, cx: &mut Context<'_, Self>) -> bool {
        let Some(command) = self.commands_by_block.get(&id).cloned() else {
            return false;
        };
        let Some(editor) = self.editor.upgrade() else {
            return false;
        };
        let Some(LspTarget {
            host,
            language,
            workspace_root,
            uri,
        }) = lsp_target(&editor, cx)
        else {
            return false;
        };
        cx.spawn(async move |_, _| {
            let server = match host.launch(&language, &workspace_root).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(target: "stoat_gui::lsp::code_lens", ?err, "failed to launch LSP server");
                    return;
                },
            };
            let _ = server.initialize(Some(uri)).await;
            let params = ExecuteCommandParams {
                command: command.command,
                arguments: command.arguments.unwrap_or_default(),
                work_done_progress_params: Default::default(),
            };
            if let Err(err) = server.execute_command(params).await {
                tracing::warn!(target: "stoat_gui::lsp::code_lens", ?err, "executeCommand request failed");
            }
        })
        .detach();
        true
    }

    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        if !editor.read(cx).mode().is_full() {
            return;
        }
        let Some(request) = CodeLensRequest::build(&editor, cx) else {
            return;
        };
        if self.last_signature.as_ref() == Some(&request.signature) {
            return;
        }
        self.last_signature = Some(request.signature.clone());
        let signature = request.signature.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = request.run().await;
            let _ = this.update(cx, |this, cx| {
                if this.last_signature.as_ref() != Some(&signature) {
                    return;
                }
                this.apply(outcome, cx);
            });
        });
        self.pending_task = Some(task);
    }

    fn apply(&mut self, outcome: Option<Vec<(u32, String, Command)>>, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let display_map = editor.read(cx).display_map().clone();
        let lenses = outcome.unwrap_or_default();
        let remove = std::mem::take(&mut self.current_block_ids);
        if remove.is_empty() && lenses.is_empty() {
            return;
        }
        self.commands_by_block.clear();
        let mut commands = Vec::with_capacity(lenses.len());
        let blocks: Vec<BlockProperties> = lenses
            .into_iter()
            .map(|(row, title, command)| {
                commands.push(command);
                BlockProperties::from_text(
                    BlockPlacement::Above(row),
                    vec![title],
                    BlockStyle::Fixed,
                )
            })
            .collect();
        let new_ids = display_map.update(cx, |dm, dm_cx| {
            dm.remove_blocks(remove, dm_cx);
            dm.insert_blocks(blocks, dm_cx)
        });
        self.commands_by_block = new_ids.iter().copied().zip(commands).collect();
        self.current_block_ids = new_ids;
    }
}

struct CodeLensRequest {
    host: Arc<dyn LspHost>,
    language: Arc<Language>,
    workspace_root: std::path::PathBuf,
    uri: Uri,
    max_buffer_row: u32,
    signature: RequestSignature,
}

impl CodeLensRequest {
    fn build(editor: &Entity<Editor>, cx: &mut Context<'_, CodeLensManager>) -> Option<Self> {
        let LspTarget {
            host,
            language,
            workspace_root,
            uri,
        } = lsp_target(editor, cx)?;
        let buffer_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let max_buffer_row = buffer_snapshot.rope().max_point().row;
        let buffer_version = buffer_snapshot.version();

        Some(Self {
            host,
            language,
            workspace_root,
            uri: uri.clone(),
            max_buffer_row,
            signature: RequestSignature {
                uri,
                buffer_version,
            },
        })
    }

    async fn run(self) -> Option<Vec<(u32, String, Command)>> {
        let server = match self.host.launch(&self.language, &self.workspace_root).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::code_lens", ?err, "failed to launch LSP server");
                return None;
            },
        };
        let _ = server.initialize(Some(self.uri.clone())).await;
        if server.capabilities().code_lens_provider.is_none() {
            return Some(Vec::new());
        }
        let params = CodeLensParams {
            text_document: TextDocumentIdentifier { uri: self.uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let lenses = match server.code_lens(params).await {
            Ok(Some(lenses)) => lenses,
            Ok(None) => return Some(Vec::new()),
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::code_lens", ?err, "code_lens request failed");
                return None;
            },
        };

        let mut out = Vec::with_capacity(lenses.len());
        for lens in lenses {
            let row = lens.range.start.line.min(self.max_buffer_row);
            let command = if lens.command.is_some() {
                lens.command
            } else if lens.data.is_some() {
                match server.code_lens_resolve(lens).await {
                    Ok(resolved) => resolved.command,
                    Err(err) => {
                        tracing::warn!(target: "stoat_gui::lsp::code_lens", ?err, "code_lens resolve failed");
                        continue;
                    },
                }
            } else {
                continue;
            };
            let Some(command) = command else {
                continue;
            };
            out.push((row, command.title.clone(), command));
        }
        Some(out)
    }
}

/// LSP launch coordinates for an editor's buffer, resolved from the
/// installed globals. Shared by request building and command dispatch.
struct LspTarget {
    host: Arc<dyn LspHost>,
    language: Arc<Language>,
    workspace_root: std::path::PathBuf,
    uri: Uri,
}

fn lsp_target(editor: &Entity<Editor>, cx: &mut Context<'_, CodeLensManager>) -> Option<LspTarget> {
    let path = editor.read(cx).file_path()?.to_path_buf();
    let host = cx.try_global::<LspHostGlobal>()?.0.clone();
    let language = cx.try_global::<LanguageRegistry>()?.0.for_path(&path)?;
    let uri = path_to_uri(&path)?;
    let workspace_root = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.clone());
    Some(LspTarget {
        host,
        language,
        workspace_root,
        uri,
    })
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use lsp_types::{CodeLens, CodeLensOptions, Command, Position, Range, ServerCapabilities};
    use std::path::PathBuf;
    use stoat::{
        buffer::BufferId,
        host::fake::{FakeLsp, FakeLspHost},
    };
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> Arc<FakeLsp> {
        let lsp = Arc::new(FakeLsp::new());
        let lsp_host = Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>;
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(LspHostGlobal(lsp_host));
            cx.set_global(LanguageRegistry::standard());
            cx.set_global(ExecutorGlobal(executor));
        });
        lsp
    }

    fn build_editor(cx: &mut TestAppContext, path: &Path, text: &str) -> Entity<Editor> {
        let path = path.to_path_buf();
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), text));
            let executor = cx.global::<ExecutorGlobal>().0.clone();
            let multi = cx.new({
                let buffer = buffer.clone();
                |cx| MultiBuffer::singleton(buffer, cx)
            });
            let display = cx.new({
                let buffer = buffer.clone();
                |cx| DisplayMap::new(buffer, executor.clone(), cx)
            });
            let diff = cx.new({
                let buffer = buffer.clone();
                |cx| DiffMap::new(buffer, cx)
            });
            cx.new(|cx| {
                let mut ed = Editor::new(multi, display, diff, EditorMode::full(), cx);
                ed.set_file_path(Some(path), cx);
                ed
            })
        })
    }

    fn capabilities_with_code_lens() -> ServerCapabilities {
        ServerCapabilities {
            code_lens_provider: Some(CodeLensOptions {
                resolve_provider: Some(true),
            }),
            ..Default::default()
        }
    }

    fn lens(line: u32, title: &str) -> CodeLens {
        CodeLens {
            range: Range::new(Position::new(line, 0), Position::new(line, 0)),
            command: Some(Command {
                title: title.to_string(),
                command: "stoat.test".to_string(),
                arguments: None,
            }),
            data: None,
        }
    }

    fn touch(editor: &Entity<Editor>, cx: &mut TestAppContext) {
        editor.update(cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();
    }

    #[test]
    fn programmed_lenses_become_blocks_above_their_rows() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        lsp.set_capabilities(capabilities_with_code_lens());
        let path = PathBuf::from("/tmp/lens.rs");
        lsp.set_code_lenses(
            path.to_str().unwrap(),
            vec![lens(0, "Run test"), lens(1, "Debug")],
        );

        let editor = build_editor(&mut cx, &path, "fn a() {}\nfn b() {}");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let manager = cx.update(|cx| {
            let editor = editor.clone();
            cx.new(|cx| CodeLensManager::new(editor, cx))
        });

        touch(&editor, &mut cx);

        assert_eq!(manager.read_with(&cx, |m, _| m.current_block_count()), 2);
        // Two source rows plus two lens blocks -> max display row index 3.
        assert_eq!(
            display_map.update(&mut cx, |dm, _| dm.snapshot().max_point().row),
            3
        );
    }

    #[test]
    fn clearing_lenses_removes_blocks() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        lsp.set_capabilities(capabilities_with_code_lens());
        let path = PathBuf::from("/tmp/clear.rs");
        lsp.set_code_lenses(path.to_str().unwrap(), vec![lens(0, "Run test")]);

        let editor = build_editor(&mut cx, &path, "fn a() {}");
        let manager = cx.update(|cx| {
            let editor = editor.clone();
            cx.new(|cx| CodeLensManager::new(editor, cx))
        });
        touch(&editor, &mut cx);
        assert_eq!(manager.read_with(&cx, |m, _| m.current_block_count()), 1);

        lsp.set_code_lenses(path.to_str().unwrap(), Vec::new());
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(9..9, "\n", cx));
        });
        cx.run_until_parked();

        assert_eq!(manager.read_with(&cx, |m, _| m.current_block_count()), 0);
    }

    #[test]
    fn skips_when_server_does_not_advertise_code_lens() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        lsp.set_capabilities(ServerCapabilities::default());
        let path = PathBuf::from("/tmp/no_caps.rs");
        lsp.set_code_lenses(path.to_str().unwrap(), vec![lens(0, "Run test")]);

        let editor = build_editor(&mut cx, &path, "fn a() {}");
        let manager = cx.update(|cx| {
            let editor = editor.clone();
            cx.new(|cx| CodeLensManager::new(editor, cx))
        });
        touch(&editor, &mut cx);

        assert_eq!(manager.read_with(&cx, |m, _| m.current_block_count()), 0);
    }

    fn lens_block_id(
        display_map: &Entity<DisplayMap>,
        cx: &mut TestAppContext,
        row: u32,
    ) -> CustomBlockId {
        use stoat::display_map::{Block, BlockRowKind};
        display_map.update(cx, |dm, _| match dm.snapshot().classify_row(row) {
            BlockRowKind::Block {
                block: Block::Custom(b),
                ..
            } => b.id,
            _ => panic!("expected a custom block at row {row}"),
        })
    }

    #[test]
    fn dispatch_block_executes_command_for_lens() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        lsp.set_capabilities(capabilities_with_code_lens());
        let path = PathBuf::from("/tmp/dispatch.rs");
        lsp.set_code_lenses(path.to_str().unwrap(), vec![lens(0, "Run test")]);

        let editor = build_editor(&mut cx, &path, "fn a() {}");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let manager = cx.update(|cx| {
            let editor = editor.clone();
            cx.new(|cx| CodeLensManager::new(editor, cx))
        });
        touch(&editor, &mut cx);

        let block_id = lens_block_id(&display_map, &mut cx, 0);
        assert!(!manager.update(&mut cx, |m, mcx| m
            .dispatch_block(CustomBlockId(99_999), mcx)));
        assert!(manager.update(&mut cx, |m, mcx| m.dispatch_block(block_id, mcx)));
        cx.run_until_parked();

        let executed = lsp.observed_executed_commands();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].command, "stoat.test");
    }
}
