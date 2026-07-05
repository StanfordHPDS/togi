//! The ruff adapter: formats and lints Python through one managed `ruff`
//! binary. Handles both `.py` and `.ipynb` — ruff supports notebooks
//! natively, so the invocations are identical.
//!
//! togi supplies no style opinions of its own: when the project has a
//! `ruff.toml`, `.ruff.toml`, or `[tool.ruff]` in `pyproject.toml`, ruff
//! discovers it natively and we simply stay out of the way. The only knob
//! togi adds is the `[tools.ruff] args` config escape hatch, appended to
//! every invocation.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::Context;
use serde::Deserialize;

use crate::adapters::{
    Adapter, Diagnostic, FormatOutcome, Formatter, Linter, Position, Range, Severity, ToolCtx,
};
use crate::term::HintExt;

/// The stable tool name, used for binary resolution, batching, and the
/// `[tools.ruff]` config key.
const TOOL: &str = "ruff";

/// The Python adapter (see module docs).
pub struct RuffAdapter;

impl Adapter for RuffAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }
}

impl Formatter for RuffAdapter {
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome> {
        if files.is_empty() {
            // Never invoke ruff with no paths: it would scan the whole
            // working directory instead.
            return Ok(FormatOutcome::default());
        }
        let extra = passthrough_args(ctx);

        // A `--check` run first, even when writing: ruff's write mode only
        // prints counts, while check mode names the files that change —
        // which is what `FormatOutcome::changed` reports.
        let output = run_ruff(ctx, &format_args(true, extra, files))?;
        if !ran_cleanly(&output) {
            return Err(tool_failure("ruff format --check", &output));
        }
        let changed = parse_format_check(&String::from_utf8_lossy(&output.stdout));

        if !check && !changed.is_empty() {
            let output = run_ruff(ctx, &format_args(false, extra, files))?;
            if !ran_cleanly(&output) {
                return Err(tool_failure("ruff format", &output));
            }
        }
        Ok(FormatOutcome {
            processed: files.len(),
            changed,
        })
    }
}

impl Linter for RuffAdapter {
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>> {
        if files.is_empty() {
            // See `format`: no paths must never mean "the whole directory".
            return Ok(Vec::new());
        }
        let output = run_ruff(ctx, &check_args(fix, passthrough_args(ctx), files))?;
        if !ran_cleanly(&output) {
            return Err(tool_failure("ruff check", &output));
        }
        parse_check_json(&String::from_utf8_lossy(&output.stdout))
    }
}

/// The `[tools.ruff] args` config escape hatch, if the project set one.
fn passthrough_args<'a>(ctx: &'a ToolCtx) -> &'a [String] {
    ctx.config
        .tools
        .args
        .get(TOOL)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Resolve the managed ruff binary and run it once over the batch.
fn run_ruff(ctx: &ToolCtx, args: &[OsString]) -> anyhow::Result<Output> {
    let binary = ctx.tool_path(TOOL)?;
    crate::adapters::log_command(ctx, &binary, args);
    Command::new(&binary)
        .args(args)
        .output()
        .with_context(|| format!("could not run ruff at {}", binary.display()))
        .hint("reinstall the managed toolchain with `togi tools update` and rerun")
}

/// Whether ruff completed its work: it exits 0 or 1 for "ran fine" (1
/// meaning findings / files that would change) and 2 for tool errors such
/// as unparsable files or bad arguments.
fn ran_cleanly(output: &Output) -> bool {
    matches!(output.status.code(), Some(0) | Some(1))
}

/// A tool-error exit turned into an error carrying ruff's own stderr and a
/// next step.
fn tool_failure(invocation: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = match stderr.trim() {
        "" => "(no error output)",
        trimmed => trimmed,
    };
    let base: anyhow::Result<()> = Err(anyhow::anyhow!(
        "{invocation} failed ({}): {detail}",
        output.status
    ));
    base.hint("fix the problem ruff reported above (or exclude the file) and rerun")
        .expect_err("just built from Err")
}

/// Arguments for `ruff format`: `--no-cache`, the mode flag, the config
/// escape hatch, and the files — nothing else, so project ruff config
/// always wins. See [`no_cache`] for why the cache is disabled.
fn format_args(check: bool, extra: &[String], files: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["format".into(), no_cache()];
    if check {
        args.push("--check".into());
    }
    finish_args(args, extra, files)
}

