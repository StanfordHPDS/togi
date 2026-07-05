//! `togi format` — format project files in place, or report what would
//! change with `--check`.
//!
//! Files are discovered from the given paths (default: the whole project),
//! batched per language, and handed to the adapters in parallel. Managed
//! tools are installed on demand on first use.

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;

use togi_core::adapters::{
    AdapterRegistry, FormatOutcome, InstalledToolPaths, ToolCtx, format_all,
};
use togi_core::config::{self, Layer};
use togi_core::term::{self, HintExt};

use super::fmt_lint;

#[derive(Debug, Args)]
pub struct FormatArgs {
    /// Files or directories to format (default: the whole project)
    #[arg(value_name = "PATHS")]
    pub paths: Vec<PathBuf>,

    /// Report the files that would change without rewriting anything
    /// (exit 1 when formatting is needed)
    #[arg(long)]
    pub check: bool,
}

pub fn run(args: FormatArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let loaded = config::load(&cwd, global.config.as_deref(), Layer::default())?;
    for warning in &loaded.warnings {
        term::warn(warning);
    }

    let root = fmt_lint::project_root(
        &cwd,
        global.config.is_some(),
        loaded.project_path.as_deref(),
    );
    let discovered = fmt_lint::discover(&cwd, &args.paths, &loaded.config.format, &root, "format")?;
    for warning in &discovered.warnings {
        term::warn(warning);
    }
    if discovered.file_count == 0 {
        term::println(
            "no files to format: nothing under the target paths matches the \
             enabled languages (see `[format]` in togi.toml)",
        );
        return Ok(());
    }

    let registry = AdapterRegistry::with_defaults();
    let provider = InstalledToolPaths::new(&loaded.config, "togi format", global.verbose);
    let ctx = ToolCtx::new(&provider, &loaded.config, global.verbose);
    let runs = format_all(&registry, &discovered.groups, args.check, &ctx);

    let mut total = FormatOutcome::default();
    let mut failed = 0usize;
    for run in runs {
        match run.result {
            Ok(outcome) => total.merge(outcome),
            Err(err) => {
                // One tool failing must not hide the others' results.
                failed += 1;
                term::error(&err.context(format!(
                    "the {} could not finish",
                    togi_core::tools::label_for(run.adapter)
                )));
            }
        }
    }

    // Normalize the changed-file paths to project-root-relative, so the
    // report reads uniformly no matter which tool reported which file.
    for path in &mut total.changed {
        *path = fmt_lint::relativize_path(path, &cwd, &root);
    }

    if args.check {
        for path in &total.changed {
            term::println(&format!("would reformat: {}", path.display()));
        }
    }

    match run_failure(args.check, total.changed_count(), total.processed, failed) {
        None => {
            term::success(&summary(args.check, &total));
            Ok(())
        }
        Some((message, hint)) => {
            if !args.check && total.processed > 0 {
                // Partial results from the tools that did run.
                term::println(&summary(false, &total));
            }
            Err(anyhow::anyhow!(message)).hint(hint)
        }
    }
}

/// The success line (rendered with a `✓` by `term::success`).
fn summary(check: bool, total: &FormatOutcome) -> String {
    if check {
        format!(
            "{} checked, nothing would change",
            fmt_lint::count(total.processed, "file")
        )
    } else {
        format!(
            "{} formatted, {} changed",
            fmt_lint::count(total.processed, "file"),
            total.changed_count()
        )
    }
}

/// Whether the run failed (exit 1), and with what message and hint: a
/// crashing tool always fails; `--check` also fails when anything would
/// change. An in-place run that rewrote files is a success.
fn run_failure(
    check: bool,
    changed: usize,
    processed: usize,
    failed_tools: usize,
) -> Option<(String, String)> {
    let mut parts = Vec::new();
    if check && changed > 0 {
        parts.push(format!(
            "{changed} of {processed} files would be reformatted"
        ));
    }
    if failed_tools > 0 {
        parts.push(format!(
            "{} could not run",
            fmt_lint::count(failed_tools, "formatter")
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let hint = if failed_tools > 0 {
        "see the errors above; fix what the tools reported (or exclude the \
         affected files) and rerun"
    } else {
        "run `togi format` to apply the changes"
    };
    Some((parts.join(" and "), hint.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn outcome(processed: usize, changed: &[&str]) -> FormatOutcome {
        FormatOutcome {
            processed,
            changed: changed.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn summary_counts_processed_and_changed_files() {
        // Rendered as "✓ 42 files formatted, 3 changed" — term::success
        // adds the ✓.
        assert_eq!(
            summary(false, &outcome(42, &["a.R", "b.py", "c.sql"])),
            "42 files formatted, 3 changed"
        );
    }

    #[test]
    fn summary_singular_file() {
        assert_eq!(
            summary(false, &outcome(1, &[])),
            "1 file formatted, 0 changed"
        );
    }

    #[test]
    fn check_summary_says_nothing_would_change() {
        assert_eq!(
            summary(true, &outcome(8, &[])),
            "8 files checked, nothing would change"
        );
    }

    #[test]
    fn a_clean_run_is_not_a_failure() {
        assert_eq!(
            run_failure(false, 3, 8, 0),
            None,
            "in-place changes are success"
        );
        assert_eq!(run_failure(true, 0, 8, 0), None, "clean check passes");
    }

    #[test]
    fn check_mode_fails_when_files_would_change() {
        let (message, hint) = run_failure(true, 5, 8, 0).expect("changes fail --check");
        assert_eq!(message, "5 of 8 files would be reformatted");
        assert!(hint.contains("togi format"), "{hint}");
    }

    #[test]
    fn a_crashed_tool_fails_the_run_even_in_write_mode() {
        let (message, hint) = run_failure(false, 0, 5, 1).expect("tool failure fails");
        assert_eq!(message, "1 formatter could not run");
        assert!(hint.contains("errors above"), "{hint}");
    }

    #[test]
    fn check_mode_reports_both_changes_and_tool_failures() {
        let (message, _) = run_failure(true, 2, 8, 2).expect("worst outcome wins");
        assert_eq!(
            message,
            "2 of 8 files would be reformatted and 2 formatters could not run"
        );
    }
}
