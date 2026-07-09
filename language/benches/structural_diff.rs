//! Divan benchmarks for the structural-diff pipeline.
//!
//! Each case resolves the rust language and synthesizes its sources once
//! outside the timed body, then times only the diff call, so later perf work
//! compares before/after `cargo bench -p stoat_language` runs. The cases span
//! the identical, small-edit, moved-function, graph-cap, and line-fallback
//! paths the perf items target in turn.

use std::{path::Path, sync::Arc};
use stoat_language::{structural_diff, Language, LanguageRegistry};

fn main() {
    divan::main();
}

/// The rust language from the standard registry, resolved once per bench.
fn rust_language() -> Arc<Language> {
    LanguageRegistry::standard()
        .for_path(Path::new("bench.rs"))
        .expect("the standard registry resolves rust")
}

/// One distinct five-line function, so a 400-copy file is ~2k lines with no two
/// functions alike.
fn fn_text(i: usize) -> String {
    format!(
        "fn f{i}() -> u32 {{\n    let a{i} = {i};\n    let b{i} = a{i} + {i};\n    b{i} * 2\n}}\n"
    )
}

/// A ~2k-line rust file of 400 distinct small functions.
fn source_2k() -> String {
    (0..400).map(fn_text).collect()
}

#[divan::bench]
fn identical_2k(bencher: divan::Bencher<'_, '_>) {
    let lang = rust_language();
    let src = source_2k();
    bencher.bench(|| structural_diff::diff_with_language_or_lines(&lang, &src, &src));
}

#[divan::bench]
fn small_edit_2k(bencher: divan::Bencher<'_, '_>) {
    let lang = rust_language();
    let lhs = source_2k();
    let rhs = lhs.replacen("let b200 = a200 + 200;", "let b200 = a200 + 999;", 1);
    bencher.bench(|| structural_diff::diff_with_language_or_lines(&lang, &lhs, &rhs));
}

#[divan::bench]
fn moved_fn_2k(bencher: divan::Bencher<'_, '_>) {
    let lang = rust_language();
    let lhs = source_2k();
    let moved = fn_text(0);
    let rhs = format!("{}{moved}", lhs.replacen(&moved, "", 1));
    bencher.bench(|| structural_diff::diff_with_language_or_lines(&lang, &lhs, &rhs));
}

#[divan::bench]
fn rewrite_cap(bencher: divan::Bencher<'_, '_>) {
    let lang = rust_language();
    let body_a: String = (0..400).map(|i| format!("    let a{i} = {i};\n")).collect();
    let body_b: String = (0..400)
        .map(|i| format!("    let z{i} = {};\n", i * 7 + 1))
        .collect();
    let lhs = format!("fn giant() {{\n{body_a}}}\n");
    let rhs = format!("fn giant() {{\n{body_b}}}\n");
    bencher.bench(|| structural_diff::diff_with_language_or_lines(&lang, &lhs, &rhs));
}

#[divan::bench]
fn line_fallback_10k(bencher: divan::Bencher<'_, '_>) {
    let lines: Vec<String> = (0..10_000).map(|i| format!("line {i}\n")).collect();
    let lhs: String = lines.concat();
    let mut rhs_lines = lines;
    for k in 0..20 {
        let idx = k * 500 + 1;
        rhs_lines[idx] = format!("CHANGED {idx}\n");
    }
    let rhs: String = rhs_lines.concat();
    bencher.bench(|| structural_diff::diff_lines(&lhs, &rhs));
}