/// Arguments for `ruff check`: `--no-cache`, JSON output, optional
/// `--fix`, the config escape hatch, and the files — nothing else.
fn check_args(fix: bool, extra: &[String], files: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "check".into(),
        no_cache(),
        "--output-format".into(),
        "json".into(),
    ];
    if fix {
        args.push("--fix".into());
    }
    finish_args(args, extra, files)
}

/// `--no-cache`, passed to every ruff invocation. ruff otherwise writes a
/// `.ruff_cache/` directory into the current project; togi runs are
/// one-shot, so that cache is pure litter (and would show up as an
/// untracked directory in the user's repo). Both `ruff format` and `ruff
/// check` accept this global flag.
fn no_cache() -> OsString {
    "--no-cache".into()
}

/// Append the config escape hatch and the files (after `--`, so odd file
/// names are never mistaken for flags).
fn finish_args(mut args: Vec<OsString>, extra: &[String], files: &[PathBuf]) -> Vec<OsString> {
    args.extend(extra.iter().map(OsString::from));
    args.push("--".into());
    args.extend(files.iter().map(OsString::from));
    args
}

/// The files `ruff format --check` reports it would reformat, from its
/// stdout (`Would reformat: <path>` lines; the trailing summary line is
/// ignored).
fn parse_format_check(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("Would reformat: "))
        .map(PathBuf::from)
        .collect()
}

/// Parse `ruff check --output-format json` stdout into [`Diagnostic`]s.
fn parse_check_json(stdout: &str) -> anyhow::Result<Vec<Diagnostic>> {
    let findings: Vec<RuffFinding> = serde_json::from_str(stdout)
        .context("could not parse ruff's JSON output")
        .hint("this can happen when a project pins an incompatible ruff version; check `[tools] ruff` in togi.toml")?;
    Ok(findings
        .into_iter()
        .map(RuffFinding::into_diagnostic)
        .collect())
}

/// One entry of ruff's JSON output. Unknown fields (`cell`, `noqa_row`,
/// `url`, ...) are ignored; for notebooks the row/column are relative to
/// the cell ruff reports them in.
#[derive(Deserialize)]
struct RuffFinding {
    filename: PathBuf,
    /// Rule code (`F401`); `invalid-syntax` (or `null` in older ruff
    /// versions) for files ruff could not parse.
    code: Option<String>,
    message: String,
    location: Option<RuffPosition>,
    end_location: Option<RuffPosition>,
    fix: Option<RuffFix>,
}

#[derive(Deserialize, Clone, Copy)]
struct RuffPosition {
    row: u32,
    column: u32,
}

#[derive(Deserialize)]
struct RuffFix {
    /// `safe` fixes are what `--fix` applies; `unsafe` and `display` ones
    /// are not, so they do not count as fixable here.
    applicability: Option<String>,
}

impl RuffFinding {
    fn into_diagnostic(self) -> Diagnostic {
        // ruff has no severity field: everything is a violation except
        // syntax errors, which are the one thing the user *must* fix
        // before the tools can do their job.
        let severity = match self.code.as_deref() {
            None | Some("invalid-syntax") => Severity::Error,
            Some(_) => Severity::Warning,
        };
        let fixable = self
            .fix
            .is_some_and(|fix| fix.applicability.as_deref() == Some("safe"));
        Diagnostic {
            path: self.filename,
            range: self.location.map(|start| Range {
                start: position(start),
                end: self.end_location.map(position),
            }),
            code: self.code,
            severity,
            message: self.message,
            fixable,
        }
    }
}

