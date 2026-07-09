//! The Quarto/Markdown adapter: panache formats and lints `.qmd`, `.Rmd`,
//! and `.md` files, routing embedded R/Python chunks to air/ruff.
//!
//! # panache CLI contract (verified against panache 2.60.0)
//!
//! - `panache format <files...>` formats in place; stdout carries one
//!   `Formatted <path>` line per changed file plus a summary line
//!   (`2 files reformatted, 1 file left unchanged`). Exit 0 on success.
//! - `panache format --check <files...>` writes nothing; stdout carries
//!   diff hunks headed by `Diff in <path>:<line>:` (a file with several
//!   separate diff regions repeats its header, hunks separated by `---`).
//!   Exit 1 when anything would change — the same code it uses for real
//!   errors, which instead print `Error: ...` lines to stderr.
//! - `panache lint --message-format short <files...>` prints one
//!   diagnostic per line as `path:line:col: severity[code]: message`,
//!   grouped per file with a trailing `Found N issue(s)` summary per
//!   group. Without `--check` the exit code is 0 even when there are
//!   findings, so a non-zero exit means the run itself failed. `--fix`
//!   applies safe autofixes and reports what remains. Short output does
//!   not say whether a finding is fixable, so [`Diagnostic::fixable`] is
//!   always `false` here.
//! - External formatters are **opt-in** via the `[formatters]` table of
//!   panache's own config. panache discovers that config per input file
//!   (its `--help` understates this; the following was verified against
//!   the real binary): starting from the file path's parent directory *as
//!   given*, it walks upward checking `.panache.toml`, `panache.toml`,
//!   then `.config/panache.toml` in each directory, and stops after the
//!   nearest directory containing `.git` (a relative input path's walk
//!   also ends at the working directory, where its ancestor chain runs
//!   out). Failing all that it reads `$XDG_CONFIG_HOME/panache/config.toml`,
//!   defaulting to `~/.config/panache/config.toml`. The built-in
//!   `air`/`ruff` presets run those commands by name, resolved through
//!   `PATH`; external linters (`[linters]`, e.g. `python = "ruff"`) are
//!   `PATH`-resolved too and have no per-linter `cmd` override.
//!
//! togi therefore resolves panache, air, and ruff through the shared
//! [`ToolCtx`] provider, prepends the managed air/ruff directories to the
//! child's `PATH`, and — only when no panache config exists — passes
//! `--config` pointing at a generated default that enables the `air`/`ruff`
//! presets. A project (or user) with its own panache config keeps full
//! control and still gets the managed binaries via `PATH`. panache's R
//! *linter* preset is jarl, which togi does not manage, so R chunks are
//! format-checked but not chunk-linted.
//!
//! `[tools.panache] args` from `togi.toml` are appended after the built-in
//! flags, before the file list.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use crate::adapters::{
    Adapter, Diagnostic, FormatOutcome, Formatter, Linter, Position, Range, Severity, ToolCtx,
};
use crate::term::HintExt;

/// The default panache config togi supplies when neither the project nor
/// the user has one: it opts the managed tools in for embedded chunks and
/// wraps prose one sentence per line (overriding panache's `reflow`
/// default).
const DEFAULT_CONFIG: &str = "\
[formatters]
r = \"air\"
python = \"ruff\"

[linters]
python = \"ruff\"

[format]
wrap = \"sentence\"
";

/// Formats and lints Quarto/R Markdown/Markdown files via panache.
///
/// One instance serves both the Quarto and Markdown language buckets; the
/// batch runner merges their files into a single panache invocation.
#[derive(Debug, Default)]
pub struct PanacheAdapter {
    /// Test override for where the panache-config search starts;
    /// production mirrors panache and starts from each input file's
    /// parent directory.
    config_search_start: Option<PathBuf>,
    /// Test override for the user-level config panache falls back to;
    /// production uses `$XDG_CONFIG_HOME/panache/config.toml`, defaulting
    /// to `~/.config/panache/config.toml`.
    user_config: Option<PathBuf>,
}

impl PanacheAdapter {
    pub fn new() -> PanacheAdapter {
        PanacheAdapter::default()
    }

    /// An adapter whose config discovery is pinned to `start` and `user`
    /// instead of the real input files and home, so tests never depend on
    /// the machine they run on.
    #[cfg(test)]
    pub(crate) fn with_config_search(start: &Path, user: &Path) -> PanacheAdapter {
        PanacheAdapter {
            config_search_start: Some(start.to_path_buf()),
            user_config: Some(user.to_path_buf()),
        }
    }

    /// An adapter with only the user-level config pinned, so the tree
    /// walk runs from the (absolute) input files exactly as in production.
    #[cfg(test)]
    pub(crate) fn with_user_config(user: &Path) -> PanacheAdapter {
        PanacheAdapter {
            config_search_start: None,
            user_config: Some(user.to_path_buf()),
        }
    }

