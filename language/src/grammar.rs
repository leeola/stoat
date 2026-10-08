use tree_sitter::Language;
use tree_sitter_language::LanguageFn;

// SAFETY: each symbol is the entry point a tree-sitter grammar crate links in,
// and every one of them takes no arguments and returns the grammar's static
// language pointer, which is the signature declared here.
unsafe extern "C" {
    fn tree_sitter_rust() -> *const ();
    fn tree_sitter_json() -> *const ();
    fn tree_sitter_toml() -> *const ();
    fn tree_sitter_yaml() -> *const ();
    fn tree_sitter_ron() -> *const ();
    fn tree_sitter_markdown() -> *const ();
    fn tree_sitter_markdown_inline() -> *const ();
}

pub fn rust() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_rust) })
}

pub fn json() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_json) })
}

pub fn toml() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_toml) })
}

pub fn yaml() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_yaml) })
}

pub fn ron() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_ron) })
}

pub fn markdown() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_markdown) })
}

pub fn markdown_inline() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_markdown_inline) })
}

#[cfg(test)]
mod tests {
    use super::{json, markdown, markdown_inline, ron, rust, toml, yaml};
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
    fn loads_yaml() {
        let mut p = Parser::new();
        p.set_language(&yaml()).unwrap();
        let tree = p.parse("a: 1\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "stream");
    }

    #[test]
    fn loads_ron() {
        let mut p = Parser::new();
        p.set_language(&ron()).unwrap();
        let tree = p.parse("Foo(a: 1)", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
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
