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
    fn tree_sitter_csv() -> *const ();
    fn tree_sitter_bash() -> *const ();
    fn tree_sitter_fish() -> *const ();
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

pub fn csv() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_csv) })
}

pub fn bash() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_bash) })
}

pub fn fish() -> Language {
    // SAFETY: from_raw takes a function that returns a tree-sitter language
    // pointer, which is what the grammar's entry point returns.
    Language::new(unsafe { LanguageFn::from_raw(tree_sitter_fish) })
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
    use super::{bash, csv, fish, json, markdown, markdown_inline, ron, rust, toml, yaml};
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

    /// YAML allows the continuation line of a double-quoted scalar at any
    /// indent, and an error node there uncolors the keys after the string.
    #[test]
    fn yaml_double_quoted_continuation_at_column_zero_parses() {
        let mut p = Parser::new();
        p.set_language(&yaml()).unwrap();
        let tree = p.parse("a:\n  b: \"x\ny\"\n  c: 1\n", None).unwrap();
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn loads_ron() {
        let mut p = Parser::new();
        p.set_language(&ron()).unwrap();
        let tree = p.parse("Foo(a: 1)", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn loads_csv() {
        let mut p = Parser::new();
        p.set_language(&csv()).unwrap();
        let tree = p.parse("a,b\n1,2\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "csv");
    }

    #[test]
    fn loads_bash() {
        let mut p = Parser::new();
        p.set_language(&bash()).unwrap();
        let tree = p.parse("echo hi\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "program");
    }

    #[test]
    fn loads_fish() {
        let mut p = Parser::new();
        p.set_language(&fish()).unwrap();
        let tree = p.parse("echo hi\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "program");
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
