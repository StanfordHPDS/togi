//! `togi lint` — report lint violations across the project, optionally
//! applying safe autofixes first (`--fix`).
//!
//! `--format json` writes the normalized diagnostics — and nothing else —
//! to stdout as a JSON array with the stable [`Diagnostic`] schema;
//! warnings and error summaries go to stderr so the JSON stays parseable.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, ValueEnum};

use togi_core::adapters::{AdapterRegistry, Diagnostic, InstalledToolPaths, ToolCtx, lint_all};
use togi_core::config::{self, Layer};
use togi_core::term::{self, HintExt};

use super::fmt_lint;

#[derive(Debug, Args)]
pub struct LintArgs {
    /// Files or directories to lint (default: the whole project)
    #[arg(value_name = "PATHS")]
    pub paths: Vec<PathBuf>,

    /// Apply safe autofixes first, then report what remains
    #[arg(long)]
    pub fix: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

pub fn run(args: LintArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
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
    let discovered = fmt_lint::discover(&cwd, &args.paths, &loaded.config.lint, &root, "lint")?;
    for warning in &discovered.warnings {
        term::warn(warning);
    }
    if discovered.file_count == 0 {
        match args.format {
            // Machine consumers always get valid JSON on stdout.
            OutputFormat::Json => term::data("[]"),
            OutputFormat::Text => term::println(
                "no files to lint: nothing under the target paths matches the \
                 enabled languages (see `[lint]` in togi.toml)",
            ),
        }
        return Ok(());
    }

    let registry = AdapterRegistry::with_defaults();
    let provider = InstalledToolPaths::new(&loaded.config, "togi lint", global.verbose);
    let ctx = ToolCtx::new(&provider, &loaded.config, global.verbose);
    let runs = lint_all(&registry, &discovered.groups, args.fix, &ctx);

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut failed = 0usize;
    for run in runs {
        match run.result {
            Ok(found) => diagnostics.extend(found),
            Err(err) => {
                // One tool failing must not hide the others' findings.
                failed += 1;
                term::error(&err.context(format!(
                    "the {} could not finish",
                    togi_core::tools::label_for(run.adapter)
                )));
            }
        }
    }
    // Normalize every tool's paths to project-root-relative before both
    // sorting (which orders by path) and reporting, so human and JSON
    // output are uniform regardless of which adapter produced a finding.
    fmt_lint::relativize_diagnostics(&mut diagnostics, &cwd, &root);
    fmt_lint::sort_diagnostics(&mut diagnostics);

    match args.format {
        OutputFormat::Json => term::data(
            &serde_json::to_string_pretty(&diagnostics)
                .context("could not serialize the diagnostics to JSON")?,
        ),
        OutputFormat::Text => {
            let use_color = term::stdout_colors();
            for diagnostic in &diagnostics {
                term::println(&fmt_lint::render_diagnostic(diagnostic, use_color));
            }
        }
    }

    let fixable = diagnostics.iter().filter(|d| d.fixable).count();
    match run_failure(diagnostics.len(), fixable, failed, args.fix) {
        None => {
            if args.format == OutputFormat::Text {
                term::success(&format!(
                    "no issues found in {}",
                    fmt_lint::count(discovered.file_count, "file")
                ));
            }
            Ok(())
        }
        Some((message, hint)) => Err(anyhow::anyhow!(message)).hint(hint),
    }
}

/// Whether the run failed (exit 1), and with what message and hint: any
/// remaining violation fails, as does a linter that crashed.
fn run_failure(
    found: usize,
    fixable: usize,
    failed_tools: usize,
    fix_used: bool,
) -> Option<(String, String)> {
    let mut parts = Vec::new();
    if found > 0 {
        let mut part = format!("found {}", fmt_lint::count(found, "issue"));
        if fixable > 0 && !fix_used {
            part.push_str(&format!(" ({fixable} fixable with `togi lint --fix`)"));
        }
        parts.push(part);
    }
    if failed_tools > 0 {
        parts.push(format!(
            "{} could not run",
            fmt_lint::count(failed_tools, "linter")
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let hint = if failed_tools > 0 {
        "see the errors above; fix what the tools reported (or exclude the \
         affected files) and rerun"
    } else if fixable > 0 && !fix_used {
        "run `togi lint --fix` to apply the safe fixes, then fix the rest by hand"
    } else {
        "fix the issues listed above and rerun `togi lint`"
    };
    Some((parts.join(" and "), hint.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_run_is_not_a_failure() {
        assert_eq!(run_failure(0, 0, 0, false), None);
        assert_eq!(run_failure(0, 0, 0, true), None);
    }

    #[test]
    fn violations_fail_and_advertise_the_fix_flag() {
        let (message, hint) = run_failure(5, 2, 0, false).expect("violations fail");
        assert_eq!(message, "found 5 issues (2 fixable with `togi lint --fix`)");
        assert!(hint.contains("togi lint --fix"), "{hint}");
    }

    #[test]
    fn a_single_issue_reads_singular() {
        let (message, hint) = run_failure(1, 0, 0, false).expect("violations fail");
        assert_eq!(message, "found 1 issue");
        assert!(hint.contains("fix the issues"), "{hint}");
    }

    #[test]
    fn after_fix_the_message_stops_advertising_the_flag() {
        // The safe fixes were already applied; pointing at --fix again
        // would send the user in a circle.
        let (message, hint) = run_failure(1, 1, 0, true).expect("remaining issues fail");
        assert_eq!(message, "found 1 issue");
        assert!(!message.contains("--fix"), "{message}");
        assert!(!hint.contains("--fix"), "{hint}");
    }

    #[test]
    fn a_crashed_linter_fails_the_run() {
        let (message, hint) = run_failure(0, 0, 1, false).expect("tool failure fails");
        assert_eq!(message, "1 linter could not run");
        assert!(hint.contains("errors above"), "{hint}");
    }

    #[test]
    fn violations_and_tool_failures_combine() {
        let (message, _) = run_failure(2, 0, 1, false).expect("worst outcome");
        assert_eq!(message, "found 2 issues and 1 linter could not run");
    }
}
