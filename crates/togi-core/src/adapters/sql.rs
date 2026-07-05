//! The SQL adapter: sqlfluff behind the [`Formatter`]/[`Linter`] traits.
//!
//! sqlfluff has no `format --check`, so check mode runs
//! `sqlfluff lint --format json` restricted to [`FORMAT_RULES`] — the same
//! rule subset the `format` subcommand force-applies — and reports files
//! with fixable violations as "would change". In-place formatting detects
//! changes by comparing file contents before and after the run instead of
//! parsing sqlfluff's human-oriented output.
//!
//! The configured `[sql] dialect` is passed as `--dialect` only when the
//! project has no sqlfluff configuration of its own; when it does,
//! sqlfluff's own config discovery wins. `[tools.sqlfluff] args` from
//! togi.toml are appended to every invocation as the escape hatch.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context;
use serde::Deserialize;

use crate::adapters::{
    Diagnostic, FormatOutcome, Formatter, Linter, Position, Range, Severity, ToolCtx,
};
use crate::config::Config;
use crate::term::HintExt;

/// The sqlfluff adapter; stateless, so one instance serves every run.
pub struct SqlFluffAdapter;

const TOOL: &str = "sqlfluff";

/// The rule subset sqlfluff's own `format` subcommand force-applies
/// (its command implementation hardwires this list). Check mode lints
/// against exactly this set so "would change" matches what `format`
/// would actually rewrite.
const FORMAT_RULES: &str = "capitalisation,layout,ambiguous.union,convention.not_equal,\
                            convention.coalesce,convention.select_trailing_comma,\
                            convention.is_null,jinja.padding,structure.distinct";

/// Violation codes sqlfluff emits for files it could not parse or
/// template: hard failures, not style findings a formatter could fix.
const PARSE_ERROR_CODES: [&str; 2] = ["PRS", "TMP"];

/// The shared what-to-do-next for sqlfluff usage/config failures.
const CONFIG_HINT: &str = "check `[sql] dialect` and `[tools.sqlfluff] args` in togi.toml \
                           (or the project's own sqlfluff config), then rerun";

impl crate::adapters::Adapter for SqlFluffAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }
}

impl Formatter for SqlFluffAdapter {
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome> {
        // No files means nothing to do; sqlfluff invoked without paths
        // would scan the whole working directory instead.
        if files.is_empty() {
            return Ok(FormatOutcome::default());
        }
        if check {
            check_format(files, ctx)
        } else {
            apply_format(files, ctx)
        }
    }
}

impl Linter for SqlFluffAdapter {
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        if fix {
            let output = run(&["fix"], files, ctx)?;
            // Exit 1 only means violations remain (unfixable findings or
            // parse errors); the follow-up lint below reports them.
            if exit_code(&output) > 1 {
                return Err(tool_error("sqlfluff fix", &output)).hint(CONFIG_HINT);
            }
        }
        let output = run(&["lint", "--format", "json"], files, ctx)?;
        if exit_code(&output) > 1 {
            return Err(tool_error("sqlfluff lint", &output)).hint(CONFIG_HINT);
        }
        let reports = parse_reports(&String::from_utf8_lossy(&output.stdout))?;
        Ok(reports_to_diagnostics(&reports))
    }
}

/// Format in place, then report which files actually changed by
/// comparing their contents around the run (sqlfluff's stdout is
/// human-oriented and not worth parsing for this).
fn apply_format(files: &[PathBuf], ctx: &ToolCtx) -> anyhow::Result<FormatOutcome> {
    let before: Vec<Vec<u8>> = files
        .iter()
        .map(|f| read_for_change_detection(f))
        .collect::<anyhow::Result<_>>()?;
    let output = run(&["format"], files, ctx)?;
    match exit_code(&output) {
        0 => {}
        // Exit 1 from `format` means templating/parse errors: sqlfluff
        // formatted what it could and skipped the broken files.
        1 => {
            return Err(tool_error("sqlfluff format", &output)).hint(
                "some SQL could not be parsed; run `togi lint` for the file \
                 and line, fix the syntax, then rerun `togi format`",
            );
        }
        _ => return Err(tool_error("sqlfluff format", &output)).hint(CONFIG_HINT),
    }
    let mut changed = Vec::new();
    for (file, old) in files.iter().zip(before) {
        if read_for_change_detection(file)? != old {
            changed.push(file.clone());
        }
    }
    Ok(FormatOutcome {
        processed: files.len(),
        changed,
    })
}

