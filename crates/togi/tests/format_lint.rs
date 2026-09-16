//! Integration tests for `togi format` and `togi lint` against the
//! `tests/fixtures/mixed-project/` fixture (R + Python + Quarto + Markdown
//! + SQL; some files misformatted, some with lint violations).
//!
//! Offline strategy: the fixture is copied into a sandbox whose
//! `TOGI_DATA_DIR` tool cache is pre-populated with *shim* binaries that
//! speak each real tool's command-line and output protocol (the protocols
//! themselves are pinned against recorded outputs in the adapter unit
//! tests). `TOGI_RELEASE_BASE_URL` points at a closed port, so any
//! accidental download attempt fails fast instead of touching the network.
//! Shims are shell scripts, so those tests are unix-only; the walk-level
//! behaviors (usage errors, empty projects) run everywhere.
//!
//! The fixture's misformat/violation markers the shims react to:
//! - `x=` in `.R`/`.py` files → needs formatting (air / ruff)
//! - `import os` / `undefined_name` in `.py` → ruff F401 (safe fix) / F821
//! - three consecutive spaces in `.qmd`/`.md` → panache reformat + finding
//! - space before a comma in `.sql` → sqlfluff LT01 (fixable)
//!
//! The `online` module at the bottom (feature `online-tests`, `#[ignore]`)
//! runs the real managed tools end to end on the same fixture.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// Baked default tool versions (mirrors `src/tools/versions.rs`; drift
/// makes the fake cache miss, which fails loudly against the dead URL).
/// Only the shim tests consume these, and shims are unix-only.
#[cfg(unix)]
const AIR_DEFAULT: &str = "0.10.0";
#[cfg(unix)]
const RUFF_DEFAULT: &str = "0.14.0";
#[cfg(unix)]
const PANACHE_DEFAULT: &str = "2.60.0";
#[cfg(unix)]
const SQLFLUFF_DEFAULT: &str = "3.4.0";

/// A throwaway copy of the mixed-project fixture plus isolated config and
/// tool-cache directories.
struct Sandbox {
    _root: tempfile::TempDir,
    project: PathBuf,
    user_dir: PathBuf,
    data_dir: PathBuf,
}

impl Sandbox {
    /// A sandbox around a fresh copy of `tests/fixtures/mixed-project`.
    fn with_fixture() -> Sandbox {
        let sb = Sandbox::empty();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("mixed-project");
        copy_tree(&fixture, &sb.project);
        sb
    }

    /// A sandbox whose project directory is empty.
    fn empty() -> Sandbox {
        let root = tempfile::tempdir().expect("create sandbox tempdir");
        let project = root.path().join("project");
        let user_dir = root.path().join("user-config");
        let data_dir = root.path().join("data");
        // `.git` marker stops config discovery from walking out of the
        // sandbox into some real togi.toml.
        fs::create_dir_all(project.join(".git")).expect("create project/.git");
        fs::create_dir_all(&user_dir).expect("create user config dir");
        Sandbox {
            _root: root,
            project,
            user_dir,
            data_dir,
        }
    }

    #[cfg(unix)]
    fn write_project_config(&self, contents: &str) {
        fs::write(self.project.join("togi.toml"), contents).expect("write togi.toml");
    }

    #[cfg(unix)]
    fn write_file(&self, rel: &str, contents: &str) {
        let path = self.project.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write project file");
    }

    #[cfg(unix)]
    fn read_file(&self, rel: &str) -> String {
        fs::read_to_string(self.project.join(rel)).expect("read project file")
    }

    /// `togi <args...>` in the sandbox with the network pointed at a
    /// closed port: everything must come from the fake tool cache.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = self.cmd_allowing_network(args);
        cmd.env("TOGI_RELEASE_BASE_URL", dead_url());
        cmd
    }

    /// `togi <args...>` without the network kill-switch (online tests).
    fn cmd_allowing_network(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("togi").expect("togi binary should build");
        cmd.current_dir(&self.project)
            .env("TOGI_CONFIG_DIR", &self.user_dir)
            .env("TOGI_DATA_DIR", &self.data_dir)
            .args(args);
        cmd
    }
}