    /// Whether panache will discover a config on its own for any file in
    /// this batch; when it won't, togi passes a generated default via
    /// `--config`. Any discovered config suppresses the default: panache
    /// treats an explicit `--config` as stronger than a discovered one,
    /// so passing it would silently take control away from the project.
    fn discovered_config(&self, files: &[PathBuf]) -> Option<PathBuf> {
        let starts = match &self.config_search_start {
            Some(start) => vec![start.clone()],
            None => search_starts(files),
        };
        let user = self.user_config.clone().or_else(default_user_config);
        discover_config(&starts, user.as_deref())
    }

    /// Run one panache subcommand over the whole batch and hand back its
    /// output; shared plumbing for both traits.
    fn run(
        &self,
        subcommand_args: &[&str],
        files: &[PathBuf],
        ctx: &ToolCtx,
    ) -> anyhow::Result<std::process::Output> {
        let panache = ctx.tool_path("panache")?;
        let air = ctx.tool_path("air")?;
        let ruff = ctx.tool_path("ruff")?;

        let mut args: Vec<OsString> = subcommand_args.iter().map(OsString::from).collect();

        // Keep the temp file alive until panache has run.
        let mut _default_config = None;
        if self.discovered_config(files).is_none() {
            let file = write_default_config()?;
            args.push("--config".into());
            args.push(file.path().as_os_str().to_owned());
            _default_config = Some(file);
        }

        if let Some(extra) = ctx.config.tools.args.get("panache") {
            args.extend(extra.iter().map(OsString::from));
        }
        args.extend(files.iter().map(|f| f.as_os_str().to_owned()));

        crate::adapters::log_command(ctx, &panache, &args);

        let mut cmd = Command::new(&panache);
        cmd.args(&args);

        // panache finds the embedded-chunk tools by name on PATH; put the
        // managed copies first so they always win.
        cmd.env("PATH", prepend_tool_dirs(&[&air, &ruff])?);

        cmd.output()
            .with_context(|| format!("could not run panache (`{}`)", panache.display()))
            .hint("run `togi tools clean` to reset the tool cache, then retry")
    }
}

impl Formatter for PanacheAdapter {
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome> {
        if files.is_empty() {
            return Ok(FormatOutcome::default());
        }
        let mut args = vec!["format", "--no-color"];
        if check {
            args.push("--check");
        }
        let output = self.run(&args, files, ctx)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // In check mode exit 1 just means "something would change"; real
        // failures are Error lines on stderr (or an unexpected exit code).
        let failed = if check {
            !matches!(output.status.code(), Some(0 | 1)) || stderr_error(&stderr).is_some()
        } else {
            !output.status.success()
        };
        if failed {
            return Err(run_failure("panache format", &output, &stderr));
        }

        let changed = if check {
            parse_check_output(&stdout)
        } else {
            parse_write_output(&stdout)
        };
        Ok(FormatOutcome {
            // panache skips a missing input with a warning and keeps
            // going; the skipped file was not processed.
            processed: files.len().saturating_sub(count_missing_paths(&stderr)),
            changed,
        })
    }
}

impl Linter for PanacheAdapter {
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["lint", "--message-format", "short", "--no-color"];
        if fix {
            args.push("--fix");
        }
        let output = self.run(&args, files, ctx)?;
        // Without --check, findings still exit 0; non-zero is a real
        // failure (unreadable file, bad flag, ...).
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(run_failure("panache lint", &output, &stderr));
        }
        Ok(parse_short_diagnostics(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

impl Adapter for PanacheAdapter {
    fn name(&self) -> &'static str {
        "panache"
    }
}

/// The error for a panache run that failed outright, pointing the user at
/// the next step.
fn run_failure(what: &str, output: &std::process::Output, stderr: &str) -> anyhow::Error {
    let detail = match stderr_error(stderr) {
        Some(lines) => lines,
        None => stderr.trim().to_string(),
    };
    let status = match output.status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    };
    Err::<(), _>(anyhow::anyhow!("{what} failed ({status}):\n{detail}"))
        .hint(
            "check the reported files and any `[tools.panache] args` in togi.toml; \
             rerun with -v for the full tool output",
        )
        .unwrap_err()
}

/// The `Error: ...` lines panache prints to stderr when a run fails, or
/// `None` when there are none.
fn stderr_error(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("Error:"))
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Files named by `Diff in <path>:<line>:` hunk headers in check-mode
/// stdout, each file once, in first-appearance order.
fn parse_check_output(stdout: &str) -> Vec<PathBuf> {
    let mut changed: Vec<PathBuf> = Vec::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("Diff in ") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(':') else {
            continue;
        };
        let Some((path, line_no)) = rest.rsplit_once(':') else {
            continue;
        };
        if line_no.is_empty() || !line_no.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = PathBuf::from(path);
        if !changed.contains(&path) {
            changed.push(path);
        }
    }
    changed
}

