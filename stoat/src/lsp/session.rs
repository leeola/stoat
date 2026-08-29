//! Bringing a language server up for a buffer, and telling it the buffer
//! exists.
//!
//! A buffer's language decides which server serves it, and a grammar-less file
//! such as `.stcfg` resolves one by extension instead, so an in-process server
//! can still answer for it. Opening the first buffer of a language spawns that
//! language's server if it is not already up, either as a subprocess or in
//! process, and announces the buffer to whatever is running.
//!
//! A spawn defers while the project environment is still loading, because the
//! command to run may come from it. What the server says about itself, or fails
//! to say, lands in the status bar rather than only the log.

use crate::{
    action_handlers,
    app::{PendingSpawn, SpawnScope, Stoat, UpdateEffect},
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    host::{LocalLsp, LspHost, LspTranscript},
    lsp::{hosts, servers::ServerSource},
    workspace::WorkspaceId,
};
use lsp_types::{DidOpenTextDocumentParams, ServerInfo, TextDocumentItem};
use std::{path::Path, sync::Arc};
use stoat_text::Rope;

/// The language name used to route LSP traffic for `buffer_id`.
///
/// A grammar-backed buffer uses its tree-sitter language's name. A buffer with
/// no grammar (e.g. `.stcfg`) falls back to an extension-keyed LSP identity via
/// [`crate::lsp::servers::lsp_language_for_extension`], so an in-process server
/// can still serve it. `None` when neither resolves, leaving the buffer without
/// a language server.
pub(crate) fn lsp_language_name(buffers: &BufferRegistry, buffer_id: BufferId) -> Option<String> {
    if let Some(language) = buffers.language_for(buffer_id) {
        return Some(language.name.to_string());
    }
    let extension = buffers.path_for(buffer_id)?.extension()?.to_str()?;
    crate::lsp::servers::lsp_language_for_extension(extension).map(str::to_string)
}

/// Notify the workspace's LSP host that `buffer_id` was just opened.
/// No-op when `buffer_id` is already in [`Stoat::lsp_opened`]; that
/// dedupes the second `OpenFile` of an already-loaded buffer (which
/// is idempotent in [`crate::buffer_registry::BufferRegistry::open`]
/// but must fire `did_open` exactly once over the buffer's lifetime).
///
/// The dispatch is detached on the workspace's `Executor` because
/// `did_open` is a fire-and-forget notification; production
/// [`crate::host::LspHost`] implementations may write to a JSON-RPC
/// channel asynchronously, so blocking the open path on it would be
/// wrong. Errors are swallowed -- a notification failure is not
/// fatal to the open.
/// `text` is the buffer's own rope rather than the string it was opened from,
/// which is what the server reads and what a clone passes for a refcount bump.
/// Each host's payload string is built inside its own spawned task, so a
/// language server coming up over many open buffers does not materialize them
/// all on the run loop.
pub(crate) fn notify_buffer_opened(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    buffer_id: BufferId,
    path: &Path,
    text: Rope,
) {
    maybe_spawn_language_server(stoat, workspace, buffer_id);
    if !stoat.lsp_opened.insert(buffer_id) {
        return;
    }
    let Some(uri) = action_handlers::lsp::path_to_uri(path) else {
        return;
    };
    let language_id = lsp_language_name(&stoat.workspaces[workspace].buffers, buffer_id)
        .unwrap_or_else(|| "plaintext".to_string());
    let buffer_version = stoat.workspaces[workspace]
        .buffers
        .get(buffer_id)
        .map(|b| b.read().expect("buffer lock").version())
        .unwrap_or(0);
    stoat.lsp_buffer_versions.insert(buffer_id, buffer_version);
    stoat.lsp_doc_versions.insert(buffer_id, 0);
    stoat
        .lsp_last_delivered_text
        .lock()
        .expect("lsp text mutex")
        .insert(buffer_id, text.clone());
    stoat
        .lsp_last_delivered_buffer_version
        .lock()
        .expect("lsp version mutex")
        .insert(buffer_id, buffer_version);
    for lsp in hosts::hosts_for_buffer(stoat, buffer_id) {
        let (uri, language_id, text) = (uri.clone(), language_id.clone(), text.clone());
        stoat
            .executor
            .spawn(async move {
                let params = DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id,
                        version: 0,
                        text: text.to_string(),
                    },
                };
                if let Err(err) = lsp.did_open(params).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "did_open notification failed");
                }
            })
            .detach();
    }
}