/// Copy `src` into `dest` recursively (fixture directories are shallow).
fn copy_tree(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).expect("create copy destination");
    for entry in fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("read fixture entry");
        let target = dest.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// A `http://127.0.0.1:<port>` URL nothing listens on: any download
/// attempt fails with connection refused, like a machine with no network.
fn dead_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// ---------------------------------------------------------------------------
// Cross-platform behavior that needs no tools at all.

#[test]
fn format_with_a_missing_path_is_a_usage_error() {
    let sb = Sandbox::with_fixture();
    sb.cmd(&["format", "no-such-file.R"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does not exist").and(predicate::str::contains("hint:")));
}

#[test]
fn lint_with_a_missing_path_is_a_usage_error() {
    let sb = Sandbox::with_fixture();
    sb.cmd(&["lint", "no-such-dir"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does not exist").and(predicate::str::contains("hint:")));
}

#[test]
fn format_in_an_empty_project_is_a_friendly_notice() {
    let sb = Sandbox::empty();
    sb.cmd(&["format"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no files to format"));
}

#[test]
fn lint_in_an_empty_project_is_a_friendly_notice() {
    let sb = Sandbox::empty();
    sb.cmd(&["lint"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no files to lint"));
}

#[test]
fn lint_json_in_an_empty_project_emits_an_empty_array() {
    let sb = Sandbox::empty();
    let assert = sb.cmd(&["lint", "--format", "json"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "[]", "machine consumers always get JSON");
}

#[test]
fn lint_json_survives_quiet_in_an_empty_project() {
    // --quiet silences human chrome only; the JSON a machine consumer
    // asked for must still land on stdout.
    let sb = Sandbox::empty();
    let assert = sb
        .cmd(&["lint", "--quiet", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "[]", "--quiet must not suppress JSON output");
}

// ---------------------------------------------------------------------------
// Offline end-to-end runs against shim tools (unix: shims are sh scripts).

#[cfg(unix)]
mod with_shims {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Lay one installed tool into the fake cache: an executable `script`
    /// plus the `manifest.json` a completed install writes.
    fn install_shim(sb: &Sandbox, name: &str, version: &str, script: &str) {
        let dir = sb.data_dir.join("tools").join(name).join(version);
        fs::create_dir_all(&dir).expect("create fake tool dir");
        let bin = dir.join(name);
        fs::write(&bin, script).expect("write shim");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod shim");
        let manifest = format!(
            r#"{{
  "version": "{version}",
  "source_url": "https://example.test/{name}-{version}.tar.gz",
  "installed_at": "2026-07-01T09:00:00Z"
}}"#
        );
        fs::write(dir.join("manifest.json"), manifest).expect("write fake manifest");
    }

    /// air protocol: `format [--check] --no-color <files...>`; check mode
    /// names drift on stderr (`Would reformat: <f>`, exit 1), write mode
    /// rewrites silently.
    const AIR_SHIM: &str = r#"#!/bin/sh
check=0
for a in "$@"; do
  case "$a" in --check) check=1 ;; esac
done
status=0
for f in "$@"; do
  case "$f" in format|--*) continue ;; esac
  if grep -q 'x=' "$f" 2>/dev/null; then
    if [ $check -eq 1 ]; then
      echo "Would reformat: $f" >&2
      status=1
    else
      printf 'x <- 1\n' > "$f"
    fi
  fi
done
exit $status
"#;

    /// ruff protocol: `format [--check] -- <files>` (would-reformat lines
    /// on stdout) and `check --output-format json [--fix] -- <files>`
    /// (JSON findings; `--fix` applies the safe F401 fix first).
    const RUFF_SHIM: &str = r#"#!/bin/sh
sub="$1"; shift
check=0; fix=0; dd=0; files=""
for a in "$@"; do
  if [ $dd -eq 1 ]; then files="$files $a"; continue; fi
  case "$a" in
    --) dd=1 ;;
    --check) check=1 ;;
    --fix) fix=1 ;;
  esac
done
status=0
if [ "$sub" = "format" ]; then
  for f in $files; do
    if grep -q 'x=' "$f"; then
      if [ $check -eq 1 ]; then
        echo "Would reformat: $f"
        status=1
      else
        printf 'x = 1\n' > "$f"
      fi
    fi
  done
  exit $status
fi
out=""
for f in $files; do
  if [ $fix -eq 1 ] && grep -q '^import os$' "$f"; then
    grep -v '^import os$' "$f" > "$f.togi-tmp" && mv "$f.togi-tmp" "$f"
  fi
  if grep -q '^import os$' "$f"; then
    [ -n "$out" ] && out="$out,"
    out="$out{\"filename\":\"$f\",\"code\":\"F401\",\"message\":\"\`os\` imported but unused\",\"location\":{\"row\":1,\"column\":8},\"end_location\":{\"row\":1,\"column\":10},\"fix\":{\"applicability\":\"safe\"}}"
    status=1
  fi
  if grep -q 'undefined_name' "$f"; then
    [ -n "$out" ] && out="$out,"
    out="$out{\"filename\":\"$f\",\"code\":\"F821\",\"message\":\"Undefined name \`undefined_name\`\",\"location\":{\"row\":3,\"column\":7},\"end_location\":{\"row\":3,\"column\":21},\"fix\":null}"
    status=1
  fi
done
echo "[$out]"
exit $status
"#;

    /// panache protocol: `format --no-color [--check] [--config F] <files>`
    /// (`Diff in <f>:<line>:` / `Formatted <f>` on stdout) and
    /// `lint --message-format short --no-color [--fix] [--config F] <files>`
    /// (short `path:line:col: severity[code]: message` lines, exit 0).
    const PANACHE_SHIM: &str = r#"#!/bin/sh
sub="$1"; shift
check=0; fix=0; skip=0; files=""
for a in "$@"; do
  if [ $skip -eq 1 ]; then skip=0; continue; fi
  case "$a" in
    --check) check=1 ;;
    --fix) fix=1 ;;
    --config|--message-format) skip=1 ;;
    --*) ;;
    *) files="$files $a" ;;
  esac