/// Files named by `Formatted <path>` lines in write-mode stdout.
fn parse_write_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("Formatted "))
        .map(PathBuf::from)
        .collect()
}

/// Every `path:line:col: severity[code]: message` line in short-format
/// lint stdout, normalized; summary and blank lines are skipped.
fn parse_short_diagnostics(stdout: &str) -> Vec<Diagnostic> {
    stdout.lines().filter_map(parse_short_line).collect()
}

fn parse_short_line(line: &str) -> Option<Diagnostic> {
    // Peel from the right: `]: ` closes the code, `[` opens it, and the
    // remaining head is `path:line:col: severity`.
    let (head, message) = line.split_once("]: ")?;
    let (head, code) = head.rsplit_once('[')?;
    let (location, severity) = head.rsplit_once(": ")?;
    let mut parts = location.rsplitn(3, ':');
    let col: u32 = parts.next()?.parse().ok()?;
    let line_no: u32 = parts.next()?.parse().ok()?;
    let path = parts.next()?;
    if path.is_empty() {
        return None;
    }
    let severity = match severity {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" | "note" => Severity::Info,
        // Anything panache grows later still surfaces, conservatively.
        _ => Severity::Warning,
    };
    Some(Diagnostic {
        path: PathBuf::from(path),
        range: Some(Range {
            start: Position { line: line_no, col },
            end: None,
        }),
        code: Some(code.to_string()),
        severity,
        message: message.to_string(),
        // Short format does not report fixability.
        fixable: false,
    })
}

/// Where panache starts its config walks: each input file's parent
/// directory, as given (panache does not canonicalize, so a bare
/// filename's start is `""` and resolves against the working directory
/// the child inherits), deduplicated in first-appearance order.
fn search_starts(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut starts: Vec<PathBuf> = Vec::new();
    for file in files {
        if let Some(parent) = file.parent()
            && !starts.iter().any(|start| start == parent)
        {
            starts.push(parent.to_path_buf());
        }
    }
    starts
}

/// First existing panache config, mirroring panache's own discovery: the
/// upward walk from each start directory, then the user-level fallback.
fn discover_config(starts: &[PathBuf], user_config: Option<&Path>) -> Option<PathBuf> {
    for start in starts {
        if let Some(found) = find_in_tree(start) {
            return Some(found);
        }
    }
    user_config
        .filter(|path| path.is_file())
        .map(Path::to_path_buf)
}

