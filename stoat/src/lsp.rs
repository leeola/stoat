pub(crate) mod edit_apply;
pub(crate) mod progress;
pub(crate) mod registry;
pub(crate) mod servers;
pub mod stcfg;
pub mod util;

/// The kind of symbol an LSP semantic token names, retained past highlight
/// decoding so cursor-aware features can tell a trait from a function.
///
/// Decoding collapses server token types to tree-sitter highlight scopes
/// (trait, struct, and enum all become `type`), which loses the distinction
/// callers such as the `space l` which-key filter need. This preserves it in a
/// coarser bucketing than the raw legend but finer than the highlight scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