/// Launch the servers `buffer_id` calls for the first time a buffer calls for
/// them, registering each in [`Stoat::lsp_registry`] once it is ready.
///
/// Those are the buffer's own language servers
/// ([`crate::lsp::servers::resolve_servers`]) plus the global ones
/// ([`crate::lsp::servers::resolve_global_servers`]), which every buffer calls
/// for, a buffer with no language at all included.
///
/// No-op unless auto-spawn is enabled. Each server spawns at most once. A
/// server already up or already spawn-attempted is skipped, and a real injected
/// sole host (tests, legacy) suppresses spawning entirely. The binary opts into
/// auto-spawn via [`Stoat::set_lsp_auto_spawn`]. Tests leave it off, so no
/// server IO happens.
///
/// Each spawn plus `initialize` handshake runs detached on the workspace
/// [`Stoat::executor`] via [`spawn_server`]. The ready host, or the failure, is
/// parked in [`Stoat::pending_lsp_host`] for [`Stoat::update`] to install.
pub(crate) fn maybe_spawn_language_server(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    buffer_id: BufferId,
) {
    if !stoat.lsp_auto_spawn {
        return;
    }
    // A real injected sole host (tests, legacy) already serves every language.
    if stoat.lsp_registry.has_real_sole_client() {
        return;
    }

    let language_name = lsp_language_name(&stoat.workspaces[workspace].buffers, buffer_id);
    let scoped: Vec<(crate::lsp::servers::ResolvedServer, SpawnScope)> = language_name
        .iter()
        .flat_map(|name| {
            crate::lsp::servers::resolve_servers(&stoat.settings, name)
                .into_iter()
                .map(|server| (server, SpawnScope::Language(name.clone())))
        })
        .chain(
            crate::lsp::servers::resolve_global_servers(&stoat.settings)
                .into_iter()
                .map(|server| (server, SpawnScope::Global)),
        )
        .collect();

    // Only the servers not already up and not already tried this session. A
    // failed spawn is never retried.
    let to_spawn: Vec<(crate::lsp::servers::ResolvedServer, SpawnScope)> = scoped
        .into_iter()
        .filter(|(server, _)| {
            !stoat.lsp_registry.contains_client(&server.name)
                && !stoat.lsp_registry.spawn_attempted(&server.name)
        })
        .collect();
    if to_spawn.is_empty() {
        return;
    }

    // A subprocess server started before the direnv environment lands would run
    // under the wrong PATH, so defer those until install_pending re-fires them.
    // An in-process server has no process and no environment, so it starts now
    // regardless.
    let env_loading =
        stoat.active_workspace().env.state == crate::project_env::EnvLoadState::Loading;
    let mut deferred = false;
    for (server, scope) in to_spawn {
        if env_loading && matches!(server.source, ServerSource::Command(_)) {
            deferred = true;
            continue;
        }
        stoat.lsp_registry.mark_spawn_attempted(server.name.clone());
        spawn_server(stoat, server, scope);
    }
    if deferred {
        stoat.lsp_spawn_deferred = Some(buffer_id);
    }
}

/// Spawn one resolved `server` for `language` detached on the workspace
/// executor, parking the ready host or the failure in
/// [`Stoat::pending_lsp_host`].
fn spawn_server(stoat: &mut Stoat, server: crate::lsp::servers::ResolvedServer, scope: SpawnScope) {
    let crate::lsp::servers::ResolvedServer { name, source, .. } = server;
    match source {
        ServerSource::Command(argv) => spawn_command_server(stoat, name, argv, scope),
        ServerSource::InProcess(construct) => {
            spawn_in_process_server(stoat, name, construct, scope)
        },
    }
}

