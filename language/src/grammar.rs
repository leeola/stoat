use tree_sitter::Language;
use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_rust() -> *const ();
    fn tree_sitter_json() -> *const ();
    fn tree_sitter_toml() -> *const ();
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

pub fn markdown() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_markdown) })
}

pub fn markdown_inline() -> Language {
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_markdown_inline) })
}

#[cfg(test)]
mod tests {
    use super::{json, markdown, markdown_inline, rust, toml};
    use tree_sitter::Parser;

    #[test]
    fn loads_rust() {
        let mut p = Parser::new();
        p.set_language(&rust()).unwrap();
        let tree = p.parse("fn main() {}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
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
