//! Gitignore-aware file walker.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};

/// Directories togi always skips: package-manager libraries and the
/// generated files inside them that no formatter or linter should touch
/// (`renv/` and `rv/` keep files like `renv/activate.R` tracked in git, so
/// `.gitignore` alone does not cover them). Additive to `.gitignore` and to
/// the user's configured `exclude`, and rooted at the project root like any
/// anchored gitignore pattern. uv's `.venv/` needs no entry — it is hidden,
/// and hidden paths are already skipped.
pub const DEFAULT_EXCLUDES: &[&str] = &["renv/**", "rv/**"];

/// Errors from file discovery. Messages tell the user what to do next;
/// rendering is the caller's job (via `term`), never this module's.
#[derive(Debug, thiserror::Error)]
pub enum FsxError {
    /// A target path handed to [`walk`] does not exist.
    #[error(
        "path `{path}` does not exist; check the path for typos or run the \
         command from the project root"
    )]
    MissingPath { path: PathBuf },

    /// An exclude glob (config `exclude` or a CLI flag) failed to parse.
    #[error(
        "invalid exclude glob `{pattern}`; fix the pattern in your `exclude` \
         configuration (gitignore-style globs, e.g. `data/**` or `*.log`)"
    )]
    InvalidExclude {
        pattern: String,
        source: ignore::Error,
    },
}

/// What [`walk`] found: the surviving files plus non-fatal problems hit on
/// the way.
///
/// `fsx` returns data only; callers (the format/lint commands) render
/// `warnings` through `term` so a permission-denied subtree never silently
/// shrinks the target set.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    /// Files that survived ignore filtering, sorted and deduplicated.
    pub files: Vec<PathBuf>,
    /// One human-readable message per entry that could not be read (e.g. an
    /// unreadable subdirectory whose contents were skipped). Sorted and
    /// deduplicated; each says what to do next.
    pub warnings: Vec<String>,
}

/// Walk `paths` and return every file that survives ignore filtering, plus
/// warnings for anything that could not be read.
///
/// Filtering respects `.gitignore` files (including nested ones, with normal
/// gitignore precedence) plus the additive gitignore-style `excludes` globs —
/// these come from the `[format].exclude` / `[lint].exclude` config keys,
/// passed in as plain parameters here. Per-machine git
/// configuration (the global gitignore, `.git/info/exclude`) is deliberately
/// *not* consulted: discovery is scoped to `.gitignore` + config
/// excludes, and results must not vary across machines. Hidden files are
/// skipped, matching the underlying tools' conventions. Explicit file targets
/// are returned as-is, bypassing both `.gitignore` and `excludes` (ruff's
/// default behavior): naming a file on the command line is an intentional
/// request to process that exact file.
///
/// Rendered markdown is skipped as well: a walked `X.md` is dropped when a
/// file named `X.qmd` or `X.Rmd` sits in the same directory, because that
/// markdown is the source's generated output and is overwritten on the next
/// render. Extensions are matched without regard to ASCII case; the stem
/// must match exactly. The source is looked up on disk, so one hidden by
/// `.gitignore` or `excludes` still counts. An explicit file target is
/// exempt from this too, even when its directory is walked in the same call.
///
/// Exclude globs are rooted at `exclude_root` when given — the project
/// root, so anchored patterns like `data/**` mean the same thing no matter
/// which subdirectory is targeted. Without it they are rooted at each
/// walked path (plain-parameter behavior for callers with no project
/// notion).
pub fn walk(
    paths: &[PathBuf],
    excludes: &[String],
    exclude_root: Option<&Path>,
) -> Result<WalkOutcome, FsxError> {
    let mut found = BTreeSet::new();
    let mut warnings = BTreeSet::new();
    // Stems of the Quarto sources in each directory that holds walked
    // markdown, so a directory is listed once however many `.md` files it has.
    let mut source_stems: HashMap<PathBuf, HashSet<OsString>> = HashMap::new();
    for path in paths {
        if !path.exists() {
            return Err(FsxError::MissingPath { path: path.clone() });
        }
        // Root the exclude globs at the project root when the caller has
        // one; otherwise where the walk starts, so anchored patterns
        // (`data/**`) behave like a .gitignore at the target root. For a
        // file target, that fallback root is its containing directory.
        let glob_root = exclude_root.unwrap_or_else(|| {
            if path.is_file() {
                path.parent().unwrap_or_else(|| Path::new("."))
            } else {
                path.as_path()
            }
        });
        let overrides = build_exclude_overrides(glob_root, excludes)?;

        let walker = WalkBuilder::new(path)
            // Respect .gitignore even outside a git checkout (fixture dirs,
            // fresh projects before `git init`).
            .require_git(false)
            // Discovery is scoped to `.gitignore` + config exclude
            // globs. Per-machine git configuration — the user's global
            // ignore (`core.excludesFile`) and the clone-local
            // `.git/info/exclude` — must not change which files togi
            // formats/lints, or results would differ across machines and CI.
            .git_global(false)
            .git_exclude(false)
            .overrides(overrides)
            .build();
        for entry in walker {
            match entry {
                Ok(entry) => {
                    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        continue;
                    }
                    // Depth 0 is the target itself: a file named explicitly
                    // is always processed.
                    if entry.depth() > 0 && is_rendered_markdown(entry.path(), &mut source_stems) {
                        continue;
                    }
                    found.insert(entry.into_path());
                }
                // Per-entry errors (e.g. unreadable subdirectories) are not
                // fatal — the roots were validated above — but they must not
                // pass silently either: anything beneath them is skipped.
                Err(err) => {
                    warnings.insert(format!(
                        "skipped {err}; files under this path were not \
                         included — fix its permissions or add it to your \
                         exclude globs"
                    ));
                }
            }
        }
    }
    Ok(WalkOutcome {
        files: found.into_iter().collect(),
        warnings: warnings.into_iter().collect(),
    })
}