done
status=0
if [ "$sub" = "format" ]; then
  for f in $files; do
    if grep -q '   ' "$f"; then
      if [ $check -eq 1 ]; then
        echo "Diff in $f:1:"
        status=1
      else
        sed 's/   */ /g' "$f" > "$f.togi-tmp" && mv "$f.togi-tmp" "$f"
        echo "Formatted $f"
      fi
    fi
  done
  exit $status
fi
for f in $files; do
  if [ $fix -eq 1 ] && grep -q '   ' "$f"; then
    sed 's/   */ /g' "$f" > "$f.togi-tmp" && mv "$f.togi-tmp" "$f"
  fi
  if grep -q '   ' "$f"; then
    echo "$f:3:7: warning[extra-spaces]: paragraph has extra spaces"
  fi
done
exit 0
"#;

    /// sqlfluff protocol: `lint --format json [--rules R] [--dialect D]
    /// [--config P] <files>` (JSON reports, exit 1 on findings) and `format`/`fix`
    /// rewriting in place.
    const SQLFLUFF_SHIM: &str = r#"#!/bin/sh
sub="$1"; shift
skip=0; files=""
for a in "$@"; do
  if [ $skip -eq 1 ]; then skip=0; continue; fi
  case "$a" in
    --format|--rules|--dialect|--config) skip=1 ;;
    --*) ;;
    *) files="$files $a" ;;
  esac
done
if [ "$sub" = "format" ] || [ "$sub" = "fix" ]; then
  for f in $files; do
    if grep -q ' ,' "$f"; then
      sed 's/ ,/,/g' "$f" > "$f.togi-tmp" && mv "$f.togi-tmp" "$f"
    fi
  done
  exit 0
fi
out=""; status=0
for f in $files; do
  if grep -q ' ,' "$f"; then
    [ -n "$out" ] && out="$out,"
    out="$out{\"filepath\":\"$f\",\"violations\":[{\"start_line_no\":1,\"start_line_pos\":9,\"end_line_no\":1,\"end_line_pos\":10,\"code\":\"LT01\",\"description\":\"Unexpected whitespace before comma ','.\",\"name\":\"layout.spacing\",\"warning\":false,\"fixes\":[{\"type\":\"delete\"}]}]}"
    status=1
  fi
