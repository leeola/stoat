use tree_sitter::Language;
use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_rust() -> *const ();
    fn tree_sitter_json() -> *const ();
    fn tree_sitter_toml() -> *const ();
    fn tree_sitter_stcfg() -> *const ();
    fn tree_sitter_markdown() -> *const ();
    fn tree_sitter_markdown_inline() -> *const ();
}

pub fn rust() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_rust) })
}

pub fn json() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_json) })
}

pub fn toml() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_toml) })
}

pub fn stcfg() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_stcfg) })
}

pub fn markdown() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_markdown) })
}

pub fn markdown_inline() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_markdown_inline) })
}

#[cfg(test)]
mod tests {
    use super::{json, markdown, markdown_inline, rust, stcfg, toml};
    use tree_sitter::Parser;

    #[test]
    fn loads_rust() {
        let mut p = Parser::new();
        p.set_language(&rust()).unwrap();
        let tree = p.parse("fn main() {}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn loads_stcfg() {
        let mut p = Parser::new();
        p.set_language(&stcfg()).unwrap();
        // Exercises a setting and a key binding; the binding drives the
        // external scanner that recognizes `Ctrl-w` as a key_part via the
        // trailing `->`.
        let tree = p
            .parse(
                "on init { theme = default_dark; }\non key { Ctrl-w -> Foo(); }\n",
                None,
            )
            .unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn loads_json() {
        let mut p = Parser::new();
        p.set_language(&json()).unwrap();
        let tree = p.parse("{}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn loads_toml() {
        let mut p = Parser::new();
        p.set_language(&toml()).unwrap();
        let tree = p.parse("a = 1\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn loads_markdown() {
        let mut p = Parser::new();
        p.set_language(&markdown()).unwrap();
        let tree = p.parse("# Title\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn loads_markdown_inline() {
        let mut p = Parser::new();
        p.set_language(&markdown_inline()).unwrap();
        let tree = p.parse("**bold**", None).unwrap();
        assert_eq!(tree.root_node().kind(), "inline");
    }
}
