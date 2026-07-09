//! Shared plumbing for `togi format` and `togi lint`: project-root
//! resolution, file discovery, language filtering, and diagnostic
//! rendering. Everything here returns data; the commands do the printing
//! through `term`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use togi_core::adapters::Diagnostic;
use togi_core::config::FileSelection;
use togi_core::fsx::{self, ExtensionRegistry, FsxError, Language, group_by_language};
use togi_core::term::HintExt;

/// The files one run hands to the adapters, batched per language bucket.
#[derive(Debug)]
pub struct Discovered {
    pub groups: BTreeMap<Language, Vec<PathBuf>>,
    /// Total files across `groups`.
    pub file_count: usize,
    /// Human-readable notes (unreadable subtrees, unknown language names in
    /// config) for the command to print via `term::warn`.
    pub warnings: Vec<String>,
}

/// Where "the whole project" starts when no paths are given: the directory
/// of the discovered `togi.toml` when there is one, else the working
/// directory. An explicit `--config <path>` names a config file, not a
/// project, so it never relocates the root.
pub fn project_root(cwd: &Path, explicit_config: bool, project_config: Option<&Path>) -> PathBuf {
    if explicit_config {
        return cwd.to_path_buf();
    }
    project_config
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Discover the files a run covers: walk `paths` (the whole project from
/// `root` when empty), respecting `.gitignore` plus togi's built-in
/// package-manager excludes ([`fsx::DEFAULT_EXCLUDES`]) and the `exclude`
/// globs from `selection` (all anchored at `root`), then keep only the
/// languages `selection` enables. Discovered paths come back relative to
/// `cwd` when they sit beneath it, so tool output and summaries stay
/// readable.
///
/// `section` names the config table (`format` / `lint`) in warnings about
/// unknown language names.
pub fn discover(
    cwd: &Path,
    paths: &[PathBuf],
    selection: &FileSelection,
    root: &Path,
    section: &str,
) -> anyhow::Result<Discovered> {
    let targets: Vec<PathBuf> = if paths.is_empty() {
        vec![root.to_path_buf()]
    } else {
        paths.iter().map(|path| absolute(cwd, path)).collect()
    };
    // togi's built-in excludes (package-manager directories) always apply;
    // the config `exclude` layers additively on top of them.
    let mut excludes: Vec<String> = fsx::DEFAULT_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .collect();
    excludes.extend(selection.exclude.iter().cloned());
    let outcome = fsx::walk(&targets, &excludes, Some(root)).map_err(walk_error)?;
    let mut warnings = outcome.warnings;
    let files: Vec<PathBuf> = outcome
        .files
        .into_iter()
        .map(|file| relative_to(file, cwd))
        .collect();

    let allowed = allowed_languages(&selection.languages, section, &mut warnings);
    let mut groups = group_by_language(&files, &ExtensionRegistry::with_defaults());
    groups.retain(|language, _| allowed.contains(language));
    let file_count = groups.values().map(Vec::len).sum();
    Ok(Discovered {
        groups,
        file_count,
        warnings,
    })
}

/// Map discovery failures onto the CLI's error taxonomy: a path the user
/// typed that does not exist is a usage error (exit 2, like any bad
/// argument); a broken exclude glob is a config problem (exit 1).
fn walk_error(err: FsxError) -> anyhow::Error {
    match err {
        FsxError::MissingPath { .. } => super::usage_error(
            err.to_string(),
            "pass existing files or directories, or run with no paths to \
             cover the whole project",
        ),
        FsxError::InvalidExclude { .. } => Err::<(), _>(anyhow::Error::new(err))
            .hint("fix the `exclude` globs under `[format]`/`[lint]` in togi.toml and rerun")
            .expect_err("just built from Err"),
    }
}

/// The language buckets `names` enables, warning (not erroring) about
/// names togi does not know so a typo never silently disables everything
/// without a trace.
fn allowed_languages(
    names: &[String],
    section: &str,
    warnings: &mut Vec<String>,
) -> BTreeSet<Language> {
    let mut allowed = BTreeSet::new();
    for name in names {
        match Language::from_config_name(name) {
            Some(language) => {
                allowed.insert(language);
            }
            None => warnings.push(format!(
                "ignoring unknown language `{name}` in `[{section}] languages` \
                 (supported: r, python, quarto, sql, markdown)"
            )),
        }
    }
    allowed
}

/// `path` made absolute against `cwd` (user-typed targets may be either).
fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// `file` relative to `cwd` when it sits beneath it; unchanged otherwise.
fn relative_to(file: PathBuf, cwd: &Path) -> PathBuf {
    match file.strip_prefix(cwd) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => file,
    }
}

/// Rewrite every diagnostic's `path` to be relative to the project `root`,
/// so reporting (human and `--format json`) is uniform no matter which tool
/// produced it. See [`relativize_path`].
pub fn relativize_diagnostics(diagnostics: &mut [Diagnostic], cwd: &Path, root: &Path) {
    for diagnostic in diagnostics {
        diagnostic.path = relativize_path(&diagnostic.path, cwd, root);
    }
}

/// A tool-reported `path` rewritten relative to the project `root`.
///
/// Tools disagree on what they echo: air, panache, and sqlfluff repeat the
/// (cwd-relative) paths togi hands them, while ruff resolves them to
/// absolute paths. Normalizing here — absolutize against `cwd`, then strip
/// `root` — makes them all project-root-relative. A path that does not sit
/// under `root` (e.g. a symlinked build dir) is left exactly as the tool
/// reported it.
pub fn relativize_path(path: &Path, cwd: &Path, root: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    match absolute.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => path.to_path_buf(),
    }
}

