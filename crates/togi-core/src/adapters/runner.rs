//! Parallel batch runner: hands each adapter its whole file batch in one
//! call, runs adapters (not files) in parallel, and reports results in a
//! deterministic order.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rayon::prelude::*;

use crate::adapters::{Adapter, AdapterRegistry, Diagnostic, FormatOutcome, ToolCtx};
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
        })
        .collect()
}

/// One adapter's whole workload for a run.
struct Batch {
    adapter: Arc<dyn Adapter>,
    files: Vec<PathBuf>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::adapters::test_support::{ConcurrencyGauge, FakeAdapter, FakeToolPaths};
    use crate::adapters::{Position, Range, Severity};
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
}