/// Spawn a subprocess language server `command` with `argv`, initialize it under
/// the workspace environment, and park the result.
fn spawn_command_server(stoat: &mut Stoat, command: String, argv: Vec<String>, scope: SpawnScope) {
    let git_root = stoat.active_workspace().git_root.clone();
    let env = stoat.active_workspace().env.diff.clone();
    let root_uri = action_handlers::lsp::path_to_uri(&git_root);
    let slot = stoat.pending_lsp_host.clone();
    let wake = stoat.redraw_notify.clone();
    let transcript = if stoat.settings.text_proto_log == Some(true) {
        match LspTranscript::create(&command) {
            Ok(transcript) => Some(transcript),
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "text_proto_log transcript disabled");
                None
            },
        }
    } else {
        None
    };

    let args: Vec<String> = argv.into_iter().skip(1).collect();

    stoat
        .executor
        .spawn(async move {
            let host: Arc<dyn LspHost> =
                match LocalLsp::spawn(&command, &args, &env, &git_root, transcript, wake) {
                    Ok(host) => Arc::new(host),
                    Err(err) => {
                        tracing::warn!(target: "stoat::lsp", ?err, %command, "language server spawn failed");
                        slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                            server: command.clone(),
                            scope: scope.clone(),
                            result: Err(format!("{command}: {err}")),
                        });
                        return;
                    },
                };
            match host.initialize(root_uri).await {
                Ok(result) => {
                    tracing::info!(
                        target: "stoat::lsp",
                        %command,
                        server = %server_label(result.server_info.as_ref()),
                        "language server initialized",
                    );
                },
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, %command, "language server initialize failed");
                    slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                        server: command.clone(),
                        scope: scope.clone(),
                        result: Err(format!("{command}: {err}")),
                    });
                    return;
                },
            }
            slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                server: command,
                scope,
                result: Ok(host),
            });
        })
        .detach();
}

/// Build an in-process language server `name` for `language` on the workspace
/// executor and park the result.
///
/// There is no subprocess and no environment overlay. An in-process host emits
/// no server-initiated traffic, so once parked it wakes the redraw loop itself;
/// otherwise nothing would drive [`Stoat::install_pending_lsp_host`].
fn spawn_in_process_server(
    stoat: &mut Stoat,
    name: String,
    construct: fn() -> Arc<dyn LspHost>,
    scope: SpawnScope,
) {
    let slot = stoat.pending_lsp_host.clone();
    let wake = stoat.redraw_notify.clone();

    stoat
        .executor
        .spawn(async move {
            let host = construct();
            let result = match host.initialize(None).await {
                Ok(_) => {
                    tracing::info!(target: "stoat::lsp", %name, "in-process language server initialized");
                    Ok(host)
                },
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, %name, "in-process language server initialize failed");
                    Err(format!("{name}: {err}"))
                },
            };
            slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                server: name,
                scope,
                result,
            });
            wake.notify_one();
        })
        .detach();
}

/// A language server's `name@version` identity from its
/// `InitializeResult`, for logging. The version is omitted when the
/// server reported a name but no version, and the whole label is
/// "unknown" when the server reported no `serverInfo` at all.
fn server_label(info: Option<&ServerInfo>) -> String {
    let Some(info) = info else {
        return "unknown".to_string();
    };
    match &info.version {
        Some(version) => format!("{}@{}", info.name, version),
        None => info.name.clone(),
    }
}

/// Deliver `msg` as the transient status message.
pub(crate) fn set_lsp_status(stoat: &mut Stoat, msg: String) {
    stoat.set_status(msg);
}

