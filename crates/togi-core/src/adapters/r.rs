//! The R adapter: formatting (and formatting-drift linting) via `air`.
//!
//! # Formatting
//!
//! `format` shells out to `air format` over the whole batch. Check mode maps
//! to `air format --check`, whose stderr (`Would reformat: <path>` lines,
//! exit code 1) says which files would change without touching them. An
//! in-place run does the check pass first — air is silent when it rewrites
//! files, so the check pass is the only way to report *which* files changed
//! — and only then applies.
//!
//! # Linting
//!
//! air ships no separate linter today, so `lint` reports formatting drift
//! using the same `--check` run: each file air would rewrite becomes one
//! file-level, fixable [`Diagnostic`], and `--fix` simply formats in place.
//! Deeper static analysis for R is deliberately *not* reimplemented here;
//! when air (or a successor tool) grows real lint rules, this adapter can
//! absorb them without `togi lint` or its output changing — that is the
//! point of the adapter boundary.
//!
//! # Configuration
//!
//! air discovers a project's own `air.toml` natively, so togi passes no
//! config flags at all. Anything extra comes from the `[tools.air] args`
//! escape hatch in `togi.toml`, appended verbatim before the file list. The
//! binary itself is resolved through [`ToolCtx`], never assumed to be on
//! `PATH`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::Context;

use crate::adapters::{Adapter, Diagnostic, FormatOutcome, Formatter, Linter, Severity, ToolCtx};
use crate::term::HintExt;

/// The `air`-backed adapter for `.R`/`.r` files.
pub struct AirAdapter;

/// The managed tool this adapter resolves through [`ToolCtx`].
const TOOL: &str = "air";

/// Check-mode stderr line naming a file that is not formatted.
const WOULD_REFORMAT_PREFIX: &str = "Would reformat: ";

/// Stderr line naming a file air could not format (e.g. a syntax error);
/// `--no-color` keeps the `ERROR` tag free of ANSI codes.
const FAILURE_PREFIX: &str = "ERROR Failed to format ";

impl Adapter for AirAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }
}

impl Formatter for AirAdapter {
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome> {
        if files.is_empty() {
            // Bare `air format` falls back to formatting the working
            // directory; an empty batch must never turn into that.
            return Ok(FormatOutcome::default());
        }
        let bin = ctx.tool_path(TOOL)?;
        let extra = extra_args(ctx);

        // The check pass answers "which files will change" — air says
        // nothing when formatting in place, so this is the only source of
        // the changed-file list in both modes.
        let (status, stderr) = run_air(ctx, &bin, &invocation(true, extra, files))?;
        let report = parse_stderr(&stderr);
        fail_on_unformattable(&report)?;
        verify_run(status, &report, &stderr, true)?;

        if !check && !report.would_reformat.is_empty() {
            let (status, stderr) = run_air(ctx, &bin, &invocation(false, extra, files))?;
            let apply = parse_stderr(&stderr);
            fail_on_unformattable(&apply)?;
            verify_run(status, &apply, &stderr, false)?;
        }

        Ok(FormatOutcome {
            processed: files.len(),
            changed: report.would_reformat,
        })
    }
}

impl Linter for AirAdapter {
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let bin = ctx.tool_path(TOOL)?;
        // With `--fix` the "safe autofix" for a formatting finding is the
        // formatting itself: one in-place run, and whatever it fixed is no
        // longer a finding. Without it, a check run reports the drift.
        let (status, stderr) = run_air(ctx, &bin, &invocation(!fix, extra_args(ctx), files))?;
        let report = parse_stderr(&stderr);
        verify_run(status, &report, &stderr, !fix)?;

        let mut diagnostics: Vec<Diagnostic> = report
            .failures
            .into_iter()
            .map(|failure| Diagnostic {
                path: failure.path,
                range: None,
                code: None,
                severity: Severity::Error,
                message: format!("air cannot format this file: {}", failure.reason),
                fixable: false,
            })
            .collect();
        diagnostics.extend(report.would_reformat.into_iter().map(|path| {
            Diagnostic {
                path,
                range: None,
                code: None,
                severity: Severity::Warning,
                message: "file is not formatted with air (run `togi format` or `togi lint --fix` \
                      to reformat it)"
                    .to_string(),
                fixable: true,
            }
        }));
        Ok(diagnostics)
    }
}