done
echo "[$out]"
exit $status
"#;

    /// A fixture sandbox with all four shim tools "installed".
    fn shimmed() -> Sandbox {
        let sb = Sandbox::with_fixture();
        install_shim(&sb, "air", AIR_DEFAULT, AIR_SHIM);
        install_shim(&sb, "ruff", RUFF_DEFAULT, RUFF_SHIM);
        install_shim(&sb, "panache", PANACHE_DEFAULT, PANACHE_SHIM);
        install_shim(&sb, "sqlfluff", SQLFLUFF_DEFAULT, SQLFLUFF_SHIM);
        sb
    }

    // ---- togi format ----------------------------------------------------

    #[test]
    fn format_check_lists_the_files_that_would_change_and_exits_1() {
        let sb = shimmed();
        sb.cmd(&["format", "--check"])
            .assert()
            .code(1)
            .stdout(
                predicate::str::contains("would reformat: messy.R")
                    .and(predicate::str::contains("would reformat: messy.py"))
                    .and(predicate::str::contains("would reformat: report.qmd"))
                    .and(predicate::str::contains("would reformat: notes.md"))
                    .and(predicate::str::contains("would reformat: query.sql"))
                    .and(predicate::str::contains("analysis.R").not())
                    .and(predicate::str::contains("clean.py").not())
                    .and(predicate::str::contains("violations.py").not()),
            )
            .stderr(
                predicate::str::contains("5 of 8 files would be reformatted")
                    .and(predicate::str::contains("hint:"))
                    .and(predicate::str::contains("togi format")),
            );
    }

    #[test]
    fn format_rewrites_in_place_and_a_second_check_is_clean() {
        let sb = shimmed();
        sb.cmd(&["format"])
            .assert()
            .success()
            .stdout(predicate::str::contains("✓ 8 files formatted, 5 changed"));

        // The files really changed on disk.
        assert_eq!(sb.read_file("messy.R"), "x <- 1\n");
        assert_eq!(sb.read_file("messy.py"), "x = 1\n");
        assert_eq!(sb.read_file("query.sql"), "select a,b from tbl\n");
        assert!(!sb.read_file("report.qmd").contains("   "));

        // Formatting is idempotent: a follow-up --check passes quietly.
        sb.cmd(&["format", "--check"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "✓ 8 files checked, nothing would change",
            ));
    }

    #[test]
    fn format_limited_to_an_explicit_clean_file_passes() {
        let sb = shimmed();
        sb.cmd(&["format", "--check", "clean.py"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "✓ 1 file checked, nothing would change",
            ));
    }

    #[test]
    fn format_respects_gitignore() {
        let sb = shimmed();
        sb.write_file(".gitignore", "vendored.py\n");
        sb.write_file("vendored.py", "x=1\n");

        sb.cmd(&["format", "--check"])
            .assert()
            .code(1)
            .stdout(predicate::str::contains("vendored.py").not());
    }

    #[test]
    fn format_respects_config_excludes() {
        let sb = shimmed();
        sb.write_project_config("[format]\nexclude = [\"skip/**\"]\n");
        sb.write_file("skip/awful.R", "x=1\n");

        sb.cmd(&["format", "--check"])
            .assert()
            .code(1)
            .stdout(predicate::str::contains("awful.R").not());
    }

    #[test]
    fn a_crashing_tool_is_reported_without_hiding_the_other_results() {
        let sb = shimmed();
        // sqlfluff "crashes" (exit 2 is a tool error, not findings).
        install_shim(
            &sb,
            "sqlfluff",
            SQLFLUFF_DEFAULT,
            "#!/bin/sh\necho 'sqlfluff exploded' >&2\nexit 2\n",
        );

        sb.cmd(&["format", "--check"])
            .assert()
            .code(1)
            // The healthy adapters' results still print.
            .stdout(
                predicate::str::contains("would reformat: messy.R")
                    .and(predicate::str::contains("would reformat: messy.py")),
            )
            .stderr(
                predicate::str::contains("SQL formatter/linter could not finish")
                    .and(predicate::str::contains("sqlfluff exploded"))
                    .and(predicate::str::contains("1 formatter could not run")),
            );
    }

    // ---- togi lint --------------------------------------------------------

    #[test]
    fn lint_reports_ruff_style_diagnostics_and_exits_1() {
        let sb = shimmed();
        sb.cmd(&["lint"])
            .assert()
            .code(1)
            .stdout(
                predicate::str::contains("violations.py:1:8: F401 [*] `os` imported but unused")
                    .and(predicate::str::contains(
                        "violations.py:3:7: F821 Undefined name `undefined_name`",
                    ))
                    .and(predicate::str::contains(
                        "messy.R: [*] file is not formatted",
                    ))
                    .and(predicate::str::contains(
                        "report.qmd:3:7: extra-spaces paragraph has extra spaces",
                    ))
                    .and(predicate::str::contains(
                        "query.sql:1:9: LT01 [*] Unexpected whitespace before comma ','.",
                    ))
                    // Plain markdown is not linted by default ([lint]
                    // languages excludes it), even though format covers it.
                    .and(predicate::str::contains("notes.md").not()),
            )
            .stderr(
                predicate::str::contains("found 5 issues")
                    .and(predicate::str::contains("togi lint --fix")),
            );
    }

    #[test]
    fn lint_on_a_clean_project_is_quietly_successful() {
        let sb = shimmed();
        // Format everything first so no drift or violation remains...
        sb.cmd(&["format"]).assert().success();
        // ...except the unfixable F821, which we drop entirely.
        sb.write_file("violations.py", "print(1)\n");

        sb.cmd(&["lint"])
            .assert()
            .success()
            .stdout(predicate::str::contains("✓ no issues found in 7 files"));
    }

    #[test]
    fn lint_fix_applies_safe_fixes_and_reports_the_rest() {
        let sb = shimmed();
        sb.cmd(&["lint", "--fix"])
            .assert()
            .code(1)
            .stdout(
                predicate::str::contains("F821")
                    .and(predicate::str::contains("F401").not())
                    .and(predicate::str::contains("LT01").not()),
            )
            .stderr(
                predicate::str::contains("found 1 issue")
                    // The safe fixes were already applied; do not send the
                    // user back to --fix.
                    .and(predicate::str::contains("--fix").not()),
            );

        // The fixes really landed: the unused import is gone, the R file
        // and SQL file were reformatted.
        assert!(!sb.read_file("violations.py").contains("import os"));
        assert_eq!(sb.read_file("messy.R"), "x <- 1\n");
        assert_eq!(sb.read_file("query.sql"), "select a,b from tbl\n");
    }

    #[test]
    fn lint_json_emits_only_the_stable_diagnostic_schema_on_stdout() {
        let sb = shimmed();
        let assert = sb.cmd(&["lint", "--format", "json"]).assert().code(1);
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");

        // The whole of stdout is one JSON document — nothing else.
        let diagnostics: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout parses as JSON");
        let items = diagnostics.as_array().expect("a JSON array");
        assert_eq!(items.len(), 5, "{stdout}");
        for item in items {
            let object = item.as_object().expect("each diagnostic is an object");
            // Every key of the stable schema is present on every entry.
            for key in ["path", "range", "code", "severity", "message", "fixable"] {
                assert!(object.contains_key(key), "missing `{key}` in {item}");
            }
        }
        let f401 = items
            .iter()
            .find(|item| item["code"] == "F401")
            .expect("the ruff finding is present");
        assert_eq!(f401["path"], "violations.py");
        assert_eq!(f401["severity"], "warning");
        assert_eq!(f401["fixable"], true);
        assert_eq!(f401["range"]["start"]["line"], 1);
        assert_eq!(f401["range"]["start"]["col"], 8);
    }

    #[test]
    fn lint_json_with_diagnostics_survives_quiet() {
        let sb = shimmed();
        let assert = sb
            .cmd(&["lint", "--quiet", "--format", "json"])
            .assert()
            .code(1);
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
        let diagnostics: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout parses as JSON under --quiet");
        let items = diagnostics.as_array().expect("a JSON array");
        assert_eq!(items.len(), 5, "{stdout}");
    }

    #[test]
    fn lint_languages_config_narrows_the_run() {
        let sb = shimmed();
        sb.write_project_config("[lint]\nlanguages = [\"python\"]\n");

        sb.cmd(&["lint"])
            .assert()
            .code(1)
            .stdout(
                predicate::str::contains("F401")
                    .and(predicate::str::contains("query.sql").not())
                    .and(predicate::str::contains("report.qmd").not())
                    .and(predicate::str::contains("messy.R").not()),
            )
            .stderr(predicate::str::contains("found 2 issues"));
    }

    #[test]
    fn unknown_language_names_in_config_warn_but_do_not_abort() {
        let sb = shimmed();
        sb.write_project_config("[lint]\nlanguages = [\"python\", \"julia\"]\n");

        sb.cmd(&["lint"])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("julia").and(predicate::str::contains("warning:")));
    }

    // ---- diagnostic path normalization (ruff reports absolute paths) ------

    /// A ruff `check` that mimics real ruff by reporting an *absolute*
    /// `filename` (the working directory joined with each input), so the
    /// path-normalization is exercised end to end. `format` is a no-op.
    const RUFF_ABSOLUTE_PATH_SHIM: &str = r#"#!/bin/sh