/// The upward walk from one start directory: `.panache.toml`,
/// `panache.toml`, then `.config/panache.toml` at each level, stopping
/// after the nearest directory that contains `.git` (panache's project
/// boundary; `.git` can be a file in worktrees). A relative start stops
/// where its ancestor chain does — at `""`, the working directory — just
/// like panache's own uncanonicalized walk.
fn find_in_tree(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        for name in [".panache.toml", "panache.toml"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let nested = dir.join(".config").join("panache.toml");
        if nested.is_file() {
            return Some(nested);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// panache's user-level config location: `$XDG_CONFIG_HOME/panache/
/// config.toml` when set, else `~/.config/panache/config.toml`.
fn default_user_config() -> Option<PathBuf> {
    user_config_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()),
    )
}

/// [`default_user_config`] with its environment inputs made explicit. An
/// empty `XDG_CONFIG_HOME` means unset, per the XDG base-dir spec.
fn user_config_path(xdg_config_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = match xdg_config_home {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => home?.join(".config"),
    };
    Some(base.join("panache").join("config.toml"))
}

/// Inputs panache skipped with a `Warning: Path not found:` stderr line;
/// it keeps going after these (only an all-missing batch is fatal), so
/// the skipped files must not count as processed. Surfacing the warning
/// text itself belongs to the command layer once reporting is wired up.
fn count_missing_paths(stderr: &str) -> usize {
    stderr
        .lines()
        .filter(|line| line.starts_with("Warning: Path not found:"))
        .count()
}

/// Write [`DEFAULT_CONFIG`] to a temp file that lives as long as the
/// returned handle.
fn write_default_config() -> anyhow::Result<tempfile::NamedTempFile> {
    let file = tempfile::Builder::new()
        .prefix("togi-panache-")
        .suffix(".toml")
        .tempfile()
        .context("could not create a temporary panache config")
        .hint("check that the system temp directory is writable")?;
    std::fs::write(file.path(), DEFAULT_CONFIG)
        .context("could not write the temporary panache config")
        .hint("check that the system temp directory is writable")?;
    Ok(file)
}

/// The child `PATH`: the directories holding `binaries`, deduplicated, in
/// front of the current `PATH`.
fn prepend_tool_dirs(binaries: &[&Path]) -> anyhow::Result<OsString> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for binary in binaries {
        if let Some(dir) = binary.parent()
            && !dir.as_os_str().is_empty()
            && !dirs.iter().any(|d| d == dir)
        {
            dirs.push(dir.to_path_buf());
        }
    }
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs)
        .context("could not build PATH for panache")
        .hint("a managed tool lives in a directory whose name contains the PATH separator; move the togi tool cache (TOGI_DATA_DIR)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::config::Config;

    /// A recorded panache output from `tests/fixtures/tool-output/panache/`.
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/panache")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn check_diff_fixture_yields_each_changed_file_once() {
        let changed = parse_check_output(&fixture("format-check-diff.txt"));
        // long.md has two separate diff regions in the recording; it must
        // still appear once. The clean README.md must not appear at all.
        assert_eq!(changed, paths(&["report.qmd", "notes.Rmd", "long.md"]));
    }

    #[test]
    fn write_fixture_yields_the_reformatted_files() {
        let changed = parse_write_output(&fixture("format-write.txt"));
        // The summary line ("2 files reformatted, ...") is not a file.
        assert_eq!(changed, paths(&["report.qmd", "notes.Rmd"]));
    }

    #[test]
    fn lint_short_fixture_parses_every_diagnostic() {
        let diagnostics = parse_short_diagnostics(&fixture("lint-short.txt"));
        assert_eq!(diagnostics.len(), 5);

        // A panache-native finding.
        let first = &diagnostics[0];
        assert_eq!(first.path, PathBuf::from("lint.qmd"));
        let range = first.range.expect("has a position");
        assert_eq!((range.start.line, range.start.col), (7, 4));
        assert_eq!(range.end, None);
        assert_eq!(first.code.as_deref(), Some("missing-chunk-labels"));
        assert_eq!(first.severity, Severity::Warning);
        assert_eq!(
            first.message,
            "Executable code chunk has no label; add `#| label: ...`"
        );
        assert!(!first.fixable);

        // A ruff finding from the embedded Python chunk, mapped back into
        // the document.
        let ruff = &diagnostics[1];
        assert_eq!(ruff.code.as_deref(), Some("F401"));
        assert_eq!(ruff.severity, Severity::Info);
        assert_eq!(ruff.message, "`os` imported but unused");

        // The second file's group parses too, and both per-group
        // `Found N issue(s)` summaries were skipped.
        let last = &diagnostics[4];
        assert_eq!(last.path, PathBuf::from("notes.Rmd"));
        assert_eq!(last.code.as_deref(), Some("missing-chunk-labels"));
    }

    #[test]
    fn severity_tokens_map_onto_the_normalized_scale() {
        for (token, expected) in [
            ("error", Severity::Error),
            ("warning", Severity::Warning),
            ("info", Severity::Info),
            ("note", Severity::Info),
            ("someday-new", Severity::Warning),
        ] {
            let line = format!("doc.qmd:3:1: {token}[x1]: something");
            let diagnostic = parse_short_line(&line).expect("parses");
            assert_eq!(diagnostic.severity, expected, "severity token {token}");
        }
    }

    #[test]
    fn non_diagnostic_lines_are_skipped() {
        for line in [
            "",
            "Found 4 issue(s)",
            "Found 4 issue(s) across 1 file(s)",
            "some prose with a colon: in it",
        ] {
            assert_eq!(parse_short_line(line), None, "line {line:?}");
        }
    }

    #[test]
    fn error_fixture_is_detected_as_a_failure() {
        let stderr = fixture("format-error.txt");
        let error = stderr_error(&stderr).expect("Error line present");
        assert_eq!(error, "Error: No supported files found");
        // The Warning line alone is not a failure.
        assert_eq!(stderr_error("Warning: Path not found: nope.qmd\n"), None);
        assert_eq!(stderr_error(""), None);
    }

    #[test]
    fn discover_config_walks_up_then_falls_back_to_the_user_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // A `.git` boundary at the root keeps the walk inside the tempdir
        // no matter what the machine's real filesystem holds above it.
        std::fs::create_dir(root.join(".git")).expect("git boundary");
        let nested = root.join("project").join("docs");
        std::fs::create_dir_all(&nested).expect("mkdirs");
        let starts = [nested.clone()];
        let missing_user = root.join("nowhere").join("config.toml");

        // Nothing anywhere.
        assert_eq!(discover_config(&starts, Some(&missing_user)), None);

        // A user-level config is found only when nothing closer exists.
        let user = root.join("user-config.toml");
        std::fs::write(&user, "").expect("write user config");
        assert_eq!(discover_config(&starts, Some(&user)), Some(user.clone()));

        // A project config in a parent directory wins over the user one.
        let project = root.join("project").join("panache.toml");
        std::fs::write(&project, "").expect("write project config");
        assert_eq!(discover_config(&starts, Some(&user)), Some(project.clone()));

        // A dotted config in the start directory wins over the parent.
        let dotted = nested.join(".panache.toml");
        std::fs::write(&dotted, "").expect("write dotted config");
        assert_eq!(discover_config(&starts, Some(&user)), Some(dotted));
    }

    #[test]
    fn discovery_checks_the_nested_dot_config_candidate_at_each_level() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).expect("git boundary");
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).expect("mkdirs");
        std::fs::create_dir_all(root.join(".config")).expect("mkdirs");
        let nested_cfg = root.join(".config").join("panache.toml");
        std::fs::write(&nested_cfg, "").expect("write nested config");

        // `<dir>/.config/panache.toml` is a discovery candidate at every
        // ancestor level (verified against panache 2.60.0), so a project
        // carrying only that file keeps control and gets no `--config`.
        assert_eq!(
            discover_config(std::slice::from_ref(&docs), None),
            Some(nested_cfg.clone())
        );

        // A plain candidate in the same directory outranks the nested one.
        let plain = root.join(".panache.toml");
        std::fs::write(&plain, "").expect("write plain config");
        assert_eq!(discover_config(&[docs], None), Some(plain));
    }

    #[test]
    fn discovery_stops_at_the_nearest_git_ancestor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outer = dir.path();
        let repo = outer.join("repo");
        let docs = repo.join("docs");
        std::fs::create_dir_all(&docs).expect("mkdirs");
        std::fs::write(outer.join("panache.toml"), "").expect("outer config");

        // Without a boundary the walk reaches the config above the repo...
        assert_eq!(
            discover_config(std::slice::from_ref(&docs), None),
            Some(outer.join("panache.toml"))
        );

        // ...but the nearest `.git` ancestor caps it (a `.git` *file*, as
        // in worktrees, counts too): panache never sees a config above the
        // repo, so togi must not predict one and skip `--config`.
        std::fs::write(repo.join(".git"), "gitdir: elsewhere").expect("git file");
        let user = outer.join("user.toml");
        std::fs::write(&user, "").expect("user config");
        assert_eq!(
            discover_config(std::slice::from_ref(&docs), Some(&user)),
            Some(user)
        );

        // The boundary directory itself is still searched.
        let repo_cfg = repo.join("panache.toml");
        std::fs::write(&repo_cfg, "").expect("repo config");
        assert_eq!(discover_config(&[docs], None), Some(repo_cfg));
    }

    #[test]
    fn search_starts_are_the_files_parents_as_given() {
        let files = paths(&["docs/a.qmd", "docs/b.md", "a.qmd", "notes/c.Rmd"]);
        assert_eq!(
            search_starts(&files),
            vec![
                PathBuf::from("docs"),
                // A bare filename's parent is `""`: the working directory,
                // where panache's uncanonicalized walk also bottoms out.
                PathBuf::from(""),
                PathBuf::from("notes"),
            ]
        );
    }

    #[test]
    fn user_config_prefers_xdg_config_home() {
        let home = PathBuf::from("/home/togi");
        // panache reads `$XDG_CONFIG_HOME/panache/config.toml` before the
        // `~/.config` default.
        assert_eq!(
            user_config_path(Some(OsString::from("/xdg")), Some(home.clone())),
            Some(PathBuf::from("/xdg").join("panache").join("config.toml"))
        );
        // Unset — or empty, which the XDG spec says means unset — falls
        // back to the home default.
        let fallback = home.join(".config").join("panache").join("config.toml");
        assert_eq!(
            user_config_path(None, Some(home.clone())),
            Some(fallback.clone())
        );
        assert_eq!(
            user_config_path(Some(OsString::new()), Some(home)),
            Some(fallback)
        );
        assert_eq!(user_config_path(Some(OsString::new()), None), None);
    }

    #[test]
    fn missing_path_warnings_are_counted_but_not_fatal() {
        let stderr = fixture("format-missing-path.txt");
        assert_eq!(count_missing_paths(&stderr), 1);
        assert_eq!(stderr_error(&stderr), None, "a skipped path is not fatal");
        assert_eq!(count_missing_paths(""), 0);
        // The all-missing recording carries the same warning shape before
        // its fatal Error line.
        assert_eq!(count_missing_paths(&fixture("format-error.txt")), 1);
    }

    #[test]
    fn default_config_enables_managed_tools_and_sentence_wrapping() {
        let value: toml::Table = DEFAULT_CONFIG.parse().expect("default config parses");
        assert_eq!(
            value["formatters"]["r"].as_str(),
            Some("air"),
            "R chunks route to air"
        );
        assert_eq!(value["formatters"]["python"].as_str(), Some("ruff"));
        assert_eq!(value["linters"]["python"].as_str(), Some("ruff"));
        // togi's default prose wrapping is one sentence per line, overriding
        // panache's own `reflow` default.
        assert_eq!(value["format"]["wrap"].as_str(), Some("sentence"));
    }

    #[test]
    fn empty_batches_return_without_touching_the_tools() {
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let adapter = PanacheAdapter::new();

        let outcome = adapter.format(&[], false, &ctx).expect("no-op");
        assert_eq!(outcome, FormatOutcome::default());
        let diagnostics = adapter.lint(&[], false, &ctx).expect("no-op");
        assert!(diagnostics.is_empty());
        assert!(provider.requests().is_empty(), "no tool was resolved");
    }

    /// End-to-end invocation tests against a fake `panache` script that
    /// records its argv and environment; unix-only because the fake is a
    /// shell script.
    #[cfg(unix)]
    mod invocation {
        use super::*;
        use std::fs;
        use std::sync::Mutex;

        /// The adapter reads the real `PATH` when building the child
        /// environment; serialize the tests that assert on it.
        static ENV_LOCK: Mutex<()> = Mutex::new(());

        struct Harness {
            dir: tempfile::TempDir,
            provider: FakeToolPaths,
        }

        impl Harness {
            /// A temp layout with a fake recording `panache` plus stub
            /// `air`/`ruff` binaries in two distinct directories, and a
            /// provider resolving all three.
            fn new(stdout_body: &str, exit_code: i32) -> Harness {
                use std::os::unix::fs::PermissionsExt;
                let dir = tempfile::tempdir().expect("tempdir");
                let record = dir.path().join("record.txt");
                let panache = dir.path().join("panache");
                fs::write(
                    &panache,
                    format!(
                        "#!/bin/sh\n\
                         {{ echo \"args:$@\"; echo \"path:$PATH\"; }} > \"{record}\"\n\
                         cat <<'TOGI_EOF'\n{stdout_body}\nTOGI_EOF\n\
                         exit {exit_code}\n",
                        record = record.display(),
                    ),
                )
                .expect("write fake panache");
                fs::set_permissions(&panache, fs::Permissions::from_mode(0o755))
                    .expect("chmod fake panache");

                let mut provider = FakeToolPaths::default();
                provider.insert("panache", &panache);
                for tool in ["air", "ruff"] {
                    let tool_dir = dir.path().join(format!("{tool}-bin"));
                    fs::create_dir_all(&tool_dir).expect("tool dir");
                    let binary = tool_dir.join(tool);
                    fs::write(&binary, "").expect("stub tool");
                    provider.insert(tool, &binary);
                }
                Harness { dir, provider }
            }

            /// An adapter that searches only inside the harness dir for
            /// panache configs (and finds none unless a test adds one).
            fn adapter(&self) -> PanacheAdapter {
                PanacheAdapter::with_config_search(
                    self.dir.path(),
                    &self.dir.path().join("no-user-config.toml"),
                )
            }

            fn recorded(&self) -> String {
                fs::read_to_string(self.dir.path().join("record.txt"))
                    .expect("fake panache must have run")
            }

            fn recorded_args(&self) -> String {
                self.recorded()
                    .lines()
                    .find_map(|l| l.strip_prefix("args:"))
                    .expect("args recorded")
                    .to_string()
            }
        }

        #[test]
        fn format_invokes_panache_with_default_config_and_managed_tools_on_path() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness = Harness::new("Formatted a.qmd\n1 file reformatted", 0);
            let config = Config::default();
            let ctx = ToolCtx::new(&harness.provider, &config, false);

            let outcome = harness
                .adapter()
                .format(&paths(&["a.qmd", "b.md"]), false, &ctx)
                .expect("format succeeds");

            assert_eq!(outcome.processed, 2);
            assert_eq!(outcome.changed, paths(&["a.qmd"]));

            let args = harness.recorded_args();
            assert!(
                args.starts_with("format --no-color --config "),
                "built-in flags first: {args}"
            );
            assert!(
                args.contains(".toml") && args.ends_with(" a.qmd b.md"),
                "generated config, then files last: {args}"
            );
            assert!(!args.contains("--check"), "{args}");

            // The managed air/ruff dirs lead the child PATH.
            let recorded = harness.recorded();
            let path_line = recorded
                .lines()
                .find_map(|l| l.strip_prefix("path:"))
                .expect("path recorded");
            let entries: Vec<PathBuf> = std::env::split_paths(&OsString::from(path_line)).collect();
            assert_eq!(entries[0], harness.dir.path().join("air-bin"));
            assert_eq!(entries[1], harness.dir.path().join("ruff-bin"));
        }

        #[test]
        fn check_mode_passes_check_and_parses_the_diff_output() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness =
                Harness::new("Diff in a.qmd:4:\n-x\n+y\n---\nDiff in a.qmd:9:\n-x\n+y", 1);
            let config = Config::default();
            let ctx = ToolCtx::new(&harness.provider, &config, false);

            let outcome = harness
                .adapter()
                .format(&paths(&["a.qmd"]), true, &ctx)
                .expect("check exit 1 with diffs is success");

            assert_eq!(outcome.changed, paths(&["a.qmd"]));
            let args = harness.recorded_args();
            assert!(args.contains("--check"), "{args}");
        }

        #[test]
        fn a_project_config_suppresses_the_generated_default() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness = Harness::new("", 0);
            fs::write(harness.dir.path().join("panache.toml"), "").expect("project config");
            let config = Config::default();
            let ctx = ToolCtx::new(&harness.provider, &config, false);

            harness
                .adapter()
                .format(&paths(&["a.qmd"]), false, &ctx)
                .expect("format succeeds");

            let args = harness.recorded_args();
            assert!(
                !args.contains("--config"),
                "panache discovers the project config itself: {args}"
            );
        }

        #[test]
        fn discovery_follows_the_files_not_the_working_directory() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness = Harness::new("", 0);
            // A project rooted inside the harness dir: its own config plus
            // a `.git` boundary so the walk never escapes into the real
            // filesystem. The process working directory is elsewhere.
            let project = harness.dir.path().join("proj");
            fs::create_dir_all(project.join(".git")).expect("git dir");
            fs::write(project.join("panache.toml"), "").expect("project config");
            let config = Config::default();
            let ctx = ToolCtx::new(&harness.provider, &config, false);
            let adapter =
                PanacheAdapter::with_user_config(&harness.dir.path().join("no-user-config.toml"));

            adapter
                .format(&[project.join("a.qmd")], false, &ctx)
                .expect("format succeeds");

            let args = harness.recorded_args();
            assert!(
                !args.contains("--config"),
                "panache starts discovery from the file's own directory, \
                 so the project config must suppress the generated default: {args}"
            );
        }

        #[test]
        fn a_configless_project_still_gets_the_generated_default() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness = Harness::new("", 0);
            let project = harness.dir.path().join("proj");
            fs::create_dir_all(project.join(".git")).expect("git dir");
            let config = Config::default();
            let ctx = ToolCtx::new(&harness.provider, &config, false);
            let adapter =
                PanacheAdapter::with_user_config(&harness.dir.path().join("no-user-config.toml"));

            adapter
                .format(&[project.join("a.qmd")], false, &ctx)
                .expect("format succeeds");

            let args = harness.recorded_args();
            assert!(args.contains("--config"), "{args}");
        }

        #[test]
        fn files_panache_skips_as_missing_are_not_counted_as_processed() {
            let _env = ENV_LOCK.lock().unwrap();
            let dir = tempfile::tempdir().expect("tempdir");
            let panache = dir.path().join("panache");
            // Recorded panache 2.60.0 behavior (format-missing-path.txt):
            // a missing input gets a stderr warning, the rest of the batch
            // is still processed, and the exit code stays 0.
            fs::write(
                &panache,
                "#!/bin/sh\n\
                 echo 'Warning: Path not found: gone.qmd' >&2\n\
                 echo 'Formatted a.qmd'\n\
                 echo '1 file reformatted, 0 files left unchanged'\n\
                 exit 0\n",
            )
            .expect("write fake panache");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&panache, fs::Permissions::from_mode(0o755)).expect("chmod");
            let mut provider = FakeToolPaths::default();
            provider.insert("panache", &panache);
            provider.insert("air", &dir.path().join("air"));
            provider.insert("ruff", &dir.path().join("ruff"));
            let config = Config::default();
            let ctx = ToolCtx::new(&provider, &config, false);
            let adapter = PanacheAdapter::with_config_search(
                dir.path(),
                &dir.path().join("no-user-config.toml"),
            );

            let outcome = adapter
                .format(&paths(&["a.qmd", "gone.qmd"]), false, &ctx)
                .expect("a skipped path is a warning, not a failure");

            // The skipped file must not inflate the processed count that
            // feeds the "<N> files formatted" summary.
            assert_eq!(outcome.processed, 1);
            assert_eq!(outcome.changed, paths(&["a.qmd"]));
        }

        #[test]
        fn escape_hatch_args_are_appended_before_the_files() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness = Harness::new("", 0);
            let mut config = Config::default();
            config
                .tools
                .args
                .insert("panache".to_string(), vec!["--no-cache".to_string()]);
            let ctx = ToolCtx::new(&harness.provider, &config, false);

            harness
                .adapter()
                .lint(&paths(&["a.qmd"]), false, &ctx)
                .expect("lint succeeds");

            let args = harness.recorded_args();
            assert!(
                args.contains("--no-cache a.qmd"),
                "escape hatch just before files: {args}"
            );
        }

        #[test]
        fn lint_passes_fix_and_parses_short_diagnostics() {
            let _env = ENV_LOCK.lock().unwrap();
            let harness = Harness::new(
                "a.qmd:7:4: warning[missing-chunk-labels]: no label\n\nFound 1 issue(s)",
                0,
            );
            let config = Config::default();
            let ctx = ToolCtx::new(&harness.provider, &config, false);

            let diagnostics = harness
                .adapter()
                .lint(&paths(&["a.qmd"]), true, &ctx)
                .expect("lint succeeds");

            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code.as_deref(), Some("missing-chunk-labels"));
            let args = harness.recorded_args();
            assert!(
                args.starts_with("lint --message-format short --no-color --fix"),
                "{args}"
            );
        }

        #[test]
        fn a_failing_run_reports_the_stderr_error_with_a_hint() {
            let _env = ENV_LOCK.lock().unwrap();
            let dir = tempfile::tempdir().expect("tempdir");
            let panache = dir.path().join("panache");
            fs::write(
                &panache,
                "#!/bin/sh\necho 'Error: No supported files found' >&2\nexit 1\n",
            )
            .expect("write fake panache");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&panache, fs::Permissions::from_mode(0o755)).expect("chmod");
            let mut provider = FakeToolPaths::default();
            provider.insert("panache", &panache);
            provider.insert("air", &dir.path().join("air"));
            provider.insert("ruff", &dir.path().join("ruff"));
            let config = Config::default();
            let ctx = ToolCtx::new(&provider, &config, false);
            let adapter = PanacheAdapter::with_config_search(
                dir.path(),
                &dir.path().join("no-user-config.toml"),
            );

            let err = adapter
                .format(&paths(&["a.qmd"]), false, &ctx)
                .expect_err("exit 1 without diffs in write mode is a failure");
            let rendered = crate::term::render_error(&err, false);
            assert!(
                rendered.contains("No supported files found"),
                "names the cause: {rendered}"
            );
            assert!(rendered.contains("hint:"), "says what to do: {rendered}");
        }
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;
    use crate::adapters::ToolPaths;
    use crate::config::Config;
    use crate::tools::{Downloader, InstallContext, Platform, ToolCache, ToolSpec};

    /// Resolves tools by downloading the real pinned releases into a
    /// test-owned cache directory.
    struct DownloadedTools {
        cache_root: PathBuf,
    }

    impl ToolPaths for DownloadedTools {
        fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
            let spec = ToolSpec::builtin(tool).expect("built-in tool");
            let platform = Platform::current().expect("supported platform");
            let ctx = InstallContext {
                label: tool,
                command: "togi format",
                verbose: true,
            };
            Downloader::new(ToolCache::at(&self.cache_root), platform).ensure_installed(
                &spec,
                spec.default_version,
                &ctx,
            )
        }
    }

    /// Downloads the real panache/air/ruff releases and formats a .qmd
    /// whose R chunk is misformatted.
    /// Run with: `cargo test --features online-tests -- --ignored`
    #[test]
    #[ignore = "downloads real releases from GitHub"]
    fn formats_a_real_qmd_with_a_misformatted_r_chunk() {
        let cache = tempfile::tempdir().expect("cache dir");
        let project = tempfile::tempdir().expect("project dir");
        let document = project.path().join("analysis.qmd");
        std::fs::write(
            &document,
            "---\ntitle: \"Demo\"\n---\n\n# Demo\n\n```{r}\nx<-c(1,2,3)\n```\n",
        )
        .expect("write qmd");

        let provider = DownloadedTools {
            cache_root: cache.path().to_path_buf(),
        };
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        // Pin discovery to the empty project dir so the generated default
        // config (which enables air for R chunks) is used regardless of
        // the machine this runs on.
        let adapter = PanacheAdapter::with_config_search(
            project.path(),
            &project.path().join("no-user-config.toml"),
        );

        let outcome = adapter
            .format(std::slice::from_ref(&document), false, &ctx)
            .expect("real panache formats the document");

        assert_eq!(outcome.processed, 1);
        assert_eq!(outcome.changed, vec![document.clone()]);
        let formatted = std::fs::read_to_string(&document).expect("read back");
        assert!(
            formatted.contains("x <- c(1, 2, 3)"),
            "air reformatted the embedded R chunk:\n{formatted}"
        );
    }
}
