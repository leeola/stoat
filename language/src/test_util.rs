//! Helpers the language crate's tests share.

use crate::{Language, LanguageRegistry};
use std::{path::Path, sync::Arc};
use tree_sitter::{Parser, Tree};

/// The standard registry's Rust language.
pub(crate) fn rust_lang() -> Arc<Language> {
    LanguageRegistry::standard()
        .for_path(Path::new("a.rs"))
        .unwrap()
}

/// The standard registry's language named `name`. Panics when no language
/// has that name.
pub(crate) fn lang(name: &str) -> Arc<Language> {
    LanguageRegistry::standard()
        .languages()
        .iter()
        .find(|l| l.name == name)
        .cloned()
        .expect("language registered")
}

/// `src` parsed with `lang`'s grammar from scratch, with no prior tree.
pub(crate) fn parse(lang: &Language, src: &str) -> Tree {
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar).expect("grammar");
    parser.parse(src, None).expect("parse")
}
