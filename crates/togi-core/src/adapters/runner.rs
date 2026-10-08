//! Parallel batch runner: hands each adapter its whole file batch in one
//! call, runs adapters (not files) in parallel, and reports results in a
//! deterministic order.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use crate::adapters::{
    Adapter, AdapterRegistry, Diagnostic, FormatOutcome, ProjectLinter, ProjectScope, ToolCtx,
};
use crate::fsx::Language;

/// One adapter's format result; `adapter` is its stable name.
#[derive(Debug)]
pub struct FormatRun {
    pub adapter: &'static str,
    pub result: anyhow::Result<FormatOutcome>,
}

/// One adapter's lint result; `adapter` is its stable name.
#[derive(Debug)]
pub struct LintRun {
    pub adapter: &'static str,
    pub result: anyhow::Result<Vec<Diagnostic>>,
    pub notes: Vec<String>,
}

/// Format every batch, one adapter invocation per underlying tool, in
/// parallel across adapters. Results come back sorted by adapter name
/// regardless of completion order; a failing adapter yields an `Err` entry
/// without hiding the others. Languages with no registered adapter are
/// skipped.
pub fn format_all(
    registry: &AdapterRegistry,
    groups: &BTreeMap<Language, Vec<PathBuf>>,
    check: bool,
    ctx: &ToolCtx,
) -> Vec<FormatRun> {
    batches(registry, groups)
        .par_iter()
        .map(|batch| FormatRun {
            adapter: batch.adapter.name(),
            result: batch.adapter.format(&batch.files, check, ctx),
        })
        .collect()
}

/// Lint every batch; same batching, parallelism, and ordering rules as
/// [`format_all`].
pub fn lint_all(
    registry: &AdapterRegistry,
    groups: &BTreeMap<Language, Vec<PathBuf>>,
    fix: bool,
    ctx: &ToolCtx,
) -> Vec<LintRun> {
    batches(registry, groups)
        .par_iter()
        .map(|batch| LintRun {
            adapter: batch.adapter.name(),
            result: batch.adapter.lint(&batch.files, fix, ctx),
            notes: Vec::new(),
        })
        .collect()
}

/// Lint file batches and project scopes. Ordinary runs execute all tools in
/// one parallel pass. Fixing runs complete every file adapter before project
/// linters start, so project analysis observes the rewritten files.
pub fn lint_all_in_project(
    registry: &AdapterRegistry,
    groups: &BTreeMap<Language, Vec<PathBuf>>,
    root: &Path,
    whole_project: bool,
    fix: bool,
    ctx: &ToolCtx,
) -> Vec<LintRun> {
    let file_batches = batches(registry, groups);
    let project_batches = project_batches(registry, groups, root, whole_project);

    let mut runs = if fix {
        let mut file_runs: Vec<LintRun> = file_batches
            .par_iter()
            .map(|batch| run_file_linter(batch, true, ctx))
            .collect();
        let project_runs = project_batches
            .par_iter()
            .map(|batch| run_project_linter(batch, ctx))
            .collect::<Vec<_>>();
        file_runs.extend(project_runs);
        file_runs
    } else {
        let mut tasks = Vec::with_capacity(file_batches.len() + project_batches.len());
        tasks.extend(file_batches.iter().map(LintTask::File));
        tasks.extend(project_batches.iter().map(LintTask::Project));
        tasks
            .par_iter()
            .map(|task| match task {
                LintTask::File(batch) => run_file_linter(batch, false, ctx),
                LintTask::Project(batch) => run_project_linter(batch, ctx),
            })
            .collect()
    };
    runs.sort_by_key(|run| run.adapter);
    runs
}

fn run_file_linter(batch: &Batch, fix: bool, ctx: &ToolCtx) -> LintRun {
    LintRun {
        adapter: batch.adapter.name(),
        result: batch.adapter.lint(&batch.files, fix, ctx),
        notes: Vec::new(),
    }
}