/// `[tools.air] args` from `togi.toml`, or nothing.
fn extra_args<'c>(ctx: &'c ToolCtx) -> &'c [String] {
    ctx.config.tools.args.get(TOOL).map_or(&[], Vec::as_slice)
}

/// The argument list for one `air format` run: `--check` when only probing,
/// always `--no-color` (air colors stderr even when piped, and the parser
/// wants plain text), then the escape-hatch args, then the files.
fn invocation(check: bool, extra: &[String], files: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![OsString::from("format")];
    if check {
        args.push(OsString::from("--check"));
    }
    args.push(OsString::from("--no-color"));
    args.extend(extra.iter().map(OsString::from));
    args.extend(files.iter().map(|file| file.as_os_str().to_owned()));
    args
}

/// Run air once and capture its exit status and stderr (air writes every
/// message to stderr; stdout stays empty).
fn run_air(ctx: &ToolCtx, bin: &Path, args: &[OsString]) -> anyhow::Result<(ExitStatus, String)> {
    crate::adapters::log_command(ctx, bin, args);
    let output = std::process::Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("could not run air at {}", bin.display()))
        .hint("run `togi tools update` to reinstall the managed air binary")?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((output.status, stderr))
}

/// Everything one air run said on stderr, sorted into what it means.
#[derive(Debug, Default, PartialEq, Eq)]
struct AirReport {
    /// Files a `--check` run says are not formatted, in report order.
    would_reformat: Vec<PathBuf>,
    /// Files air refused to format, with its reason.
    failures: Vec<AirFailure>,
    /// Non-empty lines this parser does not recognize (kept for error
    /// messages, never treated as findings).
    noise: Vec<String>,
}

/// One file air could not format and why (usually a syntax error).
#[derive(Debug, PartialEq, Eq)]
struct AirFailure {
    path: PathBuf,
    reason: String,
}

/// Sort air's stderr lines into would-reformat files, per-file failures,
/// and unrecognized noise.
fn parse_stderr(stderr: &str) -> AirReport {
    let mut report = AirReport::default();
    for line in stderr.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix(WOULD_REFORMAT_PREFIX) {
            report.would_reformat.push(PathBuf::from(path));
        } else if let Some((path, reason)) = line
            .strip_prefix(FAILURE_PREFIX)
            // `": "` cannot appear inside a path (a Windows drive colon is
            // followed by a separator, not a space), so the first match is
            // the path/reason boundary.
            .and_then(|rest| rest.split_once(": "))
        {
            report.failures.push(AirFailure {
                path: PathBuf::from(path),
                reason: reason.to_string(),
            });
        } else {
            report.noise.push(line.to_string());
        }
    }
    report
}

/// Turn per-file failures into one error naming every file and its reason.
fn fail_on_unformattable(report: &AirReport) -> anyhow::Result<()> {
    if report.failures.is_empty() {
        return Ok(());
    }
    let details: Vec<String> = report
        .failures
        .iter()
        .map(|failure| format!("  {}: {}", failure.path.display(), failure.reason))
        .collect();
    Err(anyhow::anyhow!(
        "air could not format {} file{}:\n{}",
        report.failures.len(),
        if report.failures.len() == 1 { "" } else { "s" },
        details.join("\n")
    ))
    .hint("fix the errors in the files above, then rerun the command")
}