/// One diagnostic as a ruff-style report line:
/// `path:line:col: CODE [*] message`, where `[*]` marks findings
/// `togi lint --fix` can fix. Position and code are omitted when the tool
/// did not report them.
///
/// With `use_color`, the `path:line:col` location is styled and the rule
/// code takes its color from the finding's severity, matching ruff; the
/// message stays plain. With `use_color` false the line is byte-for-byte
/// the plain form.
pub fn render_diagnostic(diagnostic: &Diagnostic, use_color: bool) -> String {
    use togi_core::term::{self, DIAGNOSTIC_LOCATION_STYLE};

    let mut location = diagnostic.path.display().to_string();
    if let Some(range) = &diagnostic.range {
        location.push_str(&format!(":{}:{}", range.start.line, range.start.col));
    }
    let mut line = term::paint(DIAGNOSTIC_LOCATION_STYLE, &location, use_color);
    line.push(':');
    if let Some(code) = &diagnostic.code {
        line.push(' ');
        line.push_str(&term::paint(
            severity_style(diagnostic.severity),
            code,
            use_color,
        ));
    }
    if diagnostic.fixable {
        line.push_str(" [*]");
    }
    line.push(' ');
    line.push_str(&diagnostic.message);
    line
}

/// The style a rule code is painted in, by severity: red for errors,
/// yellow for warnings, blue for informational findings.
fn severity_style(severity: togi_core::adapters::Severity) -> anstyle::Style {
    use togi_core::adapters::Severity;
    match severity {
        Severity::Error => togi_core::term::ERROR_STYLE,
        Severity::Warning => togi_core::term::WARN_STYLE,
        Severity::Info => togi_core::term::INFO_STYLE,
    }
}

/// Order diagnostics for reporting: by path, then position, then code —
/// stable regardless of which adapter produced what.
pub fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    fn key(d: &Diagnostic) -> (&Path, u32, u32, Option<&str>) {
        let (line, col) = d
            .range
            .map(|range| (range.start.line, range.start.col))
            .unwrap_or((0, 0));
        (d.path.as_path(), line, col, d.code.as_deref())
    }
    diagnostics.sort_by(|a, b| key(a).cmp(&key(b)));
}