/// Check mode: `sqlfluff format` has no `--check`, so lint against the
/// same rule subset it would apply and report the files that would
/// change without touching anything.
fn check_format(files: &[PathBuf], ctx: &ToolCtx) -> anyhow::Result<FormatOutcome> {
    let output = run(
        &["lint", "--format", "json", "--rules", FORMAT_RULES],
        files,
        ctx,
    )?;
    // Exit 1 is "violations found", which is exactly what we are asking.
    if exit_code(&output) > 1 {
        return Err(tool_error("sqlfluff lint", &output)).hint(CONFIG_HINT);
    }
    let reports = parse_reports(&String::from_utf8_lossy(&output.stdout))?;
    Ok(FormatOutcome {
        processed: files.len(),
        changed: would_change(&reports)?,
    })
}

/// Resolve the managed sqlfluff and run one invocation over the whole
/// batch.
fn run(subcommand: &[&str], files: &[PathBuf], ctx: &ToolCtx) -> anyhow::Result<Output> {
    let binary = ctx.tool_path(TOOL)?;
    let args = build_args(subcommand, files, ctx.config);
    crate::adapters::log_command(ctx, &binary, &args);
    Command::new(&binary)
        .args(&args)
        .output()
        .with_context(|| format!("could not run `{}`", binary.display()))
        .hint("run `togi tools clean` to reset the managed tool cache, then rerun")
}

/// The process exit code; a signal death counts as a hard error.
fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(2)
}

/// A failure error carrying whatever the tool said (stderr first, else
/// stdout) so the user sees sqlfluff's own report.
fn tool_error(what: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = match stderr.trim() {
        "" => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        s => s.to_string(),
    };
    anyhow::anyhow!("`{what}` failed (exit {}): {said}", exit_code(output))
}

/// Read one file for before/after change comparison.
fn read_for_change_detection(file: &Path) -> anyhow::Result<Vec<u8>> {
    fs::read(file)
        .with_context(|| {
            format!(
                "could not read `{}` to detect formatting changes",
                file.display()
            )
        })
        .hint("check that the file exists and is readable, or add it to `[format] exclude` in togi.toml")
}

/// The full sqlfluff command line for one invocation: subcommand, togi
/// defaults, the `[tools.sqlfluff] args` escape hatch (last, so it can
/// override the defaults), then the files.
fn build_args(subcommand: &[&str], files: &[PathBuf], config: &Config) -> Vec<OsString> {
    let mut args: Vec<OsString> = subcommand.iter().map(OsString::from).collect();
    args.push("--disable-progress-bar".into());
    // Only supply the configured dialect when the project has not
    // configured sqlfluff itself; project config passthrough wins.
    if !project_has_sqlfluff_config(files) {
        args.push("--dialect".into());
        args.push(config.sql.dialect.as_str().into());
    }
    if let Some(extra) = config.tools.args.get(TOOL) {
        args.extend(extra.iter().map(OsString::from));
    }
    args.extend(files.iter().map(|f| f.as_os_str().to_os_string()));
    args
}

/// Whether any of `files` sits in a project that configures sqlfluff
/// itself: walk each file's ancestors (mirroring sqlfluff's own config
/// discovery), stopping at the repository root (`.git`) so configuration
/// outside the project does not count.
fn project_has_sqlfluff_config(files: &[PathBuf]) -> bool {
    files.iter().any(|file| {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if dir_has_sqlfluff_config(d) {
                return true;
            }
            if d.join(".git").exists() {
                return false;
            }
            dir = d.parent();
        }
        false
    })
}

/// Whether `dir` contains sqlfluff configuration: a `.sqlfluff` file, or
/// one of the shared Python config files with a sqlfluff section.
fn dir_has_sqlfluff_config(dir: &Path) -> bool {
    if dir.join(".sqlfluff").is_file() {
        return true;
    }
    let sections: [(&str, &str); 4] = [
        ("setup.cfg", "[sqlfluff"),
        ("tox.ini", "[sqlfluff"),
        ("pep8.ini", "[sqlfluff"),
        ("pyproject.toml", "[tool.sqlfluff"),
    ];
    sections.iter().any(|(name, section)| {
        fs::read_to_string(dir.join(name)).is_ok_and(|text| text.contains(section))
    })
}