fn position(p: RuffPosition) -> Position {
    Position {
        line: p.row,
        col: p.column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::config::Config;

    /// A recorded ruff output from `tests/fixtures/tool-output/ruff/`.
    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/ruff")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn paths(files: &[&str]) -> Vec<PathBuf> {
        files.iter().map(PathBuf::from).collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn os_strings(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    // ---- invocation building -------------------------------------------

    #[test]
    fn format_args_carry_no_style_opinions() {
        // togi adds nothing beyond `--no-cache`, the mode flag, and the
        // files, so a project's own ruff.toml / [tool.ruff] always wins.
        assert_eq!(
            format_args(false, &[], &paths(&["a.py", "b.ipynb"])),
            os_strings(&["format", "--no-cache", "--", "a.py", "b.ipynb"])
        );
        assert_eq!(
            format_args(true, &[], &paths(&["a.py"])),
            os_strings(&["format", "--no-cache", "--check", "--", "a.py"])
        );
    }

    #[test]
    fn check_args_request_json_and_optionally_fix() {
        assert_eq!(
            check_args(false, &[], &paths(&["a.py"])),
            os_strings(&[
                "check",
                "--no-cache",
                "--output-format",
                "json",
                "--",
                "a.py"
            ])
        );
        assert_eq!(
            check_args(true, &[], &paths(&["a.py", "n.ipynb"])),
            os_strings(&[
                "check",
                "--no-cache",
                "--output-format",
                "json",
                "--fix",
                "--",
                "a.py",
                "n.ipynb"
            ])
        );
    }

    #[test]
    fn every_invocation_disables_the_ruff_cache() {
        // ruff otherwise litters a `.ruff_cache/` into the user's project;
        // both subcommands must carry `--no-cache`.
        for args in [
            format_args(false, &[], &paths(&["a.py"])),
            format_args(true, &[], &paths(&["a.py"])),
            check_args(false, &[], &paths(&["a.py"])),
            check_args(true, &[], &paths(&["a.py"])),
        ] {
            assert!(
                args.contains(&OsString::from("--no-cache")),
                "missing --no-cache in {args:?}"
            );
        }
    }

    #[test]
    fn config_args_escape_hatch_is_appended_before_the_files() {
        let extra = strings(&["--line-length", "100"]);
        assert_eq!(
            format_args(true, &extra, &paths(&["a.py"])),
            os_strings(&[
                "format",
                "--no-cache",
                "--check",
                "--line-length",
                "100",
                "--",
                "a.py"
            ])
        );
        assert_eq!(
            check_args(false, &extra, &paths(&["a.py"])),
            os_strings(&[
                "check",
                "--no-cache",
                "--output-format",
                "json",
                "--line-length",
                "100",
                "--",
                "a.py"
            ])
        );
    }

    // ---- recorded-output parsing: format --check -----------------------

    #[test]
    fn recorded_format_check_output_parses_the_would_reformat_files() {
        let changed = parse_format_check(&fixture("format-check-mixed.txt"));
        assert_eq!(changed, paths(&["misformatted.py", "notebook.ipynb"]));
    }

    #[test]
    fn recorded_format_check_output_with_nothing_to_change_parses_empty() {
        let changed = parse_format_check(&fixture("format-check-clean.txt"));
        assert!(changed.is_empty(), "{changed:?}");
    }

    // ---- recorded-output parsing: check --output-format json -----------

    #[test]
    fn recorded_check_json_parses_paths_lines_codes_and_fixability() {
        let diagnostics =
            parse_check_json(&fixture("check-violations.json")).expect("recorded json parses");
        assert_eq!(diagnostics.len(), 4);

        // Notebook finding: ruff reports the .ipynb path with a
        // cell-relative position.
        let notebook = &diagnostics[0];
        assert_eq!(notebook.path, PathBuf::from("/project/notebook.ipynb"));
        assert_eq!(notebook.code.as_deref(), Some("F401"));
        assert_eq!(notebook.severity, Severity::Warning);
        assert!(notebook.fixable, "F401 has a safe fix");
        let range = notebook.range.expect("has a location");
        assert_eq!(range.start, Position { line: 1, col: 8 });
        assert_eq!(range.end, Some(Position { line: 1, col: 10 }));

        // Plain .py finding with a safe fix.
        let unused = &diagnostics[1];
        assert_eq!(unused.path, PathBuf::from("/project/violations.py"));
        assert_eq!(unused.code.as_deref(), Some("F401"));
        assert_eq!(unused.message, "`os` imported but unused");
        assert_eq!(
            unused.range.expect("has a location").start,
            Position { line: 3, col: 8 }
        );
        assert!(unused.fixable);

        // An unsafe fix is not applied by `--fix`, so it is not "fixable"
        // from the user's point of view.
        let none_compare = &diagnostics[2];
        assert_eq!(none_compare.code.as_deref(), Some("E711"));
        assert!(!none_compare.fixable, "unsafe fixes are not auto-applied");

        // No fix at all.
        let undefined = &diagnostics[3];
        assert_eq!(undefined.code.as_deref(), Some("F821"));
        assert_eq!(undefined.severity, Severity::Warning);
        assert!(!undefined.fixable);
    }

    #[test]
    fn recorded_syntax_errors_parse_as_error_severity() {
        let diagnostics =
            parse_check_json(&fixture("check-syntax-error.json")).expect("recorded json parses");
        assert_eq!(diagnostics.len(), 2);
        for diagnostic in &diagnostics {
            assert_eq!(diagnostic.path, PathBuf::from("/project/syntax_error.py"));
            assert_eq!(diagnostic.code.as_deref(), Some("invalid-syntax"));
            assert_eq!(diagnostic.severity, Severity::Error);
            assert!(!diagnostic.fixable);
        }
        assert_eq!(
            diagnostics[0].message,
            "Expected a parameter or the end of the parameter list"
        );
    }

    #[test]
    fn recorded_clean_check_json_parses_to_no_diagnostics() {
        let diagnostics =
            parse_check_json(&fixture("check-clean.json")).expect("recorded json parses");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn recorded_fix_run_reports_only_the_remaining_findings() {
        // `ruff check --fix` applied the safe F401 fix to the file; the
        // JSON holds what is left for the user to deal with.
        let diagnostics =
            parse_check_json(&fixture("check-fix-remaining.json")).expect("recorded json parses");
        let codes: Vec<&str> = diagnostics
            .iter()
            .filter_map(|d| d.code.as_deref())
            .collect();
        assert_eq!(codes, vec!["E711", "F821"]);
    }

    #[test]
    fn malformed_check_json_is_an_error_not_a_panic() {
        let err = parse_check_json("this is not json").expect_err("malformed output must error");
        assert!(err.to_string().contains("ruff"), "names the tool: {err}");
    }

    // ---- adapter behavior without a process ----------------------------

    #[test]
    fn empty_batches_never_invoke_the_tool() {
        // The provider has no ruff, so any resolution attempt would fail:
        // an empty batch must return without touching it (running ruff
        // with no paths would make it scan the whole working directory).
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let outcome = RuffAdapter
            .format(&[], false, &ctx)
            .expect("empty format batch is a no-op");
        assert_eq!(outcome, FormatOutcome::default());
        let diagnostics = RuffAdapter
            .lint(&[], true, &ctx)
            .expect("empty lint batch is a no-op");
        assert!(diagnostics.is_empty());
        assert!(provider.requests().is_empty(), "tool was never resolved");
    }

    // ---- adapter behavior against a shim binary ------------------------
    //
    // Unix-only: a script shim cannot stand in for ruff on Windows. The
    // parsing and argument-building logic above is covered on every
    // platform; these tests only add the process plumbing.
    #[cfg(unix)]
    mod with_shim {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        /// A fake `ruff` binary: records its argv, prints canned stdout /
        /// stderr, exits with `status`.
        fn shim(dir: &Path, stdout: &str, stderr: &str, status: i32) -> PathBuf {
            let out = dir.join("stdout.txt");
            let err = dir.join("stderr.txt");
            std::fs::write(&out, stdout).expect("write canned stdout");
            std::fs::write(&err, stderr).expect("write canned stderr");
            let bin = dir.join("ruff");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{argv}\"\ncat \"{out}\"\ncat \"{err}\" >&2\nexit {status}\n",
                argv = dir.join("argv.txt").display(),
                out = out.display(),
                err = err.display(),
            );
            std::fs::write(&bin, script).expect("write shim");
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("mark shim executable");
            bin
        }

        fn recorded_argv(dir: &Path) -> Vec<String> {
            std::fs::read_to_string(dir.join("argv.txt"))
                .expect("shim ran")
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn ruff_ctx<'a>(config: &'a Config, provider: &'a FakeToolPaths) -> ToolCtx<'a> {
            ToolCtx::new(provider, config, false)
        }

        #[test]
        fn format_check_reports_the_files_ruff_would_change() {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(
                dir.path(),
                &fixture("format-check-mixed.txt"),
                "",
                1, // ruff format --check exits 1 when files would change
            );
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let files = paths(&["misformatted.py", "clean.py", "notebook.ipynb"]);
            let outcome = RuffAdapter.format(&files, true, &ctx).expect("check runs");

            assert_eq!(outcome.processed, 3);
            assert_eq!(
                outcome.changed,
                paths(&["misformatted.py", "notebook.ipynb"])
            );
            assert_eq!(
                recorded_argv(dir.path()),
                vec![
                    "format",
                    "--no-cache",
                    "--check",
                    "--",
                    "misformatted.py",
                    "clean.py",
                    "notebook.ipynb"
                ]
            );
        }

        #[test]
        fn format_write_reports_what_changed_via_a_preflight_check() {
            // ruff's write mode only prints counts, so the adapter runs a
            // --check preflight to learn *which* files change, then writes.
            // The shim always claims two files would change; the last
            // recorded argv must be the write invocation.
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(dir.path(), &fixture("format-check-mixed.txt"), "", 1);
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let files = paths(&["misformatted.py", "clean.py", "notebook.ipynb"]);
            let outcome = RuffAdapter
                .format(&files, false, &ctx)
                .expect("format runs");

            assert_eq!(outcome.processed, 3);
            assert_eq!(
                outcome.changed,
                paths(&["misformatted.py", "notebook.ipynb"])
            );
            assert_eq!(
                recorded_argv(dir.path()),
                vec![
                    "format",
                    "--no-cache",
                    "--",
                    "misformatted.py",
                    "clean.py",
                    "notebook.ipynb"
                ],
                "the write invocation runs after the preflight"
            );
        }

        #[test]
        fn lint_parses_diagnostics_from_the_json_invocation() {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(
                dir.path(),
                &fixture("check-violations.json"),
                "",
                1, // ruff check exits 1 when it finds violations
            );
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let files = paths(&["violations.py", "clean.py", "notebook.ipynb"]);
            let diagnostics = RuffAdapter.lint(&files, false, &ctx).expect("lint runs");

            assert_eq!(diagnostics.len(), 4);
            assert_eq!(
                recorded_argv(dir.path()),
                vec![
                    "check",
                    "--no-cache",
                    "--output-format",
                    "json",
                    "--",
                    "violations.py",
                    "clean.py",
                    "notebook.ipynb"
                ]
            );
        }

        #[test]
        fn lint_fix_passes_fix_and_reports_the_remainder() {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(dir.path(), &fixture("check-fix-remaining.json"), "", 1);
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let diagnostics = RuffAdapter
                .lint(&paths(&["fixme.py"]), true, &ctx)
                .expect("lint --fix runs");

            let codes: Vec<&str> = diagnostics
                .iter()
                .filter_map(|d| d.code.as_deref())
                .collect();
            assert_eq!(codes, vec!["E711", "F821"]);
            assert_eq!(
                recorded_argv(dir.path()),
                vec![
                    "check",
                    "--no-cache",
                    "--output-format",
                    "json",
                    "--fix",
                    "--",
                    "fixme.py"
                ]
            );
        }

        #[test]
        fn config_escape_hatch_args_reach_the_invocation() {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(dir.path(), "[]", "", 0);
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let mut config = Config::default();
            config.tools.args.insert(
                TOOL.to_string(),
                vec!["--line-length".to_string(), "100".to_string()],
            );
            let ctx = ruff_ctx(&config, &provider);

            RuffAdapter
                .lint(&paths(&["a.py"]), false, &ctx)
                .expect("lint runs");

            assert_eq!(
                recorded_argv(dir.path()),
                vec![
                    "check",
                    "--no-cache",
                    "--output-format",
                    "json",
                    "--line-length",
                    "100",
                    "--",
                    "a.py"
                ]
            );
        }

        #[test]
        fn a_tool_failure_surfaces_stderr_and_a_next_step() {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(
                dir.path(),
                "",
                &fixture("format-check-error.stderr.txt"),
                2, // ruff exits 2 on tool errors (e.g. unparsable file)
            );
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let err = RuffAdapter
                .format(&paths(&["syntax_error.py"]), true, &ctx)
                .expect_err("exit 2 is a tool failure");
            let rendered = crate::term::render_error(&err, false);
            assert!(
                rendered.contains("Failed to parse syntax_error.py"),
                "carries ruff's own stderr: {rendered}"
            );
            assert!(rendered.contains("hint:"), "says what to do: {rendered}");
        }

        #[test]
        fn lint_treats_unexpected_exit_codes_as_failures() {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = shim(dir.path(), "", "ruff panicked", 101);
            let provider = FakeToolPaths::with_tool(TOOL, bin.to_str().expect("utf8 path"));
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let err = RuffAdapter
                .lint(&paths(&["a.py"]), false, &ctx)
                .expect_err("exit 101 is a tool failure");
            assert!(err.to_string().contains("ruff"), "{err}");
        }

        #[test]
        fn a_missing_binary_is_an_error_not_a_panic() {
            let provider = FakeToolPaths::with_tool(TOOL, "/nonexistent/bin/ruff");
            let config = Config::default();
            let ctx = ruff_ctx(&config, &provider);

            let err = RuffAdapter
                .format(&paths(&["a.py"]), true, &ctx)
                .expect_err("missing binary must error");
            let rendered = crate::term::render_error(&err, false);
            assert!(rendered.contains("ruff"), "names the tool: {rendered}");
            assert!(rendered.contains("hint:"), "says what to do: {rendered}");
        }
    }
}

// Downloads the real pinned ruff and drives the adapter end to end.
// Run with: `cargo test --features online-tests -- --ignored`
#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::config::Config;
    use crate::tools::{Downloader, InstallContext, Platform, ToolCache, ToolSpec};

    /// Copy a recorded fixture input into the scratch project.
    fn stage(project: &Path, name: &str) -> PathBuf {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/ruff/input")
            .join(name);
        let dest = project.join(name);
        std::fs::copy(&src, &dest).unwrap_or_else(|e| panic!("stage {name}: {e}"));
        dest
    }

    #[test]
    #[ignore = "downloads a real ruff release from GitHub"]
    fn real_ruff_formats_and_lints_a_fixture_project_end_to_end() {
        // Install the pinned ruff into a scratch cache, exactly as the
        // production provider would.
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let spec = ToolSpec::builtin(TOOL).expect("ruff is built in");
        let ctx = InstallContext {
            label: "Python formatter/linter",
            command: "togi format",
            verbose: true,
        };
        let binary = Downloader::new(
            ToolCache::at(cache_dir.path()),
            Platform::current().expect("supported platform"),
        )
        .ensure_installed(&spec, spec.default_version, &ctx)
        .expect("download and install ruff");

        // A scratch project with a misformatted file, a violating file, a
        // notebook, and a clean file.
        let project = tempfile::tempdir().expect("project tempdir");
        let misformatted = stage(project.path(), "misformatted.py");
        let violations = stage(project.path(), "violations.py");
        let notebook = stage(project.path(), "notebook.ipynb");
        let clean = stage(project.path(), "clean.py");
        let files = vec![
            misformatted.clone(),
            violations.clone(),
            notebook.clone(),
            clean.clone(),
        ];

        let provider = FakeToolPaths::with_tool(TOOL, binary.to_str().expect("utf8 path"));
        let config = Config::default();
        let tool_ctx = ToolCtx::new(&provider, &config, false);

        // format --check flags the misformatted file and the notebook.
        let outcome = RuffAdapter
            .format(&files, true, &tool_ctx)
            .expect("format --check");
        assert_eq!(outcome.processed, files.len());
        assert!(outcome.changed.contains(&misformatted), "{outcome:?}");
        assert!(outcome.changed.contains(&notebook), "{outcome:?}");
        assert!(!outcome.changed.contains(&clean), "{outcome:?}");

        // lint finds the staged violations, including inside the notebook.
        let diagnostics = RuffAdapter.lint(&files, false, &tool_ctx).expect("lint");
        let has = |path: &Path, code: &str| {
            diagnostics
                .iter()
                .any(|d| d.path == path && d.code.as_deref() == Some(code))
        };
        assert!(has(&violations, "F401"), "{diagnostics:?}");
        assert!(has(&violations, "F821"), "{diagnostics:?}");
        assert!(has(&notebook, "F401"), "{diagnostics:?}");

        // Formatting in place reports the same change set, after which a
        // --check run is clean.
        let outcome = RuffAdapter
            .format(&files, false, &tool_ctx)
            .expect("format in place");
        assert!(outcome.changed.contains(&misformatted), "{outcome:?}");
        let after = RuffAdapter
            .format(&files, true, &tool_ctx)
            .expect("format --check after formatting");
        assert!(after.changed.is_empty(), "{after:?}");

        // lint --fix applies the safe fixes and reports only what remains.
        let remaining = RuffAdapter
            .lint(&files, true, &tool_ctx)
            .expect("lint --fix");
        assert!(
            !remaining
                .iter()
                .any(|d| d.path == violations && d.code.as_deref() == Some("F401")),
            "the unused import was auto-fixed: {remaining:?}"
        );
        assert!(
            remaining
                .iter()
                .any(|d| d.path == violations && d.code.as_deref() == Some("F821")),
            "unfixable findings are still reported: {remaining:?}"
        );
        let fixed = std::fs::read_to_string(&violations).expect("read fixed file");
        assert!(!fixed.contains("import os"), "{fixed}");
    }
}
