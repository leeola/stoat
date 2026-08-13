use crate::{action_handlers, app::Stoat};

pub(crate) mod document_highlight;
pub(crate) mod drain;
pub(crate) mod edit_apply;
pub(crate) mod folding;
pub mod hosts;
pub(crate) mod hover;
pub(crate) mod pending;
pub(crate) mod progress;
pub(crate) mod pull_diagnostics;
pub(crate) mod registry;
pub(crate) mod semantic_tokens;
pub(crate) mod servers;
pub(crate) mod session;
pub(crate) mod signature_help;
pub(crate) mod stamp;
pub mod stcfg;
pub(crate) mod sync;
pub mod util;

/// The kind of symbol an LSP semantic token names, retained past highlight
/// decoding so cursor-aware features can tell a trait from a function.
///
/// Decoding collapses server token types to tree-sitter highlight scopes
/// (trait, struct, and enum all become `type`), which loses the distinction
/// callers such as the `space l` which-key filter need. This preserves it in a
/// coarser bucketing than the raw legend but finer than the highlight scope.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum LspSymbolKind {
    Trait,
    Type,
    Function,
    Value,
    Symbol,
}

impl LspSymbolKind {
    /// The lowercase name a `token == <kind>` keymap predicate matches on.
    pub(crate) fn config_name(self) -> &'static str {
        match self {
            Self::Trait => "trait",
            Self::Type => "type",
            Self::Function => "function",
            Self::Value => "value",
            Self::Symbol => "symbol",
        }
    }
}

/// The focused editor's current buffer-snapshot version, or `None` on a review
/// view or absent editor.
///
/// Lets a trigger check its `(buffer_id, version)` dedupe key without the rope
/// clone its request builder does.
pub(crate) fn focused_buffer_version(stoat: &mut Stoat) -> Option<u64> {
    let editor = action_handlers::focused_editor_mut(stoat)?;
    if editor.review_view.is_some() {
        return None;
    }
    Some(editor.display_map.snapshot().buffer_snapshot().version())
}

/// The label for the highest-priority explicit LSP request in flight, or
/// `None` when none is pending, driving the status bar's ` lsp: ...  `
/// segment so a slow server does not make the keypress look dead.
///
/// Background-debounced requests (inlay hints, signature help, document
/// highlight, diagnostics, semantic tokens, folding) are deliberately
/// excluded because they fire on every edit and scroll and would flash the
/// segment constantly.
pub(crate) fn lsp_pending_label(stoat: &Stoat) -> Option<&'static str> {
    if let Some((label, _)) = &stoat.pending_lsp_jump {
        return Some(label);
    }

    [
        ("hover", stoat.pending_hover_request.is_some()),
        ("code actions", stoat.pending_code_action_request.is_some()),
        (
            "code action",
            stoat.pending_code_action_resolve.is_pending(),
        ),
        (
            "rename",
            stoat.pending_prepare_rename.is_some() || stoat.pending_rename.is_pending(),
        ),
        ("symbols", stoat.pending_symbol_picker_request.is_some()),
        (
            "workspace symbols",
            stoat.pending_workspace_symbol_request.is_some(),
        ),
        (
            "format",
            stoat.pending_format_request.is_pending() || stoat.pending_format_on_save.is_some(),
        ),
    ]
    .into_iter()
    .find_map(|(label, pending)| pending.then_some(label))
}

/// Poll every in-flight LSP request once, and report whether any of them
/// answered.
///
/// The run loop and the test harness both drive these features. A pump
/// enumerated in only one of the two silently never runs in the other, so this
/// is the one list both call.
///
/// This binds each result first and combines them after. A short-circuited OR
/// leaves later pumps unpolled, and the harness settles by repeating the call
/// until it reports `false`.
pub(crate) fn pump_all(stoat: &mut Stoat) -> bool {
    let jumps = action_handlers::lsp::pump_lsp_jumps(stoat);
    let hover = hover::pump_lsp_hover(stoat);
    let signature_help = signature_help::pump_lsp_signature_help(stoat);

    let inlay_hints = action_handlers::lsp::pump_lsp_inlay_hints(stoat);
    let document_highlight = document_highlight::pump_lsp_document_highlight(stoat);
    let pull_diagnostics = pull_diagnostics::pump_lsp_pull_diagnostics(stoat);
    let semantic_tokens = semantic_tokens::pump_lsp_semantic_tokens(stoat);
    let folding_ranges = folding::pump_lsp_folding_ranges(stoat);

    let code_actions = action_handlers::lsp::pump_lsp_code_actions(stoat);
    let code_action_resolve = action_handlers::lsp::pump_lsp_code_action_resolve(stoat);
    let prepare_rename = action_handlers::lsp::pump_lsp_prepare_rename(stoat);
    let rename = action_handlers::lsp::pump_lsp_rename(stoat);

    let symbol_picker = action_handlers::lsp::pump_lsp_symbol_picker(stoat);
    let workspace_symbol = action_handlers::lsp::pump_lsp_workspace_symbol(stoat);
    let symbol_finder_doc = action_handlers::lsp::pump_symbol_finder_doc(stoat);
    action_handlers::lsp::sync_symbol_finder(stoat);

    let format = action_handlers::lsp::pump_lsp_format(stoat);

    jumps
        || hover
        || signature_help
        || inlay_hints
        || document_highlight
        || pull_diagnostics
        || semantic_tokens
        || folding_ranges
        || code_actions
        || code_action_resolve
        || prepare_rename
        || rename
        || symbol_picker
        || workspace_symbol
        || symbol_finder_doc
        || format
}