fn run_project_linter(batch: &ProjectBatch, ctx: &ToolCtx) -> LintRun {
    match batch.linter.lint_project(&batch.scope, ctx) {
        Ok(outcome) => LintRun {
            adapter: batch.linter.name(),
            result: Ok(outcome.diagnostics),
            notes: outcome.notes,
        },
        Err(err) => LintRun {
            adapter: batch.linter.name(),
            result: Err(err),
            notes: Vec::new(),
        },
    }
}

enum LintTask<'a> {
    File(&'a Batch),
    Project(&'a ProjectBatch),
}

/// One adapter's whole workload for a run.
struct Batch {
    adapter: Arc<dyn Adapter>,
    files: Vec<PathBuf>,
}

struct ProjectBatch {
    linter: Arc<dyn ProjectLinter>,
    scope: ProjectScope,
}

/// Fold language buckets into per-adapter batches, keyed and sorted by
/// adapter name.
///
/// An adapter registered for several languages gets one batch holding all
/// of their files (in `Language` bucket order), so it is still invoked
/// exactly once. The name-sorted `Vec` is what makes result order
/// deterministic: rayon's indexed `collect` preserves input order no matter
/// which adapter finishes first.
fn batches(registry: &AdapterRegistry, groups: &BTreeMap<Language, Vec<PathBuf>>) -> Vec<Batch> {
    let mut by_name: BTreeMap<&'static str, Batch> = BTreeMap::new();
    for (&language, files) in groups {
        if files.is_empty() {
            continue;
        }
        let Some(adapter) = registry.adapter_for(language) else {
            // No adapter for this language: its files are simply not
            // format/lint targets in this build. The commands decide
            // whether that deserves a mention.
            continue;
        };
        by_name
            .entry(adapter.name())
            .or_insert_with(|| Batch {
                adapter: Arc::clone(adapter),
                files: Vec::new(),
            })
            .files
            .extend(files.iter().cloned());
    }
    by_name.into_values().collect()
}

/// Merge registrations for the same project linter name into one scope.
fn project_batches(
    registry: &AdapterRegistry,
    groups: &BTreeMap<Language, Vec<PathBuf>>,
    root: &Path,
    whole_project: bool,
) -> Vec<ProjectBatch> {
    let mut by_name: BTreeMap<&'static str, ProjectBatch> = BTreeMap::new();
    for (&language, files) in groups {
        if files.is_empty() {
            continue;
        }
        for linter in registry.project_linters_for(language) {
            by_name
                .entry(linter.name())
                .or_insert_with(|| ProjectBatch {
                    linter: Arc::clone(linter),
                    scope: ProjectScope {
                        root: root.to_path_buf(),
                        files: Vec::new(),
                        whole_project,
                    },
                })
                .scope
                .files
                .extend(files.iter().cloned());
        }
    }
    by_name.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    use crate::adapters::test_support::{
        ConcurrencyGauge, FakeAdapter, FakeProjectLinter, FakeToolPaths,
    };
    use crate::adapters::{Formatter, Linter, Position, ProjectLinter, Range, Severity};
    use crate::config::Config;
    use crate::fsx::{ExtensionRegistry, group_by_language};

    /// Bucket `files` exactly the way the commands will: through the fsx
    /// extension registry.
    fn grouped(files: &[&str]) -> BTreeMap<Language, Vec<PathBuf>> {
        let paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        group_by_language(&paths, &ExtensionRegistry::with_defaults())
    }

    fn paths(files: &[&str]) -> Vec<PathBuf> {
        files.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn format_hands_each_adapter_its_whole_batch_in_one_call() {
        let ruff = Arc::new(FakeAdapter::new("ruff"));
        let air = Arc::new(FakeAdapter::new("air"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);
        registry.register(Language::R, Arc::clone(&air) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        format_all(
            &registry,
            &grouped(&["a.py", "model.R", "b.ipynb"]),
            false,
            &ctx,
        );

        // One invocation per adapter, holding that adapter's whole batch —
        // never one process per file.
        let ruff_calls = ruff.format_calls();
        assert_eq!(ruff_calls.len(), 1);
        assert_eq!(ruff_calls[0].files, paths(&["a.py", "b.ipynb"]));
        let air_calls = air.format_calls();
        assert_eq!(air_calls.len(), 1);
        assert_eq!(air_calls[0].files, paths(&["model.R"]));
    }

    #[test]
    fn buckets_sharing_an_adapter_merge_into_one_invocation() {
        // Quarto and Markdown both route to the markdown formatter; it
        // should still be invoked exactly once, with both buckets' files.
        let panache = Arc::new(FakeAdapter::new("panache"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Quarto, Arc::clone(&panache) as Arc<dyn Adapter>);
        registry.register(Language::Markdown, Arc::clone(&panache) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = format_all(
            &registry,
            &grouped(&["report.qmd", "README.md", "notes.Rmd"]),
            false,
            &ctx,
        );

        let calls = panache.format_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].files,
            paths(&["report.qmd", "notes.Rmd", "README.md"])
        );
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn format_passes_the_check_flag_through() {
        let ruff = Arc::new(FakeAdapter::new("ruff"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        format_all(&registry, &grouped(&["a.py"]), true, &ctx);

        assert!(ruff.format_calls()[0].check);
    }

    #[test]
    fn adapters_run_in_parallel_not_sequentially() {
        let gauge = Arc::new(ConcurrencyGauge::default());
        let delay = Duration::from_millis(150);
        let ruff = Arc::new(
            FakeAdapter::new("ruff")
                .taking(delay)
                .gauged(Arc::clone(&gauge)),
        );
        let air = Arc::new(
            FakeAdapter::new("air")
                .taking(delay)
                .gauged(Arc::clone(&gauge)),
        );
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, ruff as Arc<dyn Adapter>);
        registry.register(Language::R, air as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        // A private two-thread pool: the assertion must not depend on the
        // global pool's size (e.g. RAYON_NUM_THREADS=1 in the environment).
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("build a two-thread rayon pool");
        pool.install(|| format_all(&registry, &grouped(&["a.py", "b.R"]), false, &ctx));

        assert_eq!(gauge.peak(), 2, "both adapters should be in flight at once");
    }

    #[test]
    fn results_come_back_sorted_by_adapter_name_not_completion_order() {
        // "zeta" finishes long before "air"; the results must still be
        // ordered air, zeta.
        let air = Arc::new(FakeAdapter::new("air").taking(Duration::from_millis(100)));
        let zeta = Arc::new(FakeAdapter::new("zeta"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::R, air as Arc<dyn Adapter>);
        registry.register(Language::Sql, zeta as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = format_all(&registry, &grouped(&["a.R", "q.sql"]), false, &ctx);

        let order: Vec<&str> = runs.iter().map(|run| run.adapter).collect();
        assert_eq!(order, vec!["air", "zeta"]);
    }

    #[test]
    fn a_failing_adapter_does_not_hide_the_other_results() {
        let air = Arc::new(FakeAdapter::new("air").failing("air exploded"));
        let ruff = Arc::new(FakeAdapter::new("ruff").changing(&["a.py"]));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::R, air as Arc<dyn Adapter>);
        registry.register(Language::Python, ruff as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = format_all(&registry, &grouped(&["m.R", "a.py"]), false, &ctx);

        assert_eq!(runs.len(), 2);
        let failure = runs[0]
            .result
            .as_ref()
            .expect_err("air was scripted to fail");
        assert!(failure.to_string().contains("air exploded"), "{failure}");
        let outcome = runs[1].result.as_ref().expect("ruff succeeded");
        assert_eq!(outcome.changed, paths(&["a.py"]));
    }

    #[test]
    fn format_outcomes_aggregate_into_a_run_wide_summary() {
        let ruff = Arc::new(FakeAdapter::new("ruff").changing(&["a.py"]));
        let air = Arc::new(FakeAdapter::new("air").changing(&["m.R", "n.R"]));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, ruff as Arc<dyn Adapter>);
        registry.register(Language::R, air as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = format_all(
            &registry,
            &grouped(&["a.py", "b.py", "m.R", "n.R", "o.R"]),
            false,
            &ctx,
        );

        let mut total = FormatOutcome::default();
        for run in runs {
            total.merge(run.result.expect("all fakes succeed"));
        }
        assert_eq!(total.processed, 5);
        // Runs are name-ordered (air before ruff), so the merged change
        // list is deterministic too.
        assert_eq!(total.changed, paths(&["m.R", "n.R", "a.py"]));
        assert_eq!(total.unchanged(), 2);
    }

    #[test]
    fn languages_without_an_adapter_are_skipped() {
        let ruff = Arc::new(FakeAdapter::new("ruff"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        // The .sql file has no adapter registered; only python runs.
        let runs = format_all(&registry, &grouped(&["a.py", "q.sql"]), false, &ctx);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].adapter, "ruff");
        assert_eq!(ruff.format_calls()[0].files, paths(&["a.py"]));
    }

    #[test]
    fn lint_hands_out_batches_and_collects_diagnostics_in_name_order() {
        let finding = Diagnostic {
            path: PathBuf::from("a.py"),
            range: Some(Range {
                start: Position { line: 1, col: 1 },
                end: None,
            }),
            code: Some("F401".to_string()),
            severity: Severity::Warning,
            message: "unused import".to_string(),
            fixable: true,
        };
        let ruff = Arc::new(FakeAdapter::new("ruff").finding(vec![finding.clone()]));
        let air = Arc::new(FakeAdapter::new("air"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);
        registry.register(Language::R, Arc::clone(&air) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = lint_all(&registry, &grouped(&["a.py", "m.R"]), true, &ctx);

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].adapter, "air");
        assert!(runs[0].result.as_ref().expect("air ran").is_empty());
        assert_eq!(runs[1].adapter, "ruff");
        assert_eq!(
            runs[1].result.as_ref().expect("ruff ran").as_slice(),
            &[finding]
        );
        // The fix flag reached the adapters.
        assert!(ruff.lint_calls()[0].fix);
        assert_eq!(air.lint_calls()[0].files, paths(&["m.R"]));
    }

    #[test]
    fn adapters_resolve_their_tool_through_the_injected_provider() {
        // Adapters never call the installer directly; the ctx carries the
        // provider, and here it is a fake with a canned path.
        let ruff = Arc::new(FakeAdapter::new("ruff").resolving("ruff"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::with_tool("ruff", "/fake/tools/ruff");
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = format_all(&registry, &grouped(&["a.py"]), false, &ctx);

        assert!(runs[0].result.is_ok());
        assert_eq!(ruff.resolved_paths(), paths(&["/fake/tools/ruff"]));
        assert_eq!(provider.requests(), vec!["ruff".to_string()]);
    }

    #[test]
    fn empty_input_yields_no_runs() {
        let ruff = Arc::new(FakeAdapter::new("ruff"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);

        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let runs = format_all(&registry, &BTreeMap::new(), false, &ctx);

        assert!(runs.is_empty());
        assert!(ruff.format_calls().is_empty());
    }

    #[test]
    fn project_linter_runs_once_with_its_selected_language_scope() {
        let project = Arc::new(FakeProjectLinter::new("deptry"));
        let mut registry = AdapterRegistry::new();
        registry.register_project_linter(
            Language::Python,
            Arc::clone(&project) as Arc<dyn ProjectLinter>,
        );
        registry.register_project_linter(
            Language::Quarto,
            Arc::clone(&project) as Arc<dyn ProjectLinter>,
        );
        let root = PathBuf::from("/workspace/project");
        let groups = grouped(&[
            "/workspace/project/a.py",
            "/workspace/project/notebook.qmd",
            "/workspace/project/model.R",
        ]);
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let runs = lint_all_in_project(&registry, &groups, &root, true, false, &ctx);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].adapter, "deptry");
        let calls = project.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].root, root);
        assert_eq!(
            calls[0].files,
            paths(&["/workspace/project/a.py", "/workspace/project/notebook.qmd"])
        );
        assert!(calls[0].whole_project);
    }

    #[test]
    fn project_linter_skips_empty_selected_language_buckets() {
        let project = Arc::new(FakeProjectLinter::new("deptry"));
        let mut registry = AdapterRegistry::new();
        registry.register_project_linter(
            Language::Python,
            Arc::clone(&project) as Arc<dyn ProjectLinter>,
        );
        let groups = BTreeMap::from([
            (Language::Python, Vec::new()),
            (Language::R, paths(&["/workspace/project/model.R"])),
        ]);
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let runs = lint_all_in_project(
            &registry,
            &groups,
            Path::new("/workspace/project"),
            false,
            false,
            &ctx,
        );

        assert!(runs.is_empty());
        assert!(project.calls().is_empty());
    }

    #[test]
    fn project_linters_are_never_dispatched_by_format() {
        let project = Arc::new(FakeProjectLinter::new("deptry"));
        let mut registry = AdapterRegistry::new();
        registry.register_project_linter(
            Language::Python,
            Arc::clone(&project) as Arc<dyn ProjectLinter>,
        );
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let runs = format_all(&registry, &grouped(&["a.py"]), false, &ctx);

        assert!(runs.is_empty());
        assert!(project.calls().is_empty());
    }

    #[test]
    fn project_failure_keeps_adapter_diagnostics_and_results_are_name_sorted() {
        let finding = Diagnostic {
            path: PathBuf::from("/workspace/project/a.py"),
            range: None,
            code: Some("F401".to_string()),
            severity: Severity::Warning,
            message: "unused import".to_string(),
            fixable: true,
        };
        let ruff = Arc::new(FakeAdapter::new("ruff").finding(vec![finding.clone()]));
        let deptry = Arc::new(FakeProjectLinter::new("deptry").failing("deptry exploded"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, ruff as Arc<dyn Adapter>);
        registry.register_project_linter(Language::Python, deptry as Arc<dyn ProjectLinter>);
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let runs = lint_all_in_project(
            &registry,
            &grouped(&["/workspace/project/a.py"]),
            Path::new("/workspace/project"),
            false,
            false,
            &ctx,
        );

        assert_eq!(
            runs.iter().map(|run| run.adapter).collect::<Vec<_>>(),
            ["deptry", "ruff"]
        );
        assert!(
            runs[0]
                .result
                .as_ref()
                .expect_err("deptry fails")
                .to_string()
                .contains("deptry exploded")
        );
        assert_eq!(runs[1].result.as_ref().expect("ruff succeeds"), &[finding]);
    }

    #[test]
    fn project_notes_are_data_and_do_not_turn_a_clean_result_into_an_error() {
        let project = Arc::new(
            FakeProjectLinter::new("deptry")
                .returning(Vec::new(), &["skipped the Python dependency check"]),
        );
        let mut registry = AdapterRegistry::new();
        registry.register_project_linter(Language::Python, project as Arc<dyn ProjectLinter>);
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let runs = lint_all_in_project(
            &registry,
            &grouped(&["/workspace/project/a.py"]),
            Path::new("/workspace/project"),
            true,
            false,
            &ctx,
        );

        assert_eq!(runs.len(), 1);
        assert!(
            runs[0]
                .result
                .as_ref()
                .expect("note is not failure")
                .is_empty()
        );
        assert_eq!(runs[0].notes, ["skipped the Python dependency check"]);
    }

    #[test]
    fn fix_mode_finishes_file_adapters_before_project_linters_start() {
        struct FixingAdapter {
            events: Arc<std::sync::Mutex<Vec<&'static str>>>,
        }

        impl Formatter for FixingAdapter {
            fn format(
                &self,
                files: &[PathBuf],
                _check: bool,
                _ctx: &ToolCtx,
            ) -> anyhow::Result<FormatOutcome> {
                Ok(FormatOutcome {
                    processed: files.len(),
                    changed: Vec::new(),
                })
            }
        }

        impl Linter for FixingAdapter {
            fn lint(
                &self,
                _files: &[PathBuf],
                fix: bool,
                _ctx: &ToolCtx,
            ) -> anyhow::Result<Vec<Diagnostic>> {
                assert!(fix);
                self.events
                    .lock()
                    .expect("events lock")
                    .push("adapter-complete");
                Ok(Vec::new())
            }
        }

        impl Adapter for FixingAdapter {
            fn name(&self) -> &'static str {
                "ruff"
            }
        }

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let project = Arc::new(FakeProjectLinter::new("deptry").recording(Arc::clone(&events)));
        let mut registry = AdapterRegistry::new();
        registry.register(
            Language::Python,
            Arc::new(FixingAdapter {
                events: Arc::clone(&events),
            }),
        );
        registry.register_project_linter(Language::Python, project as Arc<dyn ProjectLinter>);
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        lint_all_in_project(
            &registry,
            &grouped(&["/workspace/project/a.py"]),
            Path::new("/workspace/project"),
            true,
            true,
            &ctx,
        );

        assert_eq!(
            *events.lock().expect("events lock"),
            ["adapter-complete", "project"]
        );
    }

    #[test]
    fn project_scope_keeps_absolute_files_when_cwd_differs_from_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("project");
        let cwd = root.join("nested");
        let file = cwd.join("a.py");
        let project = Arc::new(FakeProjectLinter::new("deptry"));
        let mut registry = AdapterRegistry::new();
        registry.register_project_linter(
            Language::Python,
            Arc::clone(&project) as Arc<dyn ProjectLinter>,
        );
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        lint_all_in_project(
            &registry,
            &grouped(&[file.to_str().expect("utf8 temp path")]),
            &root,
            false,
            false,
            &ctx,
        );

        let calls = project.calls();
        assert_eq!(calls[0].root, root);
        assert_eq!(calls[0].files, [file]);
        assert!(calls[0].files[0].is_absolute());
        assert!(!calls[0].whole_project);
    }

    #[test]
    fn cwd_relative_files_stay_short_for_adapters_and_are_absolute_for_project_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("project-root");
        let cwd = dir.path().join("a-very-long-invocation-directory-name");
        let ruff = Arc::new(FakeAdapter::new("ruff"));
        let project = Arc::new(FakeProjectLinter::new("deptry"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::clone(&ruff) as Arc<dyn Adapter>);
        registry.register_project_linter(
            Language::Python,
            Arc::clone(&project) as Arc<dyn ProjectLinter>,
        );
        let files: Vec<_> = (0..8_000)
            .map(|index| PathBuf::from(format!("src/f{index}.py")))
            .collect();
        let groups = BTreeMap::from([(Language::Python, files.clone())]);
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        lint_all_in_project_from_cwd(&registry, &groups, &cwd, &root, false, false, &ctx);

        let adapter_calls = ruff.lint_calls();
        let adapter_files = &adapter_calls[0].files;
        assert_eq!(adapter_files.len(), 8_000);
        assert_eq!(adapter_files, &files);
        assert!(adapter_files.iter().all(|file| file.is_relative()));
        let project_calls = project.calls();
        assert_eq!(project_calls[0].root, root);
        assert_eq!(project_calls[0].files.len(), 8_000);
        assert_eq!(project_calls[0].files[0], cwd.join("src/f0.py"));
        assert_eq!(project_calls[0].files[7_999], cwd.join("src/f7999.py"));
        assert!(project_calls[0].files.iter().all(|file| file.is_absolute()));
    }
}