/// `1 file` / `42 files` — shared pluralization for summaries.
pub fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use togi_core::adapters::{Position, Range, Severity};

    fn selection(languages: &[&str], exclude: &[&str]) -> FileSelection {
        FileSelection {
            languages: languages.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Forward-slashed strings for platform-stable assertions.
    fn names(files: &[PathBuf]) -> Vec<String> {
        files
            .iter()
            .map(|f| f.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    // ---- project_root ---------------------------------------------------

    #[test]
    fn project_root_is_the_config_files_directory() {
        let cwd = Path::new("/repo/sub");
        let config = Path::new("/repo/togi.toml");
        assert_eq!(project_root(cwd, false, Some(config)), Path::new("/repo"));
    }

    #[test]
    fn project_root_falls_back_to_cwd_without_a_config() {
        let cwd = Path::new("/somewhere");
        assert_eq!(project_root(cwd, false, None), cwd);
    }

    #[test]
    fn an_explicit_config_flag_never_relocates_the_root() {
        // `--config /tmp/other.toml` names a file to read, not a project.
        let cwd = Path::new("/repo/sub");
        let config = Path::new("/tmp/other.toml");
        assert_eq!(project_root(cwd, true, Some(config)), cwd);
    }

    // ---- discover -------------------------------------------------------

    #[test]
    fn discover_groups_project_files_by_language_relative_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "model.py", "");
        write(root, "report.qmd", "");
        write(root, "notes.md", "");
        write(root, "query.sql", "");
        write(root, "data.csv", "");

        let discovered = discover(
            root,
            &[],
            &selection(&["r", "python", "quarto", "sql", "markdown"], &[]),
            root,
            "format",
        )
        .expect("discovery succeeds");

        assert_eq!(discovered.file_count, 5, "csv is not a target");
        assert!(discovered.warnings.is_empty(), "{:?}", discovered.warnings);
        // Paths under the cwd come back relative for readable output.
        assert_eq!(names(&discovered.groups[&Language::R]), vec!["analysis.R"]);
        assert_eq!(names(&discovered.groups[&Language::Sql]), vec!["query.sql"]);
    }

    #[test]
    fn discover_skips_renv_and_rv_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "renv/activate.R", "");
        write(root, "renv/settings.json", "");
        write(root, "rv/library/pkg.R", "");
        write(root, "analysis.R", "");

        // Empty config `exclude`: the built-in package-manager excludes still
        // keep togi out of renv/ and rv/.
        let discovered = discover(root, &[], &selection(&["r"], &[]), root, "format")
            .expect("discovery succeeds");

        assert_eq!(discovered.file_count, 1);
        assert_eq!(names(&discovered.groups[&Language::R]), vec!["analysis.R"]);
    }

    #[test]
    fn discover_adds_config_excludes_on_top_of_the_built_in_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "renv/activate.R", "");
        write(root, "data/scratch.R", "");
        write(root, "analysis.R", "");

        // A user `exclude` is additive, not a replacement: `data/**` is
        // skipped *and* the built-in `renv/**` still applies.
        let discovered = discover(root, &[], &selection(&["r"], &["data/**"]), root, "format")
            .expect("discovery succeeds");

        assert_eq!(discovered.file_count, 1);
        assert_eq!(names(&discovered.groups[&Language::R]), vec!["analysis.R"]);
    }

    #[test]
    fn discover_filters_out_languages_the_config_disables() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "model.py", "");
        write(root, "notes.md", "");

        // The lint default: markdown is not in the list.
        let discovered = discover(
            root,
            &[],
            &selection(&["r", "python", "quarto", "sql"], &[]),
            root,
            "lint",
        )
        .expect("discovery succeeds");

        assert_eq!(discovered.file_count, 2);
        assert!(!discovered.groups.contains_key(&Language::Markdown));
    }

    #[test]
    fn discover_warns_about_unknown_language_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");

        let discovered = discover(root, &[], &selection(&["r", "julia"], &[]), root, "format")
            .expect("unknown names warn, not error");

        assert_eq!(discovered.file_count, 1);
        assert_eq!(discovered.warnings.len(), 1);
        let warning = &discovered.warnings[0];
        assert!(warning.contains("julia"), "{warning}");
        assert!(warning.contains("[format] languages"), "{warning}");
        // Says what is supported, so the user can fix the typo.
        assert!(warning.contains("python"), "{warning}");
    }

    #[test]
    fn discover_applies_config_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "vendored/junk.R", "");

        let discovered = discover(
            root,
            &[],
            &selection(&["r"], &["vendored/**"]),
            root,
            "format",
        )
        .expect("discovery succeeds");

        assert_eq!(names(&discovered.groups[&Language::R]), vec!["analysis.R"]);
    }

    #[test]
    fn discover_limits_the_run_to_explicit_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "sub/model.py", "");
        write(root, "other.py", "");

        let discovered = discover(
            root,
            &[PathBuf::from("sub"), PathBuf::from("analysis.R")],
            &selection(&["r", "python"], &[]),
            root,
            "format",
        )
        .expect("discovery succeeds");

        assert_eq!(discovered.file_count, 2);
        assert_eq!(
            names(&discovered.groups[&Language::Python]),
            vec!["sub/model.py"]
        );
    }

    #[test]
    fn discover_reports_a_missing_path_as_a_usage_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let err = discover(
            root,
            &[PathBuf::from("no-such-dir")],
            &selection(&["r"], &[]),
            root,
            "format",
        )
        .expect_err("missing paths must error");

        // Typed so `main` exits 2: a bad path argument is a usage error.
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("usage error type");
        assert!(err.to_string().contains("no-such-dir"), "{err}");
        assert!(!usage.hint().is_empty());
    }

    #[test]
    fn discover_reports_invalid_exclude_globs_with_a_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");

        let err = discover(root, &[], &selection(&["r"], &["a["]), root, "format")
            .expect_err("broken globs must error");

        assert!(
            err.downcast_ref::<crate::cli::UsageError>().is_none(),
            "config problems are runtime failures (exit 1), not usage errors"
        );
        let rendered = togi_core::term::render_error(&err, false);
        assert!(rendered.contains("a["), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    // ---- rendering ------------------------------------------------------

    #[test]
    fn render_diagnostic_matches_the_ruff_style_line() {
        let diagnostic = Diagnostic {
            path: PathBuf::from("violations.py"),
            range: Some(Range {
                start: Position { line: 3, col: 8 },
                end: None,
            }),
            code: Some("F401".to_string()),
            severity: Severity::Warning,
            message: "`os` imported but unused".to_string(),
            fixable: true,
        };
        assert_eq!(
            render_diagnostic(&diagnostic, false),
            "violations.py:3:8: F401 [*] `os` imported but unused"
        );
    }

    #[test]
    fn render_diagnostic_omits_missing_position_and_code() {
        let diagnostic = Diagnostic {
            path: PathBuf::from("messy.R"),
            range: None,
            code: None,
            severity: Severity::Warning,
            message: "file is not formatted with air".to_string(),
            fixable: false,
        };
        assert_eq!(
            render_diagnostic(&diagnostic, false),
            "messy.R: file is not formatted with air"
        );
    }

    #[test]
    fn render_diagnostic_styles_location_and_code_on_a_tty() {
        const ESC: &str = "\x1b[";
        let diagnostic = Diagnostic {
            path: PathBuf::from("violations.py"),
            range: Some(Range {
                start: Position { line: 3, col: 8 },
                end: None,
            }),
            code: Some("F401".to_string()),
            severity: Severity::Warning,
            message: "`os` imported but unused".to_string(),
            fixable: true,
        };
        let styled = render_diagnostic(&diagnostic, true);
        // The location and code are wrapped in ANSI styling...
        assert!(styled.contains(ESC), "styled output carries ANSI: {styled}");
        // ...but the human-readable text is all still there, in order.
        assert!(styled.contains("violations.py:3:8"), "{styled}");
        assert!(styled.contains("F401"), "{styled}");
        assert!(styled.contains("`os` imported but unused"), "{styled}");
        assert!(styled.contains("[*]"), "{styled}");
        // The plain form is the exact same text with no escape codes.
        let plain = render_diagnostic(&diagnostic, false);
        assert!(!plain.contains(ESC), "{plain}");
    }

    #[test]
    fn diagnostics_sort_by_path_then_position() {
        let mut diagnostics = vec![
            Diagnostic {
                path: PathBuf::from("b.py"),
                range: Some(Range {
                    start: Position { line: 9, col: 1 },
                    end: None,
                }),
                code: Some("F821".to_string()),
                severity: Severity::Warning,
                message: "later".to_string(),
                fixable: false,
            },
            Diagnostic {
                path: PathBuf::from("b.py"),
                range: Some(Range {
                    start: Position { line: 2, col: 5 },
                    end: None,
                }),
                code: Some("F401".to_string()),
                severity: Severity::Warning,
                message: "earlier".to_string(),
                fixable: true,
            },
            Diagnostic {
                path: PathBuf::from("a.sql"),
                range: None,
                code: None,
                severity: Severity::Error,
                message: "file-level".to_string(),
                fixable: false,
            },
        ];
        sort_diagnostics(&mut diagnostics);
        let order: Vec<String> = diagnostics.iter().map(|d| d.message.clone()).collect();
        assert_eq!(order, vec!["file-level", "earlier", "later"]);
    }

    #[test]
    fn count_pluralizes() {
        assert_eq!(count(1, "file"), "1 file");
        assert_eq!(count(42, "file"), "42 files");
        assert_eq!(count(0, "issue"), "0 issues");
    }

    // ---- path normalization ---------------------------------------------

    #[test]
    fn relativize_makes_absolute_tool_paths_project_relative() {
        // ruff reports absolute paths; they must come back relative to the
        // project root.
        let root = Path::new("/repo");
        let cwd = Path::new("/repo");
        assert_eq!(
            relativize_path(Path::new("/repo/analysis/model.py"), cwd, root),
            PathBuf::from("analysis/model.py")
        );
    }

    #[test]
    fn relativize_leaves_already_relative_paths_project_relative() {
        // air/panache/sqlfluff echo the cwd-relative paths we hand them;
        // when cwd is the root, those are already correct.
        let root = Path::new("/repo");
        let cwd = Path::new("/repo");
        assert_eq!(
            relativize_path(Path::new("query.sql"), cwd, root),
            PathBuf::from("query.sql")
        );
    }

    #[test]
    fn relativize_resolves_cwd_relative_paths_against_the_root() {
        // Running from a subdirectory: a path a tool reported relative to
        // that subdirectory still normalizes to root-relative.
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/analysis");
        assert_eq!(
            relativize_path(Path::new("model.py"), cwd, root),
            PathBuf::from("analysis/model.py")
        );
    }

    #[test]
    fn relativize_leaves_paths_outside_the_root_untouched() {
        // A path that does not sit under the project root is reported as
        // the tool gave it, rather than mangled.
        let root = Path::new("/repo");
        let cwd = Path::new("/repo");
        assert_eq!(
            relativize_path(Path::new("/elsewhere/x.py"), cwd, root),
            PathBuf::from("/elsewhere/x.py")
        );
    }

    #[test]
    fn relativize_diagnostics_rewrites_every_path() {
        let mut diagnostics = vec![
            Diagnostic {
                path: PathBuf::from("/repo/a.py"),
                range: None,
                code: Some("F401".to_string()),
                severity: Severity::Warning,
                message: "unused".to_string(),
                fixable: true,
            },
            Diagnostic {
                path: PathBuf::from("b.sql"),
                range: None,
                code: None,
                severity: Severity::Error,
                message: "bad".to_string(),
                fixable: false,
            },
        ];
        relativize_diagnostics(&mut diagnostics, Path::new("/repo"), Path::new("/repo"));
        assert_eq!(diagnostics[0].path, PathBuf::from("a.py"));
        assert_eq!(diagnostics[1].path, PathBuf::from("b.sql"));
    }
}