/// Compile exclude globs into an [`Override`] set rooted at `root`.
///
/// `Override` globs are whitelists by default; negating each pattern turns
/// them into ignores, so configured excludes add to (never replace) what
/// .gitignore already skips.
fn build_exclude_overrides(root: &Path, excludes: &[String]) -> Result<Override, FsxError> {
    let mut builder = OverrideBuilder::new(root);
    for pattern in excludes {
        builder
            .add(&format!("!{pattern}"))
            .map_err(|source| FsxError::InvalidExclude {
                pattern: pattern.clone(),
                source,
            })?;
    }
    builder.build().map_err(|source| FsxError::InvalidExclude {
        pattern: excludes.join(", "),
        source,
    })
}

/// Whether `path` is markdown rendered from a Quarto source beside it: a
/// `.md` file whose directory also holds a `.qmd` or `.Rmd` file with the
/// same stem. Extensions are compared without regard to ASCII case.
///
/// `source_stems` caches, per directory, the stems of the sources found
/// there. The directory is listed rather than probed for fixed spellings so
/// that any casing of the source extension is found on a case-sensitive
/// filesystem. A directory or entry that cannot be read contributes no
/// stems, which keeps the markdown file.
fn is_rendered_markdown(
    path: &Path,
    source_stems: &mut HashMap<PathBuf, HashSet<OsString>>,
) -> bool {
    if !has_extension(path, &["md"]) {
        return false;
    }
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
        return false;
    };
    if let Some(stems) = source_stems.get(dir) {
        return stems.contains(stem);
    }
    let stems = quarto_source_stems(listing_dir(dir));
    let rendered = stems.contains(stem);
    source_stems.insert(dir.to_path_buf(), stems);
    rendered
}

/// The directory to list for a file whose parent is `parent`. A bare file
/// name such as `doc.md` has an empty parent, which names no directory on
/// disk; it stands for the current directory.
fn listing_dir(parent: &Path) -> &Path {
    if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    }
}

/// Stems of the `.qmd` and `.Rmd` files directly inside `dir`; empty when
/// the directory cannot be listed.
///
/// Each entry is judged by its file name and the file type the listing
/// reports, so most entries cost no further filesystem call. Only a symlink
/// named like a source is resolved, to count a link to a file and reject a
/// dangling link or a link to a directory. An entry whose type cannot be
/// read contributes no stem.
fn quarto_source_stems(dir: &Path) -> HashSet<OsString> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = Path::new(&name);
            if !has_extension(name, &["qmd", "rmd"]) {
                return None;
            }
            let file_type = entry.file_type().ok()?;
            let is_source =
                file_type.is_file() || (file_type.is_symlink() && entry.path().is_file());
            if is_source {
                name.file_stem().map(OsString::from)
            } else {
                None
            }
        })
        .collect()
}

