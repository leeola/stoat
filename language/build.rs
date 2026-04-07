use std::path::Path;

fn main() {
    // Path is each repo's directory inside vendor/, mirroring the repo's
    // own internal layout. The two markdown sub-grammars live as siblings
    // inside the upstream tree-sitter-markdown repo.
    compile_grammar("tree-sitter-rust", "tree-sitter-rust", "src", true);
    compile_grammar("tree-sitter-json", "tree-sitter-json", "src", false);
    compile_grammar("tree-sitter-toml", "tree-sitter-toml", "src", true);
    compile_grammar(
        "tree-sitter-markdown",
        "tree-sitter-markdown",
        "tree-sitter-markdown/src",
        true,
    );
    compile_grammar(
        "tree-sitter-markdown-inline",
        "tree-sitter-markdown",
        "tree-sitter-markdown-inline/src",
        true,
    );
}

fn compile_grammar(lib_name: &str, repo_dir: &str, src_within_repo: &str, has_scanner: bool) {
    let src = format!("../vendor/{repo_dir}/{src_within_repo}");
    let parser = format!("{src}/parser.c");
    let scanner = format!("{src}/scanner.c");

    let mut build = cc::Build::new();
    build
        .include(&src)
        .file(&parser)
        .warnings(false)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .flag_if_supported("-Wno-unused-value")
        .flag_if_supported("-Wno-implicit-function-declaration");

    if has_scanner && Path::new(&scanner).exists() {
        build.file(&scanner);
    }

    let lib_symbol = lib_name.replace('-', "_");
    build.compile(&lib_symbol);

    println!("cargo:rerun-if-changed={parser}");
    if has_scanner {
        println!("cargo:rerun-if-changed={scanner}");
    }
    println!("cargo:rerun-if-changed={src}/tree_sitter/parser.h");
}
