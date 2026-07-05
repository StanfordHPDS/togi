//! Test-only fakes for the adapter layer: a scriptable [`FakeAdapter`]
//! that records how the runner calls it, and a [`FakeToolPaths`] provider
//! so nothing ever touches the real tool installer.
//!
//! Real adapter tests follow the same pattern: build a `ToolCtx` over
//! `FakeToolPaths`, call the adapter, assert on the returned data.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::adapters::{Adapter, Diagnostic, FormatOutcome, Formatter, Linter, ToolCtx, ToolPaths};

/// One recorded `format` call: the batch and the `check` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormatCall {
    pub files: Vec<PathBuf>,
    pub check: bool,
}

/// One recorded `lint` call: the batch and the `fix` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LintCall {
    pub files: Vec<PathBuf>,
    pub fix: bool,
}

/// A scriptable adapter that records every call the runner makes.
#[derive(Default)]
pub(crate) struct FakeAdapter {
    name: &'static str,
    format_calls: Mutex<Vec<FormatCall>>,
    lint_calls: Mutex<Vec<LintCall>>,
    /// Files reported as changed by every `format` call.
    changed: Vec<PathBuf>,
    /// Findings reported by every `lint` call.
    diagnostics: Vec<Diagnostic>,
    /// Tool name to resolve through the ctx on each call, exercising
    /// injected [`ToolPaths`]; resolved paths are recorded.
    resolves: Option<&'static str>,
    resolved: Mutex<Vec<PathBuf>>,
    /// Error message every call fails with, when set.
    fails_with: Option<String>,
    /// How long each call takes (for scheduling-sensitive runner tests).
    delay: Duration,
    gauge: Option<std::sync::Arc<ConcurrencyGauge>>,
}

impl FakeAdapter {
    pub fn new(name: &'static str) -> FakeAdapter {
        FakeAdapter {
            name,
            ..FakeAdapter::default()
        }
    }

    /// Report these files as changed from every `format` call.
    pub fn changing(mut self, files: &[&str]) -> FakeAdapter {
        self.changed = files.iter().map(PathBuf::from).collect();
        self
    }

    /// Report these findings from every `lint` call.
    pub fn finding(mut self, diagnostics: Vec<Diagnostic>) -> FakeAdapter {
        self.diagnostics = diagnostics;
        self
    }

    /// Resolve `tool` through the ctx on every call, recording the path.
    pub fn resolving(mut self, tool: &'static str) -> FakeAdapter {
        self.resolves = Some(tool);
        self
    }

    /// Fail every call with this message.
    pub fn failing(mut self, message: &str) -> FakeAdapter {
        self.fails_with = Some(message.to_string());
        self
    }

    /// Sleep this long inside every call.
    pub fn taking(mut self, delay: Duration) -> FakeAdapter {
        self.delay = delay;
        self
    }

    /// Track this call's concurrency on `gauge`.
    pub fn gauged(mut self, gauge: std::sync::Arc<ConcurrencyGauge>) -> FakeAdapter {
        self.gauge = Some(gauge);
        self
    }

    pub fn format_calls(&self) -> Vec<FormatCall> {
        self.format_calls.lock().expect("format calls lock").clone()
    }

    pub fn lint_calls(&self) -> Vec<LintCall> {
        self.lint_calls.lock().expect("lint calls lock").clone()
    }

    /// Tool paths resolved through the ctx, in call order.
    pub fn resolved_paths(&self) -> Vec<PathBuf> {
        self.resolved.lock().expect("resolved lock").clone()
    }

    /// The shared per-call behavior: concurrency tracking, simulated work,
    /// ctx-based tool resolution, scripted failure.
    fn run_call(&self, ctx: &ToolCtx) -> anyhow::Result<()> {
        let _guard = self.gauge.as_deref().map(ConcurrencyGauge::enter);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        if let Some(tool) = self.resolves {
            let path = ctx.tool_path(tool)?;
            self.resolved.lock().expect("resolved lock").push(path);
        }
        match &self.fails_with {
            Some(message) => Err(anyhow::anyhow!("{message}")),
            None => Ok(()),
        }
    }
}

impl Formatter for FakeAdapter {
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome> {
        self.format_calls
            .lock()
            .expect("format calls lock")
            .push(FormatCall {
                files: files.to_vec(),
                check,
            });
        self.run_call(ctx)?;
        Ok(FormatOutcome {
            processed: files.len(),
            changed: self.changed.clone(),
        })
    }
}

impl Linter for FakeAdapter {
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>> {
        self.lint_calls
            .lock()
            .expect("lint calls lock")
            .push(LintCall {
                files: files.to_vec(),
                fix,
            });
        self.run_call(ctx)?;
        Ok(self.diagnostics.clone())
    }
}

impl Adapter for FakeAdapter {
    fn name(&self) -> &'static str {
        self.name
    }
}

/// Records the highest number of overlapping calls, proving (or
/// disproving) that adapters ran in parallel.
#[derive(Default)]
pub(crate) struct ConcurrencyGauge {
    current: AtomicUsize,
    peak: AtomicUsize,
}

/// Decrements the gauge when the tracked call finishes.
pub(crate) struct GaugeGuard<'a> {
    gauge: &'a ConcurrencyGauge,
}

impl ConcurrencyGauge {
    fn enter(&self) -> GaugeGuard<'_> {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        GaugeGuard { gauge: self }
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.gauge.current.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A [`ToolPaths`] fake: canned name → path answers, plus a request log.
#[derive(Default)]
pub(crate) struct FakeToolPaths {
    paths: BTreeMap<String, PathBuf>,
    requests: Mutex<Vec<String>>,
}

impl FakeToolPaths {
    pub fn with_tool(tool: &str, path: &str) -> FakeToolPaths {
        FakeToolPaths {
            paths: BTreeMap::from([(tool.to_string(), PathBuf::from(path))]),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Add (or replace) a canned answer for `tool`.
    pub fn insert(&mut self, tool: &str, path: &std::path::Path) {
        self.paths.insert(tool.to_string(), path.to_path_buf());
    }

    /// Tool names requested through this provider, in call order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl ToolPaths for FakeToolPaths {
    fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(tool.to_string());
        self.paths
            .get(tool)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fake provider has no tool named `{tool}`"))
    }
}
