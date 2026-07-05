//! Lint-style guard: all terminal output goes through `togi_core::term`.
//!
//! Walks both crates' `src/` trees and fails on any direct `println!` /
//! `eprintln!` / `print!` / `eprint!` outside the sanctioned places: the
//! `term` module itself, the binary's `main.rs` (the top-level error render
//! seam), test modules, and test-only support files.

use std::fs;
use std::path::{Path, PathBuf};

/// The print macros production code must not call directly.
const PRINT_MACROS: [&str; 4] = ["println!", "eprintln!", "print!", "eprint!"];

#[test]
fn production_code_prints_only_through_term() {
    let bin_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let core_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir sits inside crates/")
        .join("togi-core")
        .join("src");

    let mut offenders = Vec::new();
    scan(&bin_src, is_sanctioned_in_bin, &mut offenders);
    scan(&core_src, is_sanctioned_in_core, &mut offenders);
    assert!(
        offenders.is_empty(),
        "terminal output outside togi_core::term — use the term:: helpers instead:\n{}",
        offenders.join("\n")
    );
}

/// Collect stray print calls under `src`, skipping sanctioned files.
fn scan(src: &Path, is_sanctioned: fn(&Path) -> bool, offenders: &mut Vec<String>) {
    for file in rust_files(src) {
        let rel = file.strip_prefix(src).expect("children of src/");
        if is_sanctioned(rel) {
            continue;
        }
        let content = fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("could not read {}: {err}", file.display()));
        for (number, line) in production_lines(&content) {
            if let Some(macro_name) = stray_print(line) {
                offenders.push(format!(
                    "{}:{number}: direct `{macro_name}` (route it through togi_core::term)",
                    file.display()
                ));
            }
        }
    }
}

/// Binary-crate files allowed to print directly: `main.rs` renders
/// top-level errors after everything else has been torn down.
fn is_sanctioned_in_bin(rel: &Path) -> bool {
    rel == Path::new("main.rs")
}

/// Library files allowed to print directly: the `term` module itself, and
/// test-only support files (declared `#[cfg(test)] mod` by their parent, so
/// they never reach a release build).
fn is_sanctioned_in_core(rel: &Path) -> bool {
    rel.starts_with("term")
        || rel
            .file_name()
            .is_some_and(|name| name == "test_support.rs")
}

/// All `.rs` files under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries =
        fs::read_dir(dir).unwrap_or_else(|err| panic!("could not read {}: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// Whether `line` is (or begins) a `#[cfg(test…)]` attribute.
fn is_cfg_test_attr(trimmed: &str) -> bool {
    trimmed.starts_with("#[cfg(test)") || trimmed.starts_with("#[cfg(all(test")
}

/// Whether `line` opens a module block (as opposed to declaring a
/// module file with `mod foo;`).
fn opens_mod_block(trimmed: &str) -> bool {
    trimmed.contains("mod ") && trimmed.trim_end().ends_with('{')
}

/// The (1-indexed line number, line) pairs of `content` that are
/// production code: everything up to the first `#[cfg(test…)]` attribute
/// that opens a `mod … {` block. Test modules sit at the bottom of their
/// files in this codebase, so the rest of such a file is test code —
/// where direct prints are fine, since test output does not go through
/// `term`.
fn production_lines(content: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut pending_cfg_test = false;
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if pending_cfg_test && opens_mod_block(trimmed) {
            break;
        }
        if pending_cfg_test && !trimmed.starts_with("#[") {
            // The attribute gated something else (a `mod foo;` file
            // declaration, a helper fn, ...): keep scanning.
            pending_cfg_test = false;
        }
        if is_cfg_test_attr(trimmed) {
            if opens_mod_block(trimmed) {
                break; // `#[cfg(test)] mod tests {` on one line
            }
            pending_cfg_test = true;
        }
        lines.push((index + 1, line));
    }
    lines
}

/// The first print macro called on `line`, ignoring `//` comments. A
/// match preceded by an identifier character is part of a longer name
/// (`println!` inside `eprintln!`), not a call.
fn stray_print(line: &str) -> Option<&'static str> {
    let code = line.split("//").next().unwrap_or(line);
    PRINT_MACROS.into_iter().find(|name| {
        code.match_indices(name)
            .any(|(at, _)| !code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}