sub="$1"; shift
dd=0; files=""
for a in "$@"; do
  if [ $dd -eq 1 ]; then files="$files $a"; continue; fi
  case "$a" in --) dd=1 ;; esac
done
[ "$sub" = "format" ] && exit 0
here=$(pwd)
out=""
for f in $files; do
  [ -n "$out" ] && out="$out,"
  out="$out{\"filename\":\"$here/$f\",\"code\":\"F401\",\"message\":\"\`os\` imported but unused\",\"location\":{\"row\":1,\"column\":8},\"end_location\":{\"row\":1,\"column\":10},\"fix\":{\"applicability\":\"safe\"}}"
done
echo "[$out]"
exit 1
"#;

    /// A python-only sandbox with just the ruff shim installed, so no other
    /// adapter tries to resolve a (missing) tool.
    fn python_only(shim: &str) -> Sandbox {
        let sb = Sandbox::empty();
        sb.write_file("violations.py", "import os\n");
        install_shim(&sb, "ruff", RUFF_DEFAULT, shim);
        sb
    }

    #[test]
    fn lint_json_reports_project_relative_paths_even_when_ruff_is_absolute() {
        let sb = python_only(RUFF_ABSOLUTE_PATH_SHIM);
        let assert = sb.cmd(&["lint", "--format", "json"]).assert().code(1);
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
        let diagnostics: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout parses as JSON");
        let items = diagnostics.as_array().expect("a JSON array");
        assert_eq!(items.len(), 1, "{stdout}");
        // The absolute path ruff emitted is normalized to project-relative.
        assert_eq!(items[0]["path"], "violations.py", "{stdout}");
    }

    #[test]
    fn lint_human_output_relativizes_absolute_ruff_paths() {
        let sb = python_only(RUFF_ABSOLUTE_PATH_SHIM);
        sb.cmd(&["lint"]).assert().code(1).stdout(
            // The line starts at the relative path, with no absolute prefix.
            predicate::str::contains("violations.py:1:8: F401")
                .and(predicate::str::contains(sb.project.display().to_string()).not()),
        );
    }

    // ---- ruff leaves no .ruff_cache in the project (--no-cache) -----------

    /// A ruff shim that writes a `.ruff_cache/` in its working directory
    /// *unless* `--no-cache` was passed — exactly the litter the flag is
    /// meant to suppress. Both subcommands succeed cleanly.
    const RUFF_CACHE_PROBE_SHIM: &str = r#"#!/bin/sh