/// Whether `path`'s extension is one of `extensions` (lowercase, no leading
/// dot), compared without regard to ASCII case.
fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| extensions.iter().any(|want| ext.eq_ignore_ascii_case(want)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Create `rel` (and any parent directories) under `root`.
    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Root-relative, forward-slashed names for platform-stable assertions.
    fn rel_names(files: &[PathBuf], root: &Path) -> Vec<String> {
        files
            .iter()
            .map(|f| {
                f.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walk_respects_root_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\ndata/\n");
        write(root, "analysis.R", "");
        write(root, "run.log", "");
        write(root, "data/raw.csv", "");
        write(root, "query.sql", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["analysis.R", "query.sql"]);
    }

    #[test]
    fn walk_respects_nested_gitignore_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Root ignores all logs; the nested .gitignore ignores a file of its
        // own and un-ignores one specific log, exercising real gitignore
        // precedence between levels.
        write(root, ".gitignore", "*.log\n");
        write(root, "analysis.R", "");
        write(root, "run.log", "");
        write(root, "sub/.gitignore", "local.R\n!keep.log\n");
        write(root, "sub/local.R", "");
        write(root, "sub/model.py", "");
        write(root, "sub/keep.log", "");
        write(root, "sub/other.log", "");
        write(root, "sub/deeper/.gitignore", "*.py\n");
        write(root, "sub/deeper/scratch.py", "");
        write(root, "sub/deeper/notes.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(
            rel_names(&files, root),
            vec![
                "analysis.R",
                "sub/deeper/notes.md",
                "sub/keep.log",
                "sub/model.py",
            ]
        );
    }

    #[test]
    fn walk_applies_exclude_globs_additively_to_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\n");
        write(root, "analysis.R", "");
        write(root, "run.log", "");
        write(root, "query.sql", "");
        write(root, "data/raw.csv", "");
        write(root, "src/model.py", "");

        let excludes = ["*.sql".to_string(), "data/**".to_string()];
        let files = walk(&[root.to_path_buf()], &excludes, None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["analysis.R", "src/model.py"]);
    }

    /// Config excludes are written against the *project root*, so when a
    /// subdirectory is targeted explicitly the anchored globs must still
    /// mean the same thing. Rooting them per target would silently turn
    /// `data/**` into "data inside the target".
    #[test]
    fn walk_anchors_exclude_globs_at_the_given_root_across_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "data/raw.sql", "");
        write(root, "src/query.sql", "");
        write(root, "src/data/nested.sql", "");

        let excludes = ["data/**".to_string()];
        let files = walk(
            &[root.join("data"), root.join("src")],
            &excludes,
            Some(root),
        )
        .unwrap()
        .files;

        // `data/**` is anchored at the root: it drops the root-level data
        // directory but not `src/data`, exactly like a root .gitignore.
        assert_eq!(
            rel_names(&files, root),
            vec!["src/data/nested.sql", "src/query.sql"]
        );
    }

    #[test]
    fn walk_accepts_explicit_file_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "model.py", "");

        let files = walk(&[root.join("analysis.R")], &[], None).unwrap().files;

        assert_eq!(files, vec![root.join("analysis.R")]);
    }

    /// Pins the documented policy that explicit file targets bypass both
    /// `.gitignore` and exclude globs (ruff's default): "format this exact
    /// file" is an intentional request. If that policy ever changes, this
    /// test should change with it deliberately, not by accident.
    #[test]
    fn walk_returns_explicit_file_targets_even_if_ignored_or_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\n");
        write(root, "run.log", "");
        write(root, "query.sql", "");

        let outcome = walk(
            &[root.join("run.log"), root.join("query.sql")],
            &["*.sql".to_string()],
            None,
        )
        .unwrap();

        assert_eq!(
            rel_names(&outcome.files, root),
            vec!["query.sql", "run.log"]
        );
    }

    #[test]
    fn walk_dedupes_overlapping_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "sub/model.py", "");

        let files = walk(&[root.to_path_buf(), root.join("sub")], &[], None)
            .unwrap()
            .files;

        assert_eq!(files, vec![root.join("sub").join("model.py")]);
    }

    #[test]
    fn walk_skips_hidden_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".Rhistory", "");
        write(root, ".Rproj.user/settings", "");
        write(root, "analysis.R", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["analysis.R"]);
    }

    /// Markdown rendered from a Quarto document beside it is generated
    /// output: the source is walked, the output is not.
    #[test]
    fn walk_skips_markdown_rendered_from_a_sibling_qmd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd", "");
        write(root, "doc.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.qmd"]);
    }

    #[test]
    fn walk_skips_markdown_rendered_from_a_sibling_rmd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.Rmd", "");
        write(root, "doc.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.Rmd"]);
    }

    /// The source extension is matched without regard to ASCII case. Each
    /// spelling gets its own directory so no two names differ only by case,
    /// which keeps the fixture valid on case-insensitive filesystems.
    #[test]
    fn walk_matches_the_rendered_markdown_source_extension_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (dir, source) in [
            ("a", "doc.QMD"),
            ("b", "doc.Qmd"),
            ("c", "doc.rmd"),
            ("d", "doc.RMD"),
        ] {
            write(root, &format!("{dir}/{source}"), "");
            write(root, &format!("{dir}/doc.md"), "");
        }

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(
            rel_names(&files, root),
            vec!["a/doc.QMD", "b/doc.Qmd", "c/doc.rmd", "d/doc.RMD"]
        );
    }

    /// A bare file name has an empty parent, which is listed as the current
    /// directory; any other parent is listed as given.
    #[test]
    fn listing_dir_maps_an_empty_parent_to_the_current_directory() {
        let bare = Path::new("doc.md").parent().unwrap();
        assert_eq!(listing_dir(bare), Path::new("."));

        let nested = Path::new("docs/doc.md").parent().unwrap();
        assert_eq!(listing_dir(nested), Path::new("docs"));
    }

    /// The rendered file's own extension is matched without regard to ASCII
    /// case as well.
    #[test]
    fn walk_skips_rendered_markdown_with_an_uppercase_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd", "");
        write(root, "doc.MD", "");
        write(root, "notes.MD", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.qmd", "notes.MD"]);
    }

    /// Only a file counts as a source: a directory that happens to be named
    /// like one does not mark the markdown beside it as rendered.
    #[test]
    fn walk_keeps_markdown_beside_a_directory_named_like_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd/inner.R", "");
        write(root, "doc.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.md", "doc.qmd/inner.R"]);
    }

    /// A source reached through a symlink is still a source.
    #[cfg(unix)]
    #[test]
    fn walk_skips_markdown_rendered_from_a_symlinked_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "shared/original.qmd", "");
        write(root, "docs/doc.md", "");
        write(root, "docs/notes.md", "");
        std::os::unix::fs::symlink(root.join("shared/original.qmd"), root.join("docs/doc.qmd"))
            .unwrap();

        let files = walk(&[root.join("docs")], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["docs/notes.md"]);
    }

    /// A symlink that points nowhere is not a source, so the markdown
    /// beside it is kept.
    #[cfg(unix)]
    #[test]
    fn walk_keeps_markdown_beside_a_dangling_symlink_named_like_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.md", "");
        std::os::unix::fs::symlink(root.join("missing.qmd"), root.join("doc.qmd")).unwrap();

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.md"]);
    }

    /// A symlink to a directory is not a source either.
    #[cfg(unix)]
    #[test]
    fn walk_keeps_markdown_beside_a_symlink_to_a_directory_named_like_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "docs/doc.md", "");
        fs::create_dir_all(root.join("shared")).unwrap();
        std::os::unix::fs::symlink(root.join("shared"), root.join("docs/doc.qmd")).unwrap();

        let files = walk(&[root.join("docs")], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["docs/doc.md"]);
    }

    /// The stem is everything before the last dot, so a multi-dot source
    /// pairs only with markdown carrying the same multi-dot stem.
    #[test]
    fn walk_skips_markdown_rendered_from_a_source_with_a_dotted_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.final.qmd", "");
        write(root, "doc.final.md", "");
        write(root, "doc.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.final.qmd", "doc.md"]);
    }

    #[test]
    fn walk_keeps_markdown_without_a_sibling_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "notes.md", "");
        write(root, "doc.qmd", "");
        write(root, "analysis.R", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(
            rel_names(&files, root),
            vec!["analysis.R", "doc.qmd", "notes.md"]
        );
    }

    /// Only a source in the same directory marks markdown as rendered.
    #[test]
    fn walk_keeps_markdown_whose_source_is_in_another_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd", "");
        write(root, "out/doc.md", "");
        write(root, "src/doc.Rmd", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(
            rel_names(&files, root),
            vec!["doc.qmd", "out/doc.md", "src/doc.Rmd"]
        );
    }

    /// The sibling source is looked up on disk, not among the walked files:
    /// a source hidden by `.gitignore` still marks its output as rendered.
    #[test]
    fn walk_skips_markdown_rendered_from_a_gitignored_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.qmd\n");
        write(root, "doc.qmd", "");
        write(root, "doc.md", "");
        write(root, "notes.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["notes.md"]);
    }

    /// A source dropped by an exclude glob still marks its output as
    /// rendered, for the same reason as a gitignored one.
    #[test]
    fn walk_skips_markdown_rendered_from_an_excluded_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.Rmd", "");
        write(root, "doc.md", "");
        write(root, "notes.md", "");

        let excludes = ["*.Rmd".to_string()];
        let files = walk(&[root.to_path_buf()], &excludes, None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["notes.md"]);
    }

    /// Naming a rendered markdown file on the command line is an intentional
    /// request to process it, like any other explicit file target.
    #[test]
    fn walk_returns_an_explicit_markdown_target_beside_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd", "");
        write(root, "doc.md", "");

        let files = walk(&[root.join("doc.md")], &[], None).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["doc.md"]);
    }

    /// An explicit target stays in the result even when the same run also
    /// walks the directory that would otherwise skip it.
    #[test]
    fn walk_keeps_an_explicit_markdown_target_when_its_directory_is_also_walked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd", "");
        write(root, "doc.md", "");
        write(root, "other.qmd", "");
        write(root, "other.md", "");

        let files = walk(&[root.to_path_buf(), root.join("doc.md")], &[], None)
            .unwrap()
            .files;

        assert_eq!(
            rel_names(&files, root),
            vec!["doc.md", "doc.qmd", "other.qmd"]
        );
    }

    /// Only an exact stem match counts: markdown whose name merely starts
    /// with the source's stem or full name is not that source's output.
    #[test]
    fn walk_keeps_markdown_whose_stem_only_resembles_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "doc.qmd", "");
        write(root, "doc.qmd.md", "");
        write(root, "doc.final.md", "");
        write(root, "document.md", "");

        let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

        assert_eq!(
            rel_names(&files, root),
            vec!["doc.final.md", "doc.qmd", "doc.qmd.md", "document.md"]
        );
    }

    /// A permission-denied subtree must not silently shrink the target set:
    /// the files are skipped, but the caller gets a warning to render.
    #[cfg(unix)]
    #[test]
    fn walk_warns_on_unreadable_directories() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "locked/inner.R", "");
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = walk(&[root.to_path_buf()], &[], None);

        // Restore before asserting so the tempdir cleans up even on failure.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = outcome.unwrap();
        assert_eq!(rel_names(&outcome.files, root), vec!["analysis.R"]);
        assert_eq!(
            outcome.warnings.len(),
            1,
            "warnings: {:?}",
            outcome.warnings
        );
        let warning = &outcome.warnings[0];
        assert!(warning.contains("locked"), "warning: {warning}");
        // Project rule: every user-facing message says what to do next.
        assert!(warning.contains("exclude"), "warning: {warning}");
    }

    /// Regression test: the user's *global* gitignore (`~/.config/git/ignore`
    /// or `core.excludesFile`) must not leak into discovery — only
    /// `.gitignore` + config exclude globs only, and honoring per-machine
    /// global excludes would make format/lint targets differ across machines.
    ///
    /// Env vars cannot be mutated safely in a threaded test process, so the
    /// parent branch re-runs just this test in a child process whose
    /// HOME/XDG_CONFIG_HOME point at a hermetic global ignore dropping `*.R`.
    #[test]
    fn walk_ignores_users_global_gitignore() {
        if std::env::var_os("TOGI_TEST_GLOBAL_IGNORE_CHILD").is_some() {
            // Child: a hostile global ignore is in place; R files must survive.
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            write(root, "analysis.R", "");
            write(root, "query.sql", "");

            let files = walk(&[root.to_path_buf()], &[], None).unwrap().files;

            assert_eq!(rel_names(&files, root), vec!["analysis.R", "query.sql"]);
            return;
        }

        let fake_home = tempfile::tempdir().unwrap();
        write(fake_home.path(), "git/ignore", "*.R\n");
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "fsx::walk::tests::walk_ignores_users_global_gitignore",
            ])
            .env("TOGI_TEST_GLOBAL_IGNORE_CHILD", "1")
            .env("HOME", fake_home.path())
            .env("USERPROFILE", fake_home.path())
            .env("XDG_CONFIG_HOME", fake_home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "walk honored the user's global gitignore:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn walk_returns_empty_for_no_paths() {
        let outcome = walk(&[], &[], None).unwrap();
        assert!(outcome.files.is_empty());
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn walk_errors_on_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");

        let err = walk(std::slice::from_ref(&missing), &[], None).unwrap_err();

        assert!(matches!(err, FsxError::MissingPath { .. }));
        let message = err.to_string();
        assert!(message.contains("no-such-dir"), "message: {message}");
    }

    #[test]
    fn walk_errors_on_invalid_exclude_glob() {
        let tmp = tempfile::tempdir().unwrap();

        let err = walk(&[tmp.path().to_path_buf()], &["a[".to_string()], None).unwrap_err();

        assert!(matches!(err, FsxError::InvalidExclude { .. }));
        let message = err.to_string();
        assert!(message.contains("a["), "message: {message}");
    }
}