/// Reject exit statuses this adapter does not understand.
///
/// Success is 0, or — in check mode — 1 alongside parsed `Would reformat`
/// lines. Parsed per-file failures also pass: the caller has already chosen
/// how to represent them. Anything else (a bad escape-hatch flag, a signal,
/// an air version whose output this parser no longer recognizes) is an
/// error carrying air's own words.
fn verify_run(
    status: ExitStatus,
    report: &AirReport,
    stderr: &str,
    check: bool,
) -> anyhow::Result<()> {
    if !report.failures.is_empty() {
        return Ok(());
    }
    match status.code() {
        Some(0) => Ok(()),
        Some(1) if check && !report.would_reformat.is_empty() => Ok(()),
        _ => {
            let said = if stderr.trim().is_empty() {
                String::from("nothing")
            } else {
                format!("\n{}", stderr.trim_end())
            };
            Err(anyhow::anyhow!(
                "air exited unexpectedly ({status}) and said: {said}"
            ))
            .hint(
                "if `[tools.air] args` is set in togi.toml, check those flags against \
                 `air format --help`; otherwise please report this to the togi maintainers",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::config::Config;

    /// A recorded air stderr fixture (see
    /// `tests/fixtures/tool-output/air/README.md` for how each was made).
    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/air")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn paths(files: &[&str]) -> Vec<PathBuf> {
        files.iter().map(PathBuf::from).collect()
    }

    // ------------------------------------------------------------------
    // Parsing recorded air output.

    #[test]
    fn recorded_check_output_yields_the_would_reformat_list() {
        let report = parse_stderr(&fixture("format-check-would-reformat.txt"));
        assert_eq!(
            report.would_reformat,
            paths(&["analysis/model.r", "needs_formatting.R"])
        );
        assert!(report.failures.is_empty());
        assert!(report.noise.is_empty());
    }

    #[test]
    fn recorded_clean_check_output_yields_an_empty_report() {
        let report = parse_stderr(&fixture("format-check-clean.txt"));
        assert_eq!(report, AirReport::default());
    }

    #[test]
    fn recorded_syntax_error_check_output_captures_failure_and_drift() {
        let report = parse_stderr(&fixture("format-check-syntax-error.txt"));
        assert_eq!(
            report.failures,
            vec![AirFailure {
                path: PathBuf::from("broken.R"),
                reason: "Failed to parse due to syntax errors.".to_string(),
            }]
        );
        assert_eq!(report.would_reformat, paths(&["needs_formatting.R"]));
        assert!(report.noise.is_empty());
    }

    #[test]
    fn recorded_in_place_syntax_error_output_captures_the_failure() {
        let report = parse_stderr(&fixture("format-in-place-syntax-error.txt"));
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, PathBuf::from("broken.R"));
        assert!(report.would_reformat.is_empty());
    }

    #[test]
    fn unrecognized_lines_are_noise_not_findings() {
        let report = parse_stderr("WARN some future air message\n\nWould reformat: a.R\n");
        assert_eq!(report.would_reformat, paths(&["a.R"]));
        assert_eq!(
            report.noise,
            vec!["WARN some future air message".to_string()]
        );
    }

    // ------------------------------------------------------------------
    // Invocation shape.

    #[test]
    fn invocation_builds_check_and_in_place_argument_lists() {
        let files = paths(&["a.R", "dir/b.r"]);
        let check: Vec<OsString> = invocation(true, &[], &files);
        assert_eq!(
            check,
            ["format", "--check", "--no-color", "a.R", "dir/b.r"]
                .map(OsString::from)
                .to_vec()
        );
        let in_place: Vec<OsString> = invocation(false, &[], &files);
        assert_eq!(
            in_place,
            ["format", "--no-color", "a.R", "dir/b.r"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn escape_hatch_args_sit_between_flags_and_files() {
        let files = paths(&["a.R"]);
        let args = invocation(
            true,
            &["--log-level".to_string(), "info".to_string()],
            &files,
        );
        assert_eq!(
            args,
            [
                "format",
                "--check",
                "--no-color",
                "--log-level",
                "info",
                "a.R"
            ]
            .map(OsString::from)
            .to_vec()
        );
    }

    // ------------------------------------------------------------------
    // Adapter behavior with no files: air must never be spawned, because a
    // bare `air format` would format the whole working directory.

    #[test]
    fn empty_batches_never_reach_air() {
        // The provider has no air at all, so any tool lookup would error.
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let outcome = AirAdapter
            .format(&[], false, &ctx)
            .expect("empty format batch");
        assert_eq!(outcome, FormatOutcome::default());
        let diagnostics = AirAdapter.lint(&[], false, &ctx).expect("empty lint batch");
        assert!(diagnostics.is_empty());
        assert!(provider.requests().is_empty());
    }

    // ------------------------------------------------------------------
    // End-to-end through a stub `air` binary that replays the recorded
    // fixtures. The stubs are POSIX shell scripts, so these are unix-only;
    // the parsing and invocation logic they drive is covered for every
    // platform above.

    #[cfg(unix)]
    mod with_stub_air {
        use super::*;

        /// A fake `air` on disk: logs each invocation's arguments, replays
        /// canned stderr, and exits with a canned code — one behavior for
        /// `--check` runs, one for in-place runs.
        struct StubAir {
            dir: tempfile::TempDir,
        }

        impl StubAir {
            fn new(
                check_stderr: &str,
                check_code: i32,
                apply_stderr: &str,
                apply_code: i32,
            ) -> StubAir {
                use std::os::unix::fs::PermissionsExt;

                let dir = tempfile::tempdir().expect("stub air tempdir");
                let root = dir.path();
                std::fs::write(root.join("check-stderr.txt"), check_stderr)
                    .expect("write check stderr");
                std::fs::write(root.join("apply-stderr.txt"), apply_stderr)
                    .expect("write apply stderr");
                let script = format!(
                    "#!/bin/sh\n\
                     printf '%s\\n' \"$*\" >> '{log}'\n\
                     case \" $* \" in\n\
                       *' --check '*) cat '{check}' >&2; exit {check_code};;\n\
                       *) cat '{apply}' >&2; exit {apply_code};;\n\
                     esac\n",
                    log = root.join("calls.log").display(),
                    check = root.join("check-stderr.txt").display(),
                    apply = root.join("apply-stderr.txt").display(),
                );
                let bin = root.join("air");
                std::fs::write(&bin, script).expect("write stub air");
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                    .expect("mark stub air executable");
                StubAir { dir }
            }

            fn bin(&self) -> String {
                self.dir.path().join("air").display().to_string()
            }

            /// One line per invocation: its full argument string.
            fn calls(&self) -> Vec<String> {
                match std::fs::read_to_string(self.dir.path().join("calls.log")) {
                    Ok(log) => log.lines().map(str::to_string).collect(),
                    Err(_) => Vec::new(),
                }
            }
        }

        /// A ctx whose `air` resolves to the stub.
        fn ctx_over<'a>(provider: &'a FakeToolPaths, config: &'a Config) -> ToolCtx<'a> {
            ToolCtx::new(provider, config, false)
        }

        fn provider_for(stub: &StubAir) -> FakeToolPaths {
            FakeToolPaths::with_tool(TOOL, &stub.bin())
        }

        #[test]
        fn format_check_reports_files_air_would_change() {
            let stub = StubAir::new(&fixture("format-check-would-reformat.txt"), 1, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();
            let files = paths(&["analysis/model.r", "needs_formatting.R", "clean.R"]);

            let outcome = AirAdapter
                .format(&files, true, &ctx_over(&provider, &config))
                .expect("check run succeeds");

            assert_eq!(outcome.processed, 3);
            assert_eq!(
                outcome.changed,
                paths(&["analysis/model.r", "needs_formatting.R"])
            );
            // Exactly one probe run, never an in-place one.
            let calls = stub.calls();
            assert_eq!(calls.len(), 1);
            assert!(calls[0].contains("--check"), "{calls:?}");
            assert!(calls[0].contains("--no-color"), "{calls:?}");
        }

        #[test]
        fn format_in_place_applies_after_the_check_pass() {
            let stub = StubAir::new(&fixture("format-check-would-reformat.txt"), 1, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();
            let files = paths(&["analysis/model.r", "needs_formatting.R", "clean.R"]);

            let outcome = AirAdapter
                .format(&files, false, &ctx_over(&provider, &config))
                .expect("in-place run succeeds");

            // The changed list comes from the probe; the second run applied.
            assert_eq!(
                outcome.changed,
                paths(&["analysis/model.r", "needs_formatting.R"])
            );
            let calls = stub.calls();
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(calls[0].contains("--check"), "{calls:?}");
            assert!(!calls[1].contains("--check"), "{calls:?}");
        }

        #[test]
        fn format_in_place_skips_the_apply_pass_when_nothing_would_change() {
            let stub = StubAir::new(&fixture("format-check-clean.txt"), 0, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();

            let outcome = AirAdapter
                .format(&paths(&["clean.R"]), false, &ctx_over(&provider, &config))
                .expect("clean run succeeds");

            assert_eq!(outcome.processed, 1);
            assert!(outcome.changed.is_empty());
            assert_eq!(stub.calls().len(), 1, "no in-place pass for a clean tree");
        }

        #[test]
        fn format_surfaces_syntax_errors_with_next_steps() {
            let stub = StubAir::new(&fixture("format-check-syntax-error.txt"), 255, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();
            let files = paths(&["broken.R", "needs_formatting.R", "clean.R"]);

            let err = AirAdapter
                .format(&files, false, &ctx_over(&provider, &config))
                .expect_err("syntax errors must fail the run");

            let rendered = crate::term::render_error(&err, false);
            assert!(rendered.contains("broken.R"), "names the file: {rendered}");
            assert!(
                rendered.contains("Failed to parse due to syntax errors."),
                "carries air's reason: {rendered}"
            );
            assert!(rendered.contains("hint:"), "says what to do: {rendered}");
            // Nothing gets rewritten while any file cannot be parsed.
            assert_eq!(stub.calls().len(), 1, "no in-place pass after a failure");
        }

        #[test]
        fn format_passes_escape_hatch_args_through() {
            let stub = StubAir::new(&fixture("format-check-clean.txt"), 0, "", 0);
            let provider = provider_for(&stub);
            let mut config = Config::default();
            config
                .tools
                .args
                .insert(TOOL.to_string(), vec!["--force".to_string()]);

            AirAdapter
                .format(&paths(&["a.R"]), true, &ctx_over(&provider, &config))
                .expect("check run succeeds");

            assert!(stub.calls()[0].contains("--force"), "{:?}", stub.calls());
        }

        #[test]
        fn lint_maps_would_reformat_to_fixable_warnings() {
            let stub = StubAir::new(&fixture("format-check-would-reformat.txt"), 1, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();
            let files = paths(&["analysis/model.r", "needs_formatting.R", "clean.R"]);

            let diagnostics = AirAdapter
                .lint(&files, false, &ctx_over(&provider, &config))
                .expect("lint run succeeds");

            assert_eq!(diagnostics.len(), 2);
            for (diagnostic, path) in diagnostics
                .iter()
                .zip(["analysis/model.r", "needs_formatting.R"])
            {
                assert_eq!(diagnostic.path, PathBuf::from(path));
                assert_eq!(diagnostic.severity, Severity::Warning);
                assert!(diagnostic.fixable, "formatting drift is auto-fixable");
                assert_eq!(diagnostic.range, None, "air reports whole files");
                assert_eq!(diagnostic.code, None, "air has no rule codes");
                assert!(
                    diagnostic.message.contains("togi format"),
                    "message says how to fix: {}",
                    diagnostic.message
                );
            }
        }

        #[test]
        fn lint_reports_unformattable_files_as_error_diagnostics() {
            let stub = StubAir::new(&fixture("format-check-syntax-error.txt"), 255, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();
            let files = paths(&["broken.R", "needs_formatting.R", "clean.R"]);

            let diagnostics = AirAdapter
                .lint(&files, false, &ctx_over(&provider, &config))
                .expect("failures become diagnostics, not an error");

            assert_eq!(diagnostics.len(), 2);
            assert_eq!(diagnostics[0].path, PathBuf::from("broken.R"));
            assert_eq!(diagnostics[0].severity, Severity::Error);
            assert!(!diagnostics[0].fixable, "syntax errors need a human");
            assert!(
                diagnostics[0].message.contains("syntax errors"),
                "{}",
                diagnostics[0].message
            );
            assert_eq!(diagnostics[1].path, PathBuf::from("needs_formatting.R"));
            assert_eq!(diagnostics[1].severity, Severity::Warning);
        }

        #[test]
        fn lint_fix_formats_in_place_and_reports_whats_left() {
            let stub = StubAir::new("", 1, &fixture("format-check-clean.txt"), 0);
            let provider = provider_for(&stub);
            let config = Config::default();

            let diagnostics = AirAdapter
                .lint(&paths(&["a.R"]), true, &ctx_over(&provider, &config))
                .expect("fix run succeeds");

            assert!(diagnostics.is_empty(), "everything air fixed is no finding");
            let calls = stub.calls();
            assert_eq!(calls.len(), 1);
            assert!(
                !calls[0].contains("--check"),
                "fix formats in place: {calls:?}"
            );
        }

        #[test]
        fn unexpected_exit_is_an_error_pointing_at_tool_args() {
            // A clap-style usage error, e.g. from a bad `[tools.air] args`
            // flag: exit 2, no line this adapter recognizes.
            let stub = StubAir::new("error: unexpected argument '--nope' found\n", 2, "", 0);
            let provider = provider_for(&stub);
            let config = Config::default();

            let err = AirAdapter
                .format(&paths(&["a.R"]), true, &ctx_over(&provider, &config))
                .expect_err("exit 2 is not a formatting outcome");

            let rendered = crate::term::render_error(&err, false);
            assert!(
                rendered.contains("unexpected argument"),
                "carries air's own words: {rendered}"
            );
            assert!(
                rendered.contains("[tools.air]"),
                "points at the escape hatch: {rendered}"
            );
        }

        #[test]
        fn a_missing_binary_is_an_error_with_a_reinstall_hint() {
            let provider = FakeToolPaths::with_tool(TOOL, "/nonexistent/air");
            let config = Config::default();

            let err = AirAdapter
                .format(&paths(&["a.R"]), true, &ctx_over(&provider, &config))
                .expect_err("the binary does not exist");

            let rendered = crate::term::render_error(&err, false);
            assert!(rendered.contains("togi tools update"), "{rendered}");
        }
    }

    // ------------------------------------------------------------------
    // The real managed air, downloaded and run end to end.

    /// Full round trip against the real pinned air release: download into a
    /// throwaway cache, then lint, format, and re-check a real file.
    #[cfg(feature = "online-tests")]
    #[test]
    #[ignore = "downloads the real air release; run with --features online-tests -- --ignored"]
    fn online_managed_air_formats_and_lints_for_real() {
        use crate::adapters::ToolPaths;
        use crate::tools::{Downloader, InstallContext, Platform, ToolCache, ToolSpec};

        /// Resolves air by actually installing the pinned release into a
        /// test-local cache directory.
        struct DownloadedTools {
            data_dir: PathBuf,
        }

        impl ToolPaths for DownloadedTools {
            fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
                let spec = ToolSpec::builtin(tool)
                    .ok_or_else(|| anyhow::anyhow!("no managed tool named `{tool}`"))?;
                let ctx = InstallContext {
                    label: tool,
                    command: "togi format",
                    verbose: false,
                };
                Downloader::new(ToolCache::at(&self.data_dir), Platform::current()?)
                    .ensure_installed(&spec, spec.default_version, &ctx)
            }
        }

        let root = tempfile::tempdir().expect("online test tempdir");
        let messy = root.path().join("messy.R");
        std::fs::write(&messy, "f<-function(x){x+1}\ny = c( 1,2 ,3 )\n").expect("write messy.R");
        let clean = root.path().join("clean.R");
        std::fs::write(&clean, "g <- function(y) {\n  y * 2\n}\n").expect("write clean.R");

        let provider = DownloadedTools {
            data_dir: root.path().join("data"),
        };
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);
        let files = vec![messy.clone(), clean.clone()];

        // Lint first: exactly the messy file, as a fixable warning.
        let diagnostics = AirAdapter.lint(&files, false, &ctx).expect("lint runs");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].path, messy);
        assert!(diagnostics[0].fixable);

        // Format in place: reports the messy file and actually rewrites it.
        let outcome = AirAdapter.format(&files, false, &ctx).expect("format runs");
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.changed, vec![messy.clone()]);
        let formatted = std::fs::read_to_string(&messy).expect("read formatted file");
        assert_eq!(
            formatted,
            "f <- function(x) {\n  x + 1\n}\ny <- c(1, 2, 3)\n"
        );

        // Now everything is clean: check mode and lint both agree.
        let recheck = AirAdapter.format(&files, true, &ctx).expect("recheck runs");
        assert!(recheck.changed.is_empty(), "{recheck:?}");
        let diagnostics = AirAdapter.lint(&files, false, &ctx).expect("relint runs");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