/// One file's entry in `sqlfluff lint --format json` output.
#[derive(Debug, Deserialize)]
struct FileReport {
    filepath: String,
    #[serde(default)]
    violations: Vec<Violation>,
}

/// One violation as sqlfluff reports it; every field is defaulted so a
/// sparse entry (e.g. a parse error, which carries no `fixes`) still
/// parses.
#[derive(Debug, Default, Deserialize)]
struct Violation {
    #[serde(default)]
    start_line_no: Option<u32>,
    #[serde(default)]
    start_line_pos: Option<u32>,
    #[serde(default)]
    end_line_no: Option<u32>,
    #[serde(default)]
    end_line_pos: Option<u32>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    warning: bool,
    #[serde(default)]
    fixes: Vec<serde::de::IgnoredAny>,
}

/// Parse `sqlfluff lint --format json` stdout.
fn parse_reports(stdout: &str) -> anyhow::Result<Vec<FileReport>> {
    serde_json::from_str(stdout.trim())
        .context("could not parse sqlfluff's JSON lint output")
        .hint(
            "this togi build may not match the installed sqlfluff; run \
             `togi tools update`, or pin a compatible `[tools] sqlfluff` \
             version in togi.toml",
        )
}

/// Every violation in `reports`, normalized into [`Diagnostic`]s.
fn reports_to_diagnostics(reports: &[FileReport]) -> Vec<Diagnostic> {
    reports
        .iter()
        .flat_map(|report| {
            report
                .violations
                .iter()
                .map(|violation| to_diagnostic(&report.filepath, violation))
        })
        .collect()
}

fn to_diagnostic(filepath: &str, violation: &Violation) -> Diagnostic {
    let position = |line: Option<u32>, col: Option<u32>| match (line, col) {
        (Some(line), Some(col)) => Some(Position { line, col }),
        _ => None,
    };
    let range = position(violation.start_line_no, violation.start_line_pos).map(|start| Range {
        start,
        end: position(violation.end_line_no, violation.end_line_pos),
    });
    Diagnostic {
        path: PathBuf::from(filepath),
        range,
        code: violation.code.clone(),
        severity: if violation.warning {
            Severity::Warning
        } else {
            Severity::Error
        },
        message: violation.description.clone(),
        fixable: !violation.fixes.is_empty(),
    }
}

/// Whether a violation is a templating/parse failure rather than a style
/// finding.
fn is_parse_error(violation: &Violation) -> bool {
    violation
        .code
        .as_deref()
        .is_some_and(|code| PARSE_ERROR_CODES.contains(&code))
}