/// Report why a user-launched LSP action for `what` cannot be served, then
/// return [`UpdateEffect::Redraw`] so the frame repaints with the message.
///
/// Walks the language-server state in priority order and reports the first
/// reason that applies. An installed host that simply lacks the capability
/// comes first, then each reason the [`NoopLsp`] placeholder is still in place,
/// namely that the spawn failed, is deferred until the project environment
/// loads, is still starting, or was never attempted.
pub(crate) fn report_lsp_unavailable(stoat: &mut Stoat, what: &str) -> UpdateEffect {
    let msg = if stoat.lsp_registry.has_active_host() {
        format!("lsp: server does not support {what}")
    } else if let Some(err) = &stoat.lsp_spawn_failed {
        format!("lsp: {err}")
    } else if stoat.lsp_spawn_deferred.is_some() {
        "lsp: server start waiting on the project environment".to_string()
    } else if stoat.lsp_registry.spawn_attempted_any() {
        "lsp: server still starting".to_string()
    } else {
        "lsp: no language server running".to_string()
    };

    set_lsp_status(stoat, msg);
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, open_stcfg_with_server, seed},
        test_harness::TestHarness,
    };
    use std::time::Duration;
    use stoat_action::OpenFile;

    #[test]
    fn did_open_dispatched_on_first_open() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        h.settle();
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(opens.len(), 1, "expected exactly one did_open");
        assert!(opens[0].text_document.uri.as_str().ends_with("/a.rs"));
        assert_eq!(opens[0].text_document.text, "fn a() {}\n");
        assert_eq!(opens[0].text_document.language_id, "rust");
    }

    /// The delivered-text snapshot and the payload the server received have to
    /// be the same text. The incremental `did_change` path computes its
    /// positions against the snapshot while the server holds the payload, so
    /// any drift between them puts every later edit at the wrong offset.
    ///
    /// Both now come off the buffer's own rope, one as a clone and one as a
    /// string built in the dispatch task, which is what keeps them equal
    /// without materializing the buffer twice on the open path.
    #[test]
    fn did_open_records_the_text_it_delivered() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        h.settle();

        let opens = h.fake_lsp().observed_opens();
        let [open] = &opens[..] else {
            panic!("one open, got {}", opens.len());
        };
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("a.rs"))
            .expect("the opened buffer");

        let delivered = h
            .stoat
            .lsp_last_delivered_text
            .lock()
            .expect("lsp text mutex")
            .get(&buffer_id)
            .map(|rope| rope.to_string());
        assert_eq!(
            delivered.as_deref(),
            Some(open.text_document.text.as_str()),
            "the snapshot the next did_change diffs against is what the server got",
        );
        assert_eq!(
            delivered.as_deref(),
            Some("fn a() {}\n"),
            "and both are the buffer's text rather than something rebuilt",
        );
    }

    #[test]
    fn did_open_not_redispatched_on_reopen() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        for _ in 0..3 {
            action_handlers::dispatch(
                &mut h.stoat,
                &OpenFile {
                    path: root.join("a.rs"),
                },
            );
            h.settle();
        }
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(
            opens.len(),
            1,
            "did_open should fire exactly once per buffer lifetime"
        );
    }

    #[test]
    fn auto_spawn_skipped_when_a_real_host_is_installed() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        assert!(
            !h.stoat.lsp_registry.spawn_attempted_any(),
            "FakeLsp is a non-noop host, so opening a rust buffer attempts no spawn",
        );
    }

    #[test]
    fn a_buffer_with_no_language_still_spawns_the_globals() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat.set_lsp_host(Arc::new(crate::host::NoopLsp));
        h.stoat.set_lsp_auto_spawn(true);
        // In-process, so the spawn this drives opens no process.
        h.stoat.settings.lsp_globals = Some(vec!["stcfg-ls".to_string()]);
        let root = seed(&mut h, &[("notes.txt", "hello\n")]);

        open_buffer(&mut h, root.join("notes.txt"));

        assert!(
            h.stoat.lsp_registry.spawn_attempted("stcfg-ls"),
            "a buffer with no language of its own still calls for the globals",
        );
    }

    #[test]
    fn a_global_server_reopens_every_language_it_now_serves() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat.set_lsp_host(Arc::new(crate::host::NoopLsp));
        h.stoat.settings.lsp_globals = Some(vec!["emoji-ls".to_string()]);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.json", "{}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        open_buffer(&mut h, root.join("b.json"));

        let global = Arc::new(crate::host::FakeLsp::new());
        h.stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "emoji-ls".to_string(),
                scope: SpawnScope::Global,
                result: Ok(global.clone()),
            });

        crate::lsp::drain::install_pending_lsp_host(&mut h.stoat);
        h.settle();

        let mut opened: Vec<String> = global
            .observed_opens()
            .iter()
            .map(|open| open.text_document.uri.as_str().to_string())
            .collect();
        opened.sort();
        assert_eq!(opened.len(), 2, "opened: {opened:?}");
        assert!(opened[0].ends_with("/a.rs"));
        assert!(opened[1].ends_with("/b.json"));
    }

    #[test]
    fn lsp_spawn_defers_while_env_loading() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat.set_lsp_host(Arc::new(crate::host::NoopLsp));
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        h.stoat.active_workspace_mut().env.state = crate::project_env::EnvLoadState::Loading;

        open_buffer(&mut h, root.join("a.rs"));

        // Names the server rather than asking whether any spawn happened. The
        // in-process global has no environment to wait for and starts regardless.
        assert!(
            !h.stoat.lsp_registry.spawn_attempted("rust-analyzer"),
            "the subprocess spawn is deferred, not attempted, while the env loads",
        );
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("a.rs"));
        assert!(buffer_id.is_some());
        assert_eq!(h.stoat.lsp_spawn_deferred, buffer_id);
    }

    #[test]
    fn env_install_consumes_lsp_deferral() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat.set_lsp_host(Arc::new(crate::host::NoopLsp));
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let ws_id = h.stoat.active_workspace;
        h.stoat.active_workspace_mut().env.state = crate::project_env::EnvLoadState::Loading;
        open_buffer(&mut h, root.join("a.rs"));
        assert!(h.stoat.lsp_spawn_deferred.is_some());

        // Install a real host so the re-fired spawn is gated at the noop check,
        // keeping the test free of a real language-server process, then land
        // the env.
        h.stoat.set_lsp_host(Arc::new(crate::host::FakeLsp::new()));
        *h.stoat.pending_env.lock().expect("pending env mutex") =
            Some(crate::project_env::PendingEnvLoad {
                workspace: ws_id,
                manual: false,
                outcome: Ok(Vec::new()),
            });
        crate::project_env::install_pending(&mut h.stoat);

        assert_eq!(
            h.stoat.lsp_spawn_deferred, None,
            "install consumes the deferral"
        );
        assert_eq!(
            h.stoat.active_workspace().env.state,
            crate::project_env::EnvLoadState::Loaded,
        );
    }

    #[test]
    fn stcfg_buffer_completes_settings_via_in_process_server() {
        use crate::completion::{request::COMPLETION_DEBOUNCE, CompletionSource};

        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        open_stcfg_with_server(&mut h);

        h.type_text("on init { form");

        // did_change (50ms) syncs the buffer to the server before the completion
        // request (150ms) reads it.
        h.advance_clock(crate::lsp::sync::LSP_DID_CHANGE_DEBOUNCE);
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h
            .stoat
            .pending_completion
            .clone()
            .expect("completion popup armed");
        let format_item = popup
            .items
            .iter()
            .find(|item| item.label == "format_on_save")
            .expect("in-process stcfg server offers format_on_save");
        assert_eq!(format_item.source, CompletionSource::Lsp);
    }

    #[test]
    fn any_buffer_completes_emoji_shortcodes_and_accepting_leaves_the_glyph() {
        use crate::completion::{request::COMPLETION_DEBOUNCE, CompletionSource};

        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        // Markdown has no server of its own, so only the global one answers.
        h.stoat.lsp_registry = crate::lsp::registry::LspRegistry::new();
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("notes.md", "")]);
        open_buffer(&mut h, root.join("notes.md"));

        h.type_keys("i");
        h.type_text("hi :smil");
        // did_change syncs the buffer to the server before the completion
        // request reads it.
        h.advance_clock(crate::lsp::sync::LSP_DID_CHANGE_DEBOUNCE);
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h
            .stoat
            .pending_completion
            .clone()
            .expect("completion popup armed");
        // The display row rather than the position in the arrival order, since
        // the selection names a row the popup shows.
        let smile = popup
            .rows()
            .position(|item| item.label == ":smile:")
            .expect("the shortcode server offers :smile:");
        assert_eq!(
            popup.row(smile).expect("the row just found").source,
            CompletionSource::Lsp
        );

        h.stoat
            .pending_completion
            .as_mut()
            .expect("popup")
            .selected_idx = smile;
        action_handlers::dispatch(&mut h.stoat, &stoat_action::AcceptCompletion);

        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("notes.md"))
            .expect("the buffer is open");
        let text = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("buffer lock")
            .rope()
            .to_string();
        assert_eq!(
            text, "hi \u{1f604}",
            "accepting leaves the emoji, not the shortcode",
        );
    }

    #[test]
    fn stcfg_buffer_reports_syntax_error_diagnostics() {
        use lsp_types::DiagnosticSeverity;

        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        let path = open_stcfg_with_server(&mut h);

        h.type_text("on init { format_on_save = ");

        // did_change (50ms) syncs before the pull-diagnostics request (300ms).
        h.advance_clock(crate::lsp::sync::LSP_DID_CHANGE_DEBOUNCE);
        h.advance_clock(Duration::from_millis(300));

        let diagnostics: Vec<_> = h
            .stoat
            .diagnostics
            .iter()
            .find(|(diag_path, _)| *diag_path == path)
            .map(|(_, diags)| diags.to_vec())
            .expect("diagnostics recorded for config.stcfg");
        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR)),
            "expected a syntax-error diagnostic, got {diagnostics:?}",
        );
    }

    #[test]
    fn each_language_routes_did_open_to_its_own_server() {
        let mut h = TestHarness::with_size(80, 24);
        let rust_server = Arc::new(crate::host::FakeLsp::new());
        let json_server = Arc::new(crate::host::FakeLsp::new());
        h.stoat
            .lsp_registry
            .insert("rust-analyzer".into(), rust_server.clone());
        h.stoat
            .lsp_registry
            .set_language("rust".into(), "rust-analyzer".into());
        h.stoat
            .lsp_registry
            .insert("json-ls".into(), json_server.clone());
        h.stoat
            .lsp_registry
            .set_language("json".into(), "json-ls".into());

        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.json", "{}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        open_buffer(&mut h, root.join("b.json"));

        let rust_opens = rust_server.observed_opens();
        assert_eq!(rust_opens.len(), 1, "rust server sees only the rust file");
        assert!(rust_opens[0].text_document.uri.as_str().ends_with("/a.rs"));

        let json_opens = json_server.observed_opens();
        assert_eq!(json_opens.len(), 1, "json server sees only the json file");
        assert!(json_opens[0]
            .text_document
            .uri
            .as_str()
            .ends_with("/b.json"));
    }

    #[test]
    fn did_open_falls_back_to_plaintext_when_no_language() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("note.txt", "hello\n")]);
        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("note.txt"),
            },
        );
        h.settle();
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].text_document.language_id, "plaintext");
    }

    #[test]
    fn did_open_separate_files_each_dispatch() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "x\n"), ("b.rs", "y\n")]);
        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("b.rs"),
            },
        );
        h.settle();
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(opens.len(), 2);
    }
}
