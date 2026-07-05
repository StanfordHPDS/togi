//! Doc-check for `docs/togi.toml.md`: the config reference must stay in sync
//! with the binary's real parse surface. Two directions are enforced:
//!
//!  * every TOML example in the doc parses against the binary with no
//!    "unknown key" warning (so the doc never shows a key togi does not know);
//!  * every config key togi actually parses is documented (the canonical list
//!    below mirrors `togi-core`'s `config::raw`).

use std::path::PathBuf;

use assert_cmd::Command;

fn doc_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/togi.toml.md"
    ))
}

fn doc() -> String {
    std::fs::read_to_string(doc_path()).expect("docs/togi.toml.md must exist and be UTF-8")
}

/// The bodies of every fenced ```toml block in the doc.
fn toml_blocks(md: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some(body) = current.as_mut() {
            if trimmed.starts_with("```") {
                blocks.push(std::mem::take(body));
                current = None;
            } else {
                body.push_str(line);
                body.push('\n');
            }
        } else if trimmed == "```toml" {
            current = Some(String::new());
        }
    }
    blocks
}

/// Run `togi lint` in an empty project whose `togi.toml` is `toml`, with the
/// user-config layer isolated to an empty directory. With no files to lint,
/// the run exits 0 without touching any tools, so the only thing exercised
/// is config parsing — unknown keys surface as warnings on stderr.
/// Returns (success, stderr).
fn run_lint_with(toml: &str) -> (bool, String) {
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::write(project.path().join("togi.toml"), toml).expect("write project config");
    let config_dir = tempfile::tempdir().expect("config tempdir");

    let output = Command::cargo_bin("togi")
        .expect("togi binary should build")
        .arg("lint")
        .env("TOGI_CONFIG_DIR", config_dir.path())
        .current_dir(project.path())
        .output()
        .expect("togi lint runs");

    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    (output.status.success(), stderr)
}

#[test]
fn config_reference_exists() {
    assert!(doc_path().is_file(), "docs/togi.toml.md must exist");
}

#[test]
fn the_documented_user_config_path_matches_the_real_platform_path() {
    // The reference names the user-config file per platform; the binary
    // derives it from the `directories` crate (see
    // `togi-core/src/config/discover.rs`). Check the entry for whichever
    // platform runs the tests — the CI matrix covers all three — by
    // rendering the real path the way the doc abbreviates it (`~` for the
    // home directory on Unix, `%APPDATA%` on Windows).
    let dirs = directories::ProjectDirs::from("", "", "togi")
        .expect("a home directory exists in the test environment");
    let real = dirs.config_dir().join("config.toml");

    #[cfg(windows)]
    let documented = {
        let appdata = std::env::var("APPDATA").expect("APPDATA is set on Windows");
        let Ok(suffix) = real.strip_prefix(&appdata) else {
            // Non-default profile layout; the doc documents the default.
            return;
        };
        format!("%APPDATA%\\{}", suffix.display())
    };
    #[cfg(not(windows))]
    let documented = {
        let base = directories::BaseDirs::new().expect("home directory");
        let Ok(suffix) = real.strip_prefix(base.home_dir()) else {
            // Non-default layout (e.g. XDG_CONFIG_HOME outside the home
            // directory); the doc documents the default.
            return;
        };
        format!("~/{}", suffix.display())
    };

    assert!(
        doc().contains(&documented),
        "docs/togi.toml.md must document the user config path `{documented}` for this platform"
    );
}

#[test]
fn every_documented_toml_example_parses_with_no_unknown_keys() {
    let blocks = toml_blocks(&doc());
    assert!(
        !blocks.is_empty(),
        "sanity: the reference should contain at least one ```toml example"
    );
    for block in &blocks {
        let (ok, stderr) = run_lint_with(block);
        assert!(
            ok,
            "a documented TOML example failed to parse against the binary:\n{block}\n--- stderr ---\n{stderr}"
        );
        assert!(
            !stderr.contains("unknown key"),
            "a documented TOML example uses a key togi does not recognize:\n{block}\n--- stderr ---\n{stderr}"
        );
    }
}

/// Every config key togi parses today, dotted. Mirrors the parse surface in
/// `togi-core`'s `config::raw` — a hand-maintained mirror, so a key added
/// to `raw.rs` must be added here too (the forward direction, every doc
/// example parsing cleanly against the binary, is derived and needs no
/// upkeep). `<name>` stands for any managed-tool name (a bare
/// `<name> = "x.y.z"` under `[tools]` is the pin shorthand for
/// `tools.<name>.version`).
const CANONICAL_KEYS: &[&str] = &[
    "format.languages",
    "format.exclude",
    "lint.languages",
    "lint.exclude",
    "sql.dialect",
    "tools.<name>.version",
    "tools.<name>.args",
];

#[test]
fn every_parsed_config_key_is_documented() {
    let md = doc();
    for key in CANONICAL_KEYS {
        // Document by the leaf key name under its section header, which is
        // how the reference is written (e.g. a `[sql]` section that
        // documents `dialect`). Require the backticked form so incidental
        // prose (e.g. "version" in a sentence) cannot satisfy coverage.
        let (section, leaf) = key.rsplit_once('.').expect("dotted key");
        assert!(
            md.contains(&format!("`{leaf}`")),
            "docs/togi.toml.md must document `{key}` (backticked leaf `{leaf}` not found)"
        );
        let section_root = section.split('.').next().expect("section root");
        assert!(
            md.contains(&format!("[{section_root}")),
            "docs/togi.toml.md must name the `[{section_root}]` section for `{key}`"
        );
    }
}

#[test]
fn the_annotated_examples_exercise_every_documented_key() {
    // Every leaf key must appear in the doc's TOML examples, so "documented"
    // is anchored to something the binary actually parsed (the examples all
    // run through `every_documented_toml_example_parses_with_no_unknown_keys`).
    let example: String = toml_blocks(&doc()).join("\n");
    for leaf in ["languages", "exclude", "dialect", "version", "args"] {
        assert!(
            example.contains(&format!("{leaf} =")),
            "the TOML examples should set `{leaf}` so the reference is executable"
        );
    }
    for section in ["[format]", "[lint]", "[sql]", "[tools]"] {
        assert!(
            example.contains(section),
            "the TOML examples should include a `{section}` table"
        );
    }
    assert!(
        example.contains("[tools."),
        "the TOML examples should show a [tools.<name>] table with version/args"
    );
}