/// Check-mode change detection over parsed lint output: files with
/// fixable violations under [`FORMAT_RULES`] would change; unparsable
/// files are an error, matching what running `format` on them would be.
fn would_change(reports: &[FileReport]) -> anyhow::Result<Vec<PathBuf>> {
    let unparsable: Vec<&str> = reports
        .iter()
        .filter(|report| report.violations.iter().any(is_parse_error))
        .map(|report| report.filepath.as_str())
        .collect();
    if !unparsable.is_empty() {
        return Err(anyhow::anyhow!(
            "sqlfluff could not parse: {}",
            unparsable.join(", ")
        ))
        .hint(
            "fix the SQL syntax in the listed files (`togi lint` shows the \
             failing lines), then rerun",
        );
    }
    Ok(reports
        .iter()
        .filter(|report| {
            report
                .violations
                .iter()
                .any(|violation| !violation.fixes.is_empty())
        })
        .map(|report| PathBuf::from(&report.filepath))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::adapters::{Formatter, Linter};

    /// A recorded sqlfluff output from
    /// `tests/fixtures/tool-output/sqlfluff/`.
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/sqlfluff")
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    // ---- recorded-output parsing -------------------------------------

    #[test]
    fn recorded_lint_violations_parse_into_normalized_diagnostics() {
        let reports = parse_reports(&fixture("lint-violations.json")).expect("parses");
        let diagnostics = reports_to_diagnostics(&reports);

        // clean.sql contributes nothing; events.sql has five findings.
        assert_eq!(diagnostics.len(), 5);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.path == Path::new("events.sql")),
            "{diagnostics:?}"
        );

        let first = &diagnostics[0];
        assert_eq!(first.code.as_deref(), Some("LT09"));
        assert_eq!(first.severity, Severity::Error);
        assert!(first.fixable, "LT09 comes with fixes: {first:?}");
        assert!(
            first.message.contains("Select targets"),
            "keeps sqlfluff's description: {first:?}"
        );
        let range = first.range.expect("LT09 has positions");
        assert_eq!(range.start, Position { line: 1, col: 1 });
        assert_eq!(range.end, Some(Position { line: 3, col: 18 }));
    }

    #[test]
    fn recorded_parse_errors_become_error_diagnostics() {
        let reports = parse_reports(&fixture("lint-parse-error.json")).expect("parses");
        let diagnostics = reports_to_diagnostics(&reports);

        assert_eq!(diagnostics.len(), 1);
        let prs = &diagnostics[0];
        assert_eq!(prs.path, PathBuf::from("broken.sql"));
        assert_eq!(prs.code.as_deref(), Some("PRS"));
        assert_eq!(prs.severity, Severity::Error);
        assert!(!prs.fixable, "parse errors carry no fixes: {prs:?}");
        let range = prs.range.expect("PRS still has a position");
        assert_eq!(range.start, Position { line: 1, col: 19 });
    }

    #[test]
    fn check_mode_flags_files_with_fixable_violations_as_would_change() {
        let reports = parse_reports(&fixture("lint-violations.json")).expect("parses");
        let changed = would_change(&reports).expect("no parse errors in this recording");
        assert_eq!(changed, vec![PathBuf::from("events.sql")]);
    }

    #[test]
    fn check_mode_errors_on_unparsable_files() {
        // The format-rules recording includes broken.sql, whose PRS
        // violation means `sqlfluff format` would fail on it too.
        let reports = parse_reports(&fixture("lint-format-rules.json")).expect("parses");
        let err = would_change(&reports).expect_err("parse errors must not pass silently");
        let rendered = crate::term::render_error(&err, false);
        assert!(
            rendered.contains("broken.sql"),
            "names the file: {rendered}"
        );
        assert!(rendered.contains("hint:"), "says what to do: {rendered}");
    }

    #[test]
    fn garbled_tool_output_is_an_error_with_a_hint_not_a_panic() {
        let err = parse_reports("not json at all").expect_err("garbage must not parse");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("sqlfluff"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    // ---- dialect flag presence/absence -------------------------------

    /// A directory tree rooted in a tempdir with a `.git` marker, so the
    /// project-config walk stops at the tempdir instead of scanning the
    /// real filesystem above it.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join(".git")).expect("git marker");
        dir
    }

    #[test]
    fn dialect_flag_present_without_project_sqlfluff_config() {
        let dir = project();
        let files = vec![dir.path().join("q.sql")];
        let args = strings(&build_args(&["lint"], &files, &Config::default()));
        let dialect_at = args.iter().position(|a| a == "--dialect");
        let at = dialect_at.expect("no project config, so togi supplies the dialect");
        assert_eq!(args[at + 1], "bigquery", "{args:?}");
    }

    #[test]
    fn configured_dialect_is_passed_through() {
        let dir = project();
        let files = vec![dir.path().join("q.sql")];
        let mut config = Config::default();
        config.sql.dialect = "duckdb".to_string();
        let args = strings(&build_args(&["lint"], &files, &config));
        let at = args.iter().position(|a| a == "--dialect").expect("flag");
        assert_eq!(args[at + 1], "duckdb", "{args:?}");
    }

    #[test]
    fn dialect_flag_absent_with_project_dot_sqlfluff() {
        let dir = project();
        fs::write(dir.path().join(".sqlfluff"), "[sqlfluff]\ndialect = ansi\n")
            .expect("write config");
        let files = vec![dir.path().join("q.sql")];
        let args = strings(&build_args(&["lint"], &files, &Config::default()));
        assert!(
            !args.contains(&"--dialect".to_string()),
            "the project's own config wins: {args:?}"
        );
    }

    #[test]
    fn shared_config_files_count_only_with_a_sqlfluff_section() {
        for (name, with_section, without_section) in [
            ("setup.cfg", "[sqlfluff]\ndialect = ansi\n", "[metadata]\n"),
            ("tox.ini", "[sqlfluff:rules]\nx = y\n", "[tox]\n"),
            (
                "pyproject.toml",
                "[tool.sqlfluff.core]\ndialect = \"ansi\"\n",
                "[tool.ruff]\n",
            ),
        ] {
            let dir = project();
            let files = vec![dir.path().join("q.sql")];

            fs::write(dir.path().join(name), without_section).expect("write");
            let args = strings(&build_args(&["lint"], &files, &Config::default()));
            assert!(
                args.contains(&"--dialect".to_string()),
                "{name} without a sqlfluff section is not sqlfluff config: {args:?}"
            );

            fs::write(dir.path().join(name), with_section).expect("write");
            let args = strings(&build_args(&["lint"], &files, &Config::default()));
            assert!(
                !args.contains(&"--dialect".to_string()),
                "{name} with a sqlfluff section is project config: {args:?}"
            );
        }
    }

    #[test]
    fn config_in_a_parent_directory_within_the_project_counts() {
        let dir = project();
        fs::write(dir.path().join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let nested = dir.path().join("analysis/queries");
        fs::create_dir_all(&nested).expect("mkdirs");
        let files = vec![nested.join("q.sql")];
        let args = strings(&build_args(&["lint"], &files, &Config::default()));
        assert!(!args.contains(&"--dialect".to_string()), "{args:?}");
    }

    #[test]
    fn config_above_the_git_boundary_is_ignored() {
        // A .sqlfluff outside the repository (e.g. in a parent checkout
        // directory) is not this project's configuration.
        let outer = tempfile::tempdir().expect("tempdir");
        fs::write(outer.path().join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let repo = outer.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("git marker");
        let files = vec![repo.join("q.sql")];
        let args = strings(&build_args(&["lint"], &files, &Config::default()));
        assert!(args.contains(&"--dialect".to_string()), "{args:?}");
    }

    #[test]
    fn tools_sqlfluff_args_are_appended_between_defaults_and_files() {
        let dir = project();
        let file = dir.path().join("q.sql");
        let files = vec![file.clone()];
        let mut config = Config::default();
        config.tools.args.insert(
            TOOL.to_string(),
            vec!["--templater".to_string(), "raw".to_string()],
        );

        let args = strings(&build_args(&["format"], &files, &config));
        let templater_at = args.iter().position(|a| a == "--templater").expect("flag");
        let dialect_at = args.iter().position(|a| a == "--dialect").expect("flag");
        let file_at = args
            .iter()
            .position(|a| *a == file.to_string_lossy())
            .expect("file");
        assert!(
            dialect_at < templater_at && templater_at < file_at,
            "defaults, then escape hatch (so it can override), then files: {args:?}"
        );
        assert_eq!(args[0], "format", "{args:?}");
        assert!(
            args.contains(&"--disable-progress-bar".to_string()),
            "{args:?}"
        );
    }

    // ---- adapter behavior against a scripted tool --------------------

    /// Write an executable `sqlfluff` shell script whose body is `body`.
    #[cfg(unix)]
    fn fake_sqlfluff(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("sqlfluff");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake sqlfluff");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[cfg(unix)]
    fn ctx_with_script<'a>(
        script: &Path,
        provider: &'a mut Option<FakeToolPaths>,
        config: &'a Config,
    ) -> ToolCtx<'a> {
        *provider = Some(FakeToolPaths::with_tool(TOOL, &script.to_string_lossy()));
        ToolCtx::new(provider.as_ref().expect("just set"), config, false)
    }

    #[cfg(unix)]
    #[test]
    fn format_reports_only_content_changed_files() {
        let dir = project();
        let messy = dir.path().join("messy.sql");
        let clean = dir.path().join("clean.sql");
        fs::write(&messy, "select  1\n").expect("write");
        fs::write(&clean, "select 1\n").expect("write");
        // The script rewrites messy.sql only, like a real formatter would.
        let script = fake_sqlfluff(
            dir.path(),
            r#"for arg in "$@"; do
  case "$arg" in
    *messy.sql) printf 'select 1\n' > "$arg" ;;
  esac
done"#,
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let outcome = SqlFluffAdapter
            .format(&[messy.clone(), clean.clone()], false, &ctx)
            .expect("format");

        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.changed, vec![messy]);
    }

    #[cfg(unix)]
    #[test]
    fn format_check_lints_with_the_format_rule_subset() {
        let dir = project();
        let record = dir.path().join("record.txt");
        let json = dir.path().join("out.json");
        fs::write(&json, fixture("lint-violations.json")).expect("write json");
        let sql = dir.path().join("events.sql");
        fs::write(&sql, "select 1\n").expect("write");
        // Real `lint` exits 1 when it finds violations; check mode must
        // tolerate that.
        let script = fake_sqlfluff(
            dir.path(),
            &format!(
                r#"printf '%s ' "$@" > "{record}"
cat "{json}"
exit 1"#,
                record = record.display(),
                json = json.display()
            ),
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let outcome = SqlFluffAdapter
            .format(&[sql], true, &ctx)
            .expect("format --check");

        assert_eq!(outcome.processed, 1);
        assert_eq!(outcome.changed, vec![PathBuf::from("events.sql")]);
        let recorded = fs::read_to_string(&record).expect("script ran");
        assert!(recorded.starts_with("lint "), "{recorded}");
        assert!(recorded.contains("--format json"), "{recorded}");
        assert!(
            recorded.contains(&format!("--rules {FORMAT_RULES}")),
            "check mode mirrors the format rule subset: {recorded}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn format_surfaces_templating_and_parse_failures() {
        let dir = project();
        let sql = dir.path().join("broken.sql");
        fs::write(&sql, "SELECT FROM WHERE (\n").expect("write");
        // Real `format` exits 1 (with a stderr note) on parse errors.
        let script = fake_sqlfluff(
            dir.path(),
            r#"echo "  [1 templating/parsing errors found]" >&2
exit 1"#,
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let err = SqlFluffAdapter
            .format(&[sql], false, &ctx)
            .expect_err("parse errors must fail the format run");
        let rendered = crate::term::render_error(&err, false);
        assert!(
            rendered.contains("templating/parsing errors"),
            "keeps sqlfluff's own report: {rendered}"
        );
        assert!(
            rendered.contains("togi lint"),
            "says what to do: {rendered}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_fix_runs_fix_then_reports_remaining_findings() {
        let dir = project();
        let record = dir.path().join("record.txt");
        let json = dir.path().join("out.json");
        fs::write(&json, fixture("lint-violations.json")).expect("write json");
        let sql = dir.path().join("events.sql");
        fs::write(&sql, "select 1\n").expect("write");
        // `fix` exits 1 when unfixable violations remain; that is not an
        // error, the follow-up lint reports them.
        let script = fake_sqlfluff(
            dir.path(),
            &format!(
                r#"printf '%s\n' "$1" >> "{record}"
if [ "$1" = "lint" ]; then
  cat "{json}"
fi
exit 1"#,
                record = record.display(),
                json = json.display()
            ),
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let diagnostics = SqlFluffAdapter
            .lint(&[sql], true, &ctx)
            .expect("lint --fix");

        assert_eq!(diagnostics.len(), 5);
        let recorded = fs::read_to_string(&record).expect("script ran");
        assert_eq!(recorded, "fix\nlint\n", "fix first, then lint");
    }

    #[cfg(unix)]
    #[test]
    fn lint_tolerates_exit_one_and_parses_diagnostics() {
        let dir = project();
        let json = dir.path().join("out.json");
        fs::write(&json, fixture("lint-parse-error.json")).expect("write json");
        let sql = dir.path().join("broken.sql");
        fs::write(&sql, "SELECT FROM WHERE (\n").expect("write");
        let script = fake_sqlfluff(dir.path(), &format!("cat \"{}\"\nexit 1", json.display()));

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let diagnostics = SqlFluffAdapter.lint(&[sql], false, &ctx).expect("lint");

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code.as_deref(), Some("PRS"));
    }

    #[cfg(unix)]
    #[test]
    fn usage_errors_name_the_config_escape_hatches() {
        let dir = project();
        let sql = dir.path().join("q.sql");
        fs::write(&sql, "select 1\n").expect("write");
        // Exit 2 is sqlfluff's usage/config error (e.g. unknown dialect).
        let script = fake_sqlfluff(
            dir.path(),
            r#"echo "User Error: Unknown dialect 'bigquery2'" >&2
exit 2"#,
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let err = SqlFluffAdapter
            .lint(&[sql], false, &ctx)
            .expect_err("usage errors must fail the run");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("Unknown dialect"), "{rendered}");
        assert!(rendered.contains("togi.toml"), "{rendered}");
    }

    // ---- batch hygiene ------------------------------------------------

    #[test]
    fn empty_batches_never_invoke_the_tool() {
        // sqlfluff with no paths would lint the whole working directory;
        // an empty batch must not even resolve the binary.
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let outcome = SqlFluffAdapter.format(&[], false, &ctx).expect("format");
        assert_eq!(outcome, FormatOutcome::default());
        let outcome = SqlFluffAdapter.format(&[], true, &ctx).expect("check");
        assert_eq!(outcome, FormatOutcome::default());
        let diagnostics = SqlFluffAdapter.lint(&[], true, &ctx).expect("lint");
        assert!(diagnostics.is_empty());
        assert!(provider.requests().is_empty(), "no tool resolution");
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;
    use crate::adapters::{Formatter, Linter, ToolPaths};
    use crate::tools::{InstallContext, Platform, ToolCache, ToolSpec, UvToolInstaller, versions};

    /// A provider that hands out one pre-installed binary.
    struct FixedToolPaths {
        binary: PathBuf,
    }

    impl ToolPaths for FixedToolPaths {
        fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
            assert_eq!(tool, TOOL);
            Ok(self.binary.clone())
        }
    }

    /// Installs real sqlfluff (bootstrapping uv) into a temp cache, then
    /// drives the adapter end to end: lint, format --check, format,
    /// lint --fix.
    /// Run with: `cargo test --features online-tests -- --ignored`
    #[test]
    #[ignore = "downloads real uv and sqlfluff from the network"]
    fn real_sqlfluff_formats_lints_and_fixes_bigquery_sql() {
        let tools_dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(tools_dir.path());
        let platform = Platform::current().expect("supported platform");
        let spec = ToolSpec::builtin(TOOL).expect("sqlfluff is built in");
        let install_ctx = InstallContext {
            label: "SQL linter",
            command: "togi lint",
            verbose: true,
        };
        let binary = UvToolInstaller::new(cache, platform, versions::UV.to_string())
            .ensure_installed(&spec, spec.default_version, &install_ctx)
            .expect("bootstrap uv and install sqlfluff");

        let project = tempfile::tempdir().expect("tempdir");
        fs::create_dir(project.path().join(".git")).expect("git marker");
        let messy = project.path().join("messy.sql");
        fs::write(
            &messy,
            "select event_id,\n    user_id ,\n  event_timestamp\n\
             from `analytics.events`\nWHERE event_date != '2024-01-01'\n",
        )
        .expect("write messy.sql");
        let star = project.path().join("star.sql");
        fs::write(&star, "select * from `analytics.events`\n").expect("write star.sql");
        let files = vec![messy.clone(), star.clone()];

        let provider = FixedToolPaths { binary };
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, true);
        let adapter = SqlFluffAdapter;

        // Lint sees real violations under the default bigquery dialect.
        let diagnostics = adapter.lint(&files, false, &ctx).expect("lint");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.path == messy && d.code.as_deref() == Some("LT02")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.path == star && d.code.as_deref() == Some("AM04")),
            "{diagnostics:?}"
        );

        // Check mode: only the messy file would change.
        let outcome = adapter.format(&files, true, &ctx).expect("format --check");
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.changed, vec![messy.clone()]);

        // Formatting rewrites it in place...
        let outcome = adapter.format(&files, false, &ctx).expect("format");
        assert_eq!(outcome.changed, vec![messy.clone()]);
        // ...after which check mode is clean.
        let outcome = adapter.format(&files, true, &ctx).expect("re-check");
        assert!(outcome.changed.is_empty(), "{outcome:?}");

        // lint --fix applies what it can and reports the unfixable rest
        // (star.sql's `select *`).
        let remaining = adapter.lint(&files, true, &ctx).expect("lint --fix");
        assert!(
            remaining
                .iter()
                .any(|d| d.path == star && d.code.as_deref() == Some("AM04")),
            "{remaining:?}"
        );
        assert!(remaining.iter().all(|d| !d.fixable), "{remaining:?}");
    }
}