nocache=0
for a in "$@"; do case "$a" in --no-cache) nocache=1 ;; esac; done
[ $nocache -eq 0 ] && mkdir -p .ruff_cache
sub="$1"
[ "$sub" = "check" ] && echo "[]"
exit 0
"#;

    #[test]
    fn ruff_runs_leave_no_ruff_cache_in_the_project() {
        let sb = python_only(RUFF_CACHE_PROBE_SHIM);

        // Both a format and a lint pass go through ruff; neither may create
        // the cache directory in the user's project.
        sb.cmd(&["format"]).assert().success();
        assert!(
            !sb.project.join(".ruff_cache").exists(),
            "togi format left a .ruff_cache in the project"
        );
        sb.cmd(&["lint"]).assert().success();
        assert!(
            !sb.project.join(".ruff_cache").exists(),
            "togi lint left a .ruff_cache in the project"
        );
    }

    // ---- -v logs each underlying tool invocation --------------------------

    #[test]
    fn verbose_logs_the_underlying_tool_invocations_on_stderr() {
        let sb = shimmed();
        sb.cmd(&["lint", "-v"]).assert().code(1).stderr(
            // The dimmed "running" prefix plus argv of several real tools.
            predicate::str::contains("running")
                .and(predicate::str::contains(
                    "check --no-cache --output-format json",
                ))
                .and(predicate::str::contains("lint --message-format short")),
        );
    }

    #[test]
    fn format_verbose_logs_the_underlying_tool_invocations_on_stderr() {
        let sb = shimmed();
        sb.cmd(&["format", "-v"]).assert().success().stderr(
            // The dimmed "running" prefix plus argv of the format-path tools
            // (ruff formats with --no-cache, air/panache with --no-color);
            // the lint path would instead log `check`/`lint` subcommands.
            predicate::str::contains("running")
                .and(predicate::str::contains("format --no-cache"))
                .and(predicate::str::contains("format --no-color")),
        );
    }

    #[test]
    fn verbose_never_pollutes_the_json_on_stdout() {
        // -v output goes to stderr, so a machine consumer's stdout is still
        // nothing but the stable JSON schema.
        let sb = shimmed();
        let assert = sb.cmd(&["lint", "-v", "--format", "json"]).assert().code(1);
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
        let diagnostics: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout parses as JSON under -v");
        assert!(diagnostics.as_array().is_some(), "{stdout}");
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf-8 stderr");
        assert!(
            stderr.contains("running"),
            "the invocation log still lands on stderr: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// The real managed tools, end to end. Downloads air, ruff, panache, and
// uv + sqlfluff into a scratch cache on first use.
// Run with: `cargo test --features online-tests -- --ignored`

#[cfg(feature = "online-tests")]
mod online {
    use super::*;

    impl Sandbox {
        /// An empty home directory inside the sandbox, so neither togi nor
        /// the real tools read the developer's own configuration.
        fn fake_home(&self) -> PathBuf {
            let home = self._root.path().join("home");
            fs::create_dir_all(&home).expect("create fake home");
            home
        }

        /// `togi <args...>` with network access and `home` as the home
        /// directory (and no XDG config outside it).
        fn cmd_with_home(&self, home: &Path, args: &[&str]) -> Command {
            let mut cmd = self.cmd_allowing_network(args);
            cmd.env("HOME", home)
                .env("USERPROFILE", home)
                .env("XDG_CONFIG_HOME", home.join(".config"));
            cmd
        }
    }

    #[test]
    #[ignore = "downloads the real managed tools from the network"]
    fn real_tools_format_and_lint_the_mixed_project_end_to_end() {
        let sb = Sandbox::with_fixture();
        let home = sb.fake_home();

        // format --check flags the misformatted files of every language.
        sb.cmd_with_home(&home, &["format", "--check"])
            .assert()
            .code(1)
            .stdout(
                predicate::str::contains("messy.R")
                    .and(predicate::str::contains("messy.py"))
                    .and(predicate::str::contains("report.qmd"))
                    .and(predicate::str::contains("query.sql")),
            );

        // lint reports real diagnostics (ruff's F401/F821 at minimum).
        sb.cmd_with_home(&home, &["lint"]).assert().code(1).stdout(
            predicate::str::contains("violations.py").and(predicate::str::contains("F401")),
        );

        // --format json emits nothing but the stable schema on stdout.
        let assert = sb
            .cmd_allowing_network(&["lint", "--format", "json"])
            .assert()
            .code(1);
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
        let diagnostics: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout parses as JSON");
        let items = diagnostics.as_array().expect("a JSON array");
        assert!(!items.is_empty(), "{stdout}");
        for item in items {
            for key in ["path", "range", "code", "severity", "message", "fixable"] {
                assert!(
                    item.as_object().is_some_and(|o| o.contains_key(key)),
                    "missing `{key}` in {item}"
                );
            }
        }
        assert!(items.iter().any(|item| item["code"] == "F401"), "{stdout}");

        // Formatting in place converges: afterwards --check passes.
        sb.cmd_with_home(&home, &["format"]).assert().success();
        sb.cmd_with_home(&home, &["format", "--check"])
            .assert()
            .success()
            .stdout(predicate::str::contains("nothing would change"));
    }
}
