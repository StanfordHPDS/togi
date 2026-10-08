use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context;
use serde::Deserialize;

use crate::adapters::{
    Diagnostic, Position, ProjectLint, ProjectLinter, ProjectScope, Range, Severity, ToolCtx,
    log_command,
};
use crate::term::HintExt;

const TOOL: &str = "deptry";
const ERROR_HINT: &str = "check the project's `[tool.deptry]` configuration and `[tools.deptry] args` in togi.toml, then rerun";
const REPORT_HINT: &str =
    "check the `[tools] deptry` version pin and `[tools.deptry] args` in togi.toml, then rerun";
const LAUNCHER: &str = include_str!("deptry_launcher.py");

#[derive(Debug, Clone, Copy)]
enum DeptryPlatform {
    Unix,
    Windows,
}

impl DeptryPlatform {
    fn current() -> DeptryPlatform {
        if cfg!(windows) {
            DeptryPlatform::Windows
        } else {
            DeptryPlatform::Unix
        }
    }
}

#[derive(Debug, Clone)]
struct DeptryEnv {
    virtual_env: Option<PathBuf>,
    platform: DeptryPlatform,
}

impl DeptryEnv {
    fn current() -> DeptryEnv {
        DeptryEnv {
            virtual_env: std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
            platform: DeptryPlatform::current(),
        }
    }

    fn environment(&self, root: &Path) -> Option<PathBuf> {
        self.virtual_env
            .as_ref()
            .filter(|path| path.is_dir())
            .cloned()
            .or_else(|| root.join(".venv").is_dir().then(|| root.join(".venv")))
    }

    fn site_packages_in(&self, environment: &Path) -> Option<PathBuf> {
        match self.platform {
            DeptryPlatform::Windows => environment
                .join("Lib/site-packages")
                .is_dir()
                .then(|| environment.join("Lib/site-packages")),
            DeptryPlatform::Unix => {
                let lib = environment.join("lib");
                let mut candidates = fs::read_dir(lib)
                    .ok()?
                    .filter_map(Result::ok)
                    .map(|entry| entry.path().join("site-packages"))
                    .filter(|path| {
                        path.is_dir()
                            && path
                                .parent()
                                .and_then(Path::file_name)
                                .is_some_and(|name| name.to_string_lossy().starts_with("python"))
                    })
                    .collect::<Vec<_>>();
                candidates.sort();
                candidates.into_iter().next()
            }
        }
    }

    #[cfg(test)]
    fn site_packages(&self) -> Option<PathBuf> {
        self.virtual_env
            .as_deref()
            .and_then(|environment| self.site_packages_in(environment))
    }
}

#[derive(Debug, Default)]
pub struct DeptryAdapter {
    env: Option<DeptryEnv>,
}

impl DeptryAdapter {
    pub fn new() -> DeptryAdapter {
        DeptryAdapter::default()
    }

    #[cfg(test)]
    fn with_env(env: DeptryEnv) -> DeptryAdapter {
        DeptryAdapter { env: Some(env) }
    }

    fn env(&self) -> DeptryEnv {
        self.env.clone().unwrap_or_else(DeptryEnv::current)
    }
}

impl ProjectLinter for DeptryAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn lint_project(&self, scope: &ProjectScope, ctx: &ToolCtx) -> anyhow::Result<ProjectLint> {
        if !ctx.config.python.dependencies || !has_manifest(&scope.root) {
            return Ok(ProjectLint::default());
        }
        let env = self.env();
        let Some(environment) = env.environment(&scope.root) else {
            return Ok(missing_environment());
        };
        let Some(site_packages) = env.site_packages_in(&environment) else {
            return Ok(missing_environment());
        };
        let site_packages = fs::canonicalize(&site_packages)
            .context("could not resolve the project environment's site-packages directory")
            .hint("check the project environment and rerun `uv sync`")?;

        let python = ctx.tool_python(TOOL)?;
        let launcher = write_launcher()?;
        let report = tempfile::Builder::new()
            .prefix("togi-deptry-")
            .suffix(".json")
            .tempfile()
            .context("could not create a temporary deptry report")
            .hint("check that the system temp directory is writable")?;
        let args = invocation_args(
            launcher.path(),
            &site_packages,
            report.path(),
            ctx.config
                .tools
                .args
                .get(TOOL)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        );
        log_command(ctx, &python, &args);
        let output = run(&python, &args, &scope.root)?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(anyhow::anyhow!(
                "deptry exited with status {}{}",
                output.status,
                stderr_suffix(&stderr)
            ))
            .hint(ERROR_HINT);
        }

        let json = fs::read_to_string(report.path())
            .context("deptry did not produce a usable JSON report")
            .with_context(|| stderr.clone())
            .hint(ERROR_HINT)?;
        let diagnostics = parse_output(&json, &scope.root).map_err(|error| {
            if stderr.is_empty() {
                error
            } else {
                error.context(stderr.clone())
            }
        })?;
        Ok(ProjectLint {
            diagnostics: filter_diagnostics(diagnostics, scope),
            notes: stderr_notes(&stderr),
        })
    }
}

fn has_manifest(root: &Path) -> bool {
    root.join("pyproject.toml").is_file() || root.join("requirements.txt").is_file()
}

fn missing_environment() -> ProjectLint {
    ProjectLint {
        diagnostics: Vec::new(),
        notes: vec!["skipped the Python dependency check: no project environment found; create one (for example `uv sync`) and rerun".to_string()],
    }
}

fn write_launcher() -> anyhow::Result<tempfile::NamedTempFile> {
    let file = tempfile::Builder::new()
        .prefix("togi-deptry-launcher-")
        .suffix(".py")
        .tempfile()
        .context("could not create the temporary deptry launcher")
        .hint("check that the system temp directory is writable")?;
    fs::write(file.path(), LAUNCHER)
        .context("could not write the temporary deptry launcher")
        .hint("check that the system temp directory is writable")?;
    Ok(file)
}

fn invocation_args(launcher: &Path, site: &Path, report: &Path, extra: &[String]) -> Vec<OsString> {
    let mut args = vec![
        "-I".into(),
        launcher.as_os_str().to_owned(),
        site.as_os_str().to_owned(),
        ".".into(),
        "--no-ansi".into(),
        "--json-output".into(),
        report.as_os_str().to_owned(),
    ];
    args.extend(extra.iter().map(OsString::from));
    args
}

fn run(python: &Path, args: &[OsString], root: &Path) -> anyhow::Result<Output> {
    crate::adapters::process::retry_etxtbsy(|| {
        Command::new(python).args(args).current_dir(root).output()
    })
    .with_context(|| format!("could not run deptry through `{}`", python.display()))
    .hint(ERROR_HINT)
}

fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(":\n{stderr}")
    }
}

fn stderr_notes(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_normal_chrome(line))
        .map(str::to_string)
        .collect()
}

fn is_normal_chrome(line: &str) -> bool {
    line.starts_with("Scanning ")
        || line.starts_with("Success! No dependency issues found")
        || line == "For more information, see the documentation: https://deptry.com/"
        || (line.starts_with("Found ")
            && (line.ends_with("dependency issues.") || line.ends_with("dependency violations.")))
        || line.contains(": DEP")
}

#[derive(Deserialize)]
struct DeptryFinding {
    error: DeptryError,
    location: DeptryLocation,
}

#[derive(Deserialize)]
struct DeptryError {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct DeptryLocation {
    file: PathBuf,
    line: Option<u32>,
    column: Option<u32>,
}

#[derive(Debug)]
struct ParsedDiagnostic {
    diagnostic: Diagnostic,
    declaration: bool,
}

fn parse_output(json: &str, root: &Path) -> anyhow::Result<Vec<ParsedDiagnostic>> {
    (|| {
        let findings: Vec<DeptryFinding> =
            serde_json::from_str(json).context("could not parse deptry's JSON report")?;
        Ok(findings
            .into_iter()
            .map(|finding| {
                let declaration = finding.location.line.is_none()
                    && matches!(finding.error.code.as_str(), "DEP002" | "DEP005");
                ParsedDiagnostic {
                    diagnostic: Diagnostic {
                        path: root.join(finding.location.file),
                        range: finding.location.line.zip(finding.location.column).map(
                            |(line, col)| Range {
                                start: Position { line, col },
                                end: None,
                            },
                        ),
                        code: Some(finding.error.code),
                        severity: Severity::Warning,
                        message: finding.error.message,
                        fixable: false,
                    },
                    declaration,
                }
            })
            .collect())
    })()
    .hint(REPORT_HINT)
}

fn filter_diagnostics(diagnostics: Vec<ParsedDiagnostic>, scope: &ProjectScope) -> Vec<Diagnostic> {
    let selected: HashSet<_> = scope.files.iter().map(|path| comparable(path)).collect();
    diagnostics
        .into_iter()
        .filter(|parsed| {
            if parsed.declaration {
                scope.whole_project
            } else {
                selected.contains(&comparable(&parsed.diagnostic.path))
            }
        })
        .map(|parsed| parsed.diagnostic)
        .collect()
}

fn comparable(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{DeptryAdapter, DeptryEnv, DeptryPlatform, filter_diagnostics, parse_output};
    use crate::adapters::test_support::FakeToolPaths;
    use crate::adapters::{Position, ProjectLinter, ProjectScope, Severity, ToolCtx};
    use crate::config::Config;

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/deptry")
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    fn scope(root: &Path, files: &[&str], whole_project: bool) -> ProjectScope {
        ProjectScope {
            root: root.to_path_buf(),
            files: files.iter().map(|file| root.join(file)).collect(),
            whole_project,
        }
    }

    fn env(virtual_env: Option<PathBuf>, platform: DeptryPlatform) -> DeptryEnv {
        DeptryEnv {
            virtual_env,
            platform,
        }
    }

    fn ctx<'a>(provider: &'a FakeToolPaths, config: &'a Config) -> ToolCtx<'a> {
        ToolCtx::new(provider, config, false)
    }

    #[test]
    fn parses_recorded_violations_and_null_positions() {
        let root = Path::new("/project");
        let diagnostics: Vec<_> = parse_output(&fixture("violations.json"), root)
            .expect("valid fixture")
            .into_iter()
            .map(|parsed| parsed.diagnostic)
            .collect();
        assert_eq!(diagnostics.len(), 4);
        assert_eq!(diagnostics[0].path, root.join("main.py"));
        assert_eq!(diagnostics[0].code.as_deref(), Some("DEP001"));
        assert_eq!(diagnostics[0].severity, Severity::Warning);
        assert_eq!(
            diagnostics[0].range.unwrap().start,
            Position { line: 1, col: 8 }
        );
        assert!(!diagnostics[0].fixable);
        assert_eq!(diagnostics[1].path, root.join("pyproject.toml"));
        assert_eq!(diagnostics[1].range, None);
        assert!(diagnostics[1].message.contains("defined as a dependency"));
    }

    #[test]
    fn parses_recorded_clean_result() {
        assert!(
            parse_output(&fixture("clean.json"), Path::new("/project"))
                .expect("valid fixture")
                .is_empty()
        );
    }

    #[test]
    fn malformed_json_has_pin_hint() {
        let error = parse_output("this is not json", Path::new("/project"))
            .expect_err("malformed output must fail");
        let rendered = crate::term::render_error(&error, false);
        assert!(rendered.contains("deptry"), "{rendered}");
        assert!(rendered.contains("[tools]"), "{rendered}");
    }

    #[test]
    fn default_registry_registers_deptry_for_python_projects() {
        let registry = crate::adapters::AdapterRegistry::with_defaults();
        let names: Vec<_> = registry
            .project_linters_for(crate::fsx::Language::Python)
            .iter()
            .map(|linter| linter.name())
            .collect();
        assert_eq!(names, ["deptry"]);
    }

    #[test]
    fn disabled_missing_manifest_and_missing_environment_do_not_resolve_tool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let provider = FakeToolPaths::default();
        let mut config = Config::default();
        config.python.dependencies = false;
        let adapter = DeptryAdapter::with_env(env(None, DeptryPlatform::Unix));
        assert_eq!(
            adapter
                .lint_project(&scope(root, &["main.py"], true), &ctx(&provider, &config))
                .unwrap(),
            Default::default()
        );

        config.python.dependencies = true;
        assert_eq!(
            adapter
                .lint_project(&scope(root, &["main.py"], true), &ctx(&provider, &config))
                .unwrap(),
            Default::default()
        );
        fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        let skipped = adapter
            .lint_project(&scope(root, &["main.py"], true), &ctx(&provider, &config))
            .unwrap();
        assert!(skipped.diagnostics.is_empty());
        assert_eq!(skipped.notes.len(), 1);
        assert!(skipped.notes[0].contains("uv sync"));
        assert!(provider.requests().is_empty());
        assert!(provider.python_requests().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn existing_virtual_env_wins_and_missing_virtual_env_falls_back_to_dot_venv() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        let active = root.join("active");
        let fallback = root.join(".venv");
        fs::create_dir_all(active.join("lib/python3.13/site-packages")).unwrap();
        fs::create_dir_all(fallback.join("lib/python3.12/site-packages")).unwrap();

        let (adapter, provider, record) =
            shim_adapter(root, Some(active.clone()), fixture("clean.json"));
        adapter
            .lint_project(
                &scope(root, &["main.py"], true),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert!(
            fs::read_to_string(record)
                .unwrap()
                .contains(&active.display().to_string())
        );

        let (adapter, provider, record) =
            shim_adapter(root, Some(root.join("absent")), fixture("clean.json"));
        adapter
            .lint_project(
                &scope(root, &["main.py"], true),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert!(
            fs::read_to_string(record)
                .unwrap()
                .contains(&fallback.display().to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_virtual_env_is_resolved_before_the_child_changes_directory() {
        let cwd = std::env::current_dir().unwrap();
        let environment = tempfile::tempdir_in(&cwd).unwrap();
        fs::create_dir_all(environment.path().join("lib/python3.13/site-packages")).unwrap();
        let relative = environment.path().strip_prefix(&cwd).unwrap().to_path_buf();
        let project = tempfile::tempdir().unwrap();
        setup_project(project.path());
        let (_, provider, record) = shim_adapter(project.path(), None, fixture("clean.json"));
        let adapter = DeptryAdapter::with_env(env(Some(relative), DeptryPlatform::Unix));
        adapter
            .lint_project(
                &scope(project.path(), &["main.py"], true),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        let call = fs::read_to_string(record).unwrap();
        assert!(
            call.contains(
                &fs::canonicalize(environment.path())
                    .unwrap()
                    .display()
                    .to_string()
            ),
            "{call}"
        );
    }

    #[test]
    fn site_package_layouts_cover_unix_and_windows() {
        let temp = tempfile::tempdir().unwrap();
        let unix = temp.path().join("unix");
        let windows = temp.path().join("windows");
        fs::create_dir_all(unix.join("lib/python3.14/site-packages")).unwrap();
        fs::create_dir_all(windows.join("Lib/site-packages")).unwrap();
        assert_eq!(
            env(Some(unix.clone()), DeptryPlatform::Unix)
                .site_packages()
                .unwrap(),
            unix.join("lib/python3.14/site-packages")
        );
        assert_eq!(
            env(Some(windows.clone()), DeptryPlatform::Windows)
                .site_packages()
                .unwrap(),
            windows.join("Lib/site-packages")
        );
    }

    #[cfg(unix)]
    #[test]
    fn scope_filters_sources_and_manifest_findings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        setup_project(root);
        let (adapter, provider, _) = shim_adapter(root, None, fixture("violations.json"));
        let all = adapter
            .lint_project(
                &scope(root, &["main.py"], true),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert_eq!(all.diagnostics.len(), 4);

        let selected = adapter
            .lint_project(
                &scope(root, &["main.py"], false),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert_eq!(selected.diagnostics.len(), 1);
        assert_eq!(selected.diagnostics[0].path, root.join("main.py"));
        let excluded = adapter
            .lint_project(
                &scope(root, &["other.py"], false),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert!(excluded.diagnostics.is_empty());
    }

    #[test]
    fn declaration_filter_uses_rule_semantics_instead_of_filenames() {
        let root = Path::new("/project");
        let report = r#"[
          {"error":{"code":"DEP002","message":"unused"},"location":{"file":"deps.in","line":null,"column":null}},
          {"error":{"code":"DEP001","message":"missing"},"location":{"file":"requirements_helper.py","line":3,"column":8}}
        ]"#;
        let parsed = parse_output(report, root).unwrap();
        let explicit = ProjectScope {
            root: root.to_path_buf(),
            files: vec![root.join("requirements_helper.py")],
            whole_project: false,
        };
        let diagnostics = filter_diagnostics(parsed, &explicit);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, root.join("requirements_helper.py"));

        let parsed = parse_output(report, root).unwrap();
        let whole = ProjectScope {
            root: root.to_path_buf(),
            files: Vec::new(),
            whole_project: true,
        };
        let diagnostics = filter_diagnostics(parsed, &whole);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, root.join("deps.in"));
    }

    #[cfg(unix)]
    #[test]
    fn matches_selected_files_across_cwd_dot_dot_and_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        setup_project(&root);
        fs::create_dir(root.join("sub")).unwrap();
        let link = temp.path().join("linked");
        symlink(&root, &link).unwrap();
        let selected = link.join("sub/../main.py");
        let (adapter, provider, _) = shim_adapter(&root, None, fixture("violations.json"));
        let result = adapter
            .lint_project(
                &ProjectScope {
                    root: root.clone(),
                    files: vec![selected],
                    whole_project: false,
                },
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert_eq!(result.diagnostics.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn invocation_uses_isolated_tool_python_launcher_root_and_appended_args() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        setup_project(root);
        let (adapter, provider, record) = shim_adapter(root, None, fixture("clean.json"));
        let mut config = Config::default();
        config
            .tools
            .args
            .insert("deptry".into(), vec!["--ignore".into(), "DEP002".into()]);
        adapter
            .lint_project(&scope(root, &["main.py"], true), &ctx(&provider, &config))
            .unwrap();
        let call = fs::read_to_string(record).unwrap();
        let recorded_cwd = call
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("cwd="))
            .expect("recorded cwd");
        assert_eq!(
            fs::canonicalize(recorded_cwd).unwrap(),
            fs::canonicalize(root).unwrap(),
            "{call}"
        );
        assert!(call.contains("arg=-I\n"), "{call}");
        assert!(call.contains("arg=.\n"), "{call}");
        assert!(call.contains("arg=--no-ansi\n"), "{call}");
        assert!(call.ends_with("arg=--ignore\narg=DEP002\n"), "{call}");
        assert_eq!(provider.python_requests(), vec!["deptry"]);
    }

    #[cfg(unix)]
    #[test]
    fn successful_stderr_is_returned_as_notes_and_stdout_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        setup_project(temp.path());
        let (adapter, provider, _) = shim_adapter_with_status(
            temp.path(),
            fixture("clean.json"),
            0,
            "Assuming module name\n",
            "not json on stdout\n",
        );
        let result = adapter
            .lint_project(
                &scope(temp.path(), &["main.py"], true),
                &ctx(&provider, &Config::default()),
            )
            .unwrap();
        assert_eq!(result.notes, vec!["Assuming module name"]);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn normal_deptry_chrome_is_suppressed_but_distribution_warnings_remain() {
        let stderr = "Scanning 2 files...\n\
                      main.py:1:8: DEP001 missing dependency\n\
                      Found 1 dependency issues.\n\
                      For more information, see the documentation: https://deptry.com/\n\
                      Assuming the corresponding module name of package 'PyYAML' is 'pyyaml'.\n";
        assert_eq!(
            super::stderr_notes(stderr),
            ["Assuming the corresponding module name of package 'PyYAML' is 'pyyaml'."]
        );
        assert!(super::stderr_notes("Success! No dependency issues found.").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn abnormal_exit_and_exit_one_without_json_retain_stderr_and_hint() {
        for status in [2, 1] {
            let temp = tempfile::tempdir().unwrap();
            setup_project(temp.path());
            let (adapter, provider, _) = shim_adapter_with_status(
                temp.path(),
                String::new(),
                status,
                "deptry exploded\n",
                "",
            );
            let error = adapter
                .lint_project(
                    &scope(temp.path(), &["main.py"], true),
                    &ctx(&provider, &Config::default()),
                )
                .expect_err("failure");
            let rendered = crate::term::render_error(&error, false);
            assert!(rendered.contains("deptry exploded"), "{rendered}");
            assert!(rendered.contains("[tools.deptry] args"), "{rendered}");
        }
    }

    fn setup_project(root: &Path) {
        fs::create_dir_all(root.join(".venv/lib/python3.13/site-packages")).unwrap();
        fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(root.join("main.py"), "import numpy\n").unwrap();
    }

    #[cfg(unix)]
    fn shim_adapter(
        root: &Path,
        active: Option<PathBuf>,
        json: String,
    ) -> (DeptryAdapter, FakeToolPaths, PathBuf) {
        shim_adapter_full(root, active, json, 0, "", "")
    }

    #[cfg(unix)]
    fn shim_adapter_with_status(
        root: &Path,
        json: String,
        status: i32,
        stderr: &str,
        stdout: &str,
    ) -> (DeptryAdapter, FakeToolPaths, PathBuf) {
        shim_adapter_full(root, None, json, status, stderr, stdout)
    }

    #[cfg(unix)]
    fn shim_adapter_full(
        root: &Path,
        active: Option<PathBuf>,
        json: String,
        status: i32,
        stderr: &str,
        stdout: &str,
    ) -> (DeptryAdapter, FakeToolPaths, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("tool-python");
        let record = root.join("deptry-call.txt");
        let payload = root.join("deptry-result.json");
        fs::write(&payload, json).unwrap();
        let body = format!(
            r#"#!/bin/sh
{{ printf 'cwd=%s\n' "$PWD"; for arg in "$@"; do printf 'arg=%s\n' "$arg"; done; }} > '{}'
out=''
previous=''
for arg in "$@"; do [ "$previous" = '--json-output' ] && out="$arg"; previous="$arg"; done
[ -n "$out" ] && [ -s '{}' ] && cp '{}' "$out"
printf '%s' '{}' >&2
printf '%s' '{}'
exit {}
"#,
            record.display(),
            payload.display(),
            payload.display(),
            stderr,
            stdout,
            status
        );
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let provider = FakeToolPaths::with_python("deptry", script.to_str().unwrap());
        let adapter = DeptryAdapter::with_env(env(active, DeptryPlatform::Unix));
        (adapter, provider, record)
    }
}

#[cfg(all(test, feature = "online-tests", unix))]
mod online_tests {
    use std::process::Command;

    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::config::Config;
    use crate::tools::{
        Downloader, InstallContext, Platform, ToolCache, ToolSpec, UvToolInstaller,
    };

    fn install_tool_python(cache: &Path) -> PathBuf {
        let platform = Platform::current().expect("supported platform");
        let spec = ToolSpec::builtin("deptry").expect("deptry spec");
        let context = InstallContext {
            label: "Python dependency checker",
            command: "togi lint",
            verbose: true,
        };
        UvToolInstaller::new(
            ToolCache::at(cache),
            platform,
            crate::tools::versions::UV.to_string(),
        )
        .ensure_python(&spec, spec.default_version, &context)
        .expect("install deptry")
    }

    fn install_uv(cache: &Path) -> PathBuf {
        let spec = ToolSpec::builtin("uv").expect("uv spec");
        Downloader::new(
            ToolCache::at(cache),
            Platform::current().expect("supported platform"),
        )
        .ensure_installed(
            &spec,
            spec.default_version,
            &InstallContext {
                label: "uv",
                command: "deptry online test",
                verbose: true,
            },
        )
        .expect("install uv")
    }

    fn write_distribution(
        site: &Path,
        distribution: &str,
        metadata: &str,
        top_level: Option<&str>,
        record: &str,
    ) {
        let info = site.join(format!("{distribution}.dist-info"));
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("METADATA"), metadata).unwrap();
        fs::write(info.join("RECORD"), record).unwrap();
        if let Some(top_level) = top_level {
            fs::write(info.join("top_level.txt"), top_level).unwrap();
        }
    }

    fn run_probe(
        python: &Path,
        root: &Path,
        site: &Path,
        manifest: &str,
        source: &str,
    ) -> ProjectLint {
        fs::create_dir_all(site).unwrap();
        fs::write(root.join("pyproject.toml"), manifest).unwrap();
        fs::write(root.join("main.py"), source).unwrap();
        let provider = FakeToolPaths::with_python(TOOL, python.to_str().expect("utf8 path"));
        DeptryAdapter::with_env(DeptryEnv {
            virtual_env: site
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            platform: DeptryPlatform::Unix,
        })
        .lint_project(
            &ProjectScope {
                root: root.to_path_buf(),
                files: vec![root.join("main.py")],
                whole_project: true,
            },
            &ToolCtx::new(&provider, &Config::default(), false),
        )
        .expect("run probe")
    }

    #[test]
    #[ignore = "downloads real uv, deptry, and PyYAML from the network"]
    fn real_deptry_uses_the_project_environment_without_false_dependency_findings() {
        let scratch = tempfile::tempdir().expect("scratch");
        let cache = scratch.path().join("cache");
        let project = scratch.path().join("project");
        let environment = project.join(".venv");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("requirements.txt"), "PyYAML==6.0.3\n").unwrap();
        fs::write(
            project.join("main.py"),
            "import yaml\nimport definitely_missing\n",
        )
        .unwrap();

        let uv = install_uv(&cache);
        let status = Command::new(&uv)
            .args(["venv", "--python", "3.13"])
            .arg(&environment)
            .status()
            .expect("create project venv");
        assert!(status.success());
        let status = Command::new(&uv)
            .args(["pip", "install", "--python"])
            .arg(environment.join("bin/python"))
            .arg("PyYAML==6.0.3")
            .status()
            .expect("install PyYAML");
        assert!(status.success());

        let python = install_tool_python(&cache);
        let provider = FakeToolPaths::with_python(TOOL, python.to_str().expect("utf8 path"));
        let config = Config::default();
        let result = DeptryAdapter::with_env(DeptryEnv {
            virtual_env: Some(environment),
            platform: DeptryPlatform::current(),
        })
        .lint_project(
            &ProjectScope {
                root: project.clone(),
                files: vec![project.join("main.py")],
                whole_project: true,
            },
            &ToolCtx::new(&provider, &config, false),
        )
        .expect("run real deptry");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|item| item.code.as_deref() == Some("DEP001")
                    && item.message.contains("definitely_missing")),
            "{:?}",
            result.diagnostics
        );
        assert!(
            result
                .diagnostics
                .iter()
                .all(|item| !item.message.contains("yaml") && !item.message.contains("PyYAML")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    #[ignore = "downloads real uv and deptry from the network"]
    fn real_launcher_preserves_project_isolation_and_foreign_abi_metadata() {
        let scratch = tempfile::tempdir().expect("scratch");
        let python = install_tool_python(&scratch.path().join("cache"));

        let shadow = scratch.path().join("shadow");
        let shadow_site = shadow.join(".venv/lib/python3.13/site-packages");
        fs::create_dir_all(&shadow).unwrap();
        fs::create_dir_all(&shadow_site).unwrap();
        fs::write(
            shadow_site.join("probe.pth"),
            "import pathlib; pathlib.Path('pth-ran').touch()\n",
        )
        .unwrap();
        let result = run_probe(
            &python,
            &shadow,
            &shadow_site,
            "[project]\nname='shadow'\nversion='0.1.0'\ndependencies=[]\n",
            "import click\nimport packaging\n",
        );
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|item| item.code.as_deref() == Some("DEP001"))
                .count(),
            2,
            "{:?}",
            result.diagnostics
        );
        assert!(!shadow.join("pth-ran").exists());

        let execution = scratch.path().join("execution");
        let execution_site = execution.join(".venv/lib/python3.13/site-packages");
        fs::create_dir_all(&execution).unwrap();
        fs::write(
            execution.join("evilmodule.py"),
            "import pathlib; pathlib.Path('module-ran').touch()\n",
        )
        .unwrap();
        fs::create_dir_all(&execution_site).unwrap();
        fs::write(
            execution_site.join("probe.pth"),
            "import pathlib; pathlib.Path('pth-ran').touch()\n",
        )
        .unwrap();
        let _ = run_probe(
            &python,
            &execution,
            &execution_site,
            "[project]\nname='execution'\nversion='0.1.0'\ndependencies=[]\n",
            "import evilmodule\n",
        );
        assert!(!execution.join("pth-ran").exists());
        assert!(!execution.join("module-ran").exists());

        for (name, top_level) in [
            ("foreign-top", Some("_cffi_backend\n")),
            ("foreign-record", None),
        ] {
            let root = scratch.path().join(name);
            let site = root.join(".venv/lib/python3.13/site-packages");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&site).unwrap();
            fs::write(site.join("_cffi_backend.cpython-313-darwin.so"), "").unwrap();
            write_distribution(
                &site,
                "cffi-2.0.0",
                "Metadata-Version: 2.4\nName: cffi\nVersion: 2.0.0\n",
                top_level,
                "_cffi_backend.cpython-313-darwin.so,,\n",
            );
            let result = run_probe(
                &python,
                &root,
                &site,
                "[project]\nname='foreign'\nversion='0.1.0'\ndependencies=[]\n",
                "import _cffi_backend\n",
            );
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|item| item.code.as_deref() == Some("DEP003")),
                "{name}: {:?}",
                result.diagnostics
            );
            assert!(
                result
                    .diagnostics
                    .iter()
                    .all(|item| item.code.as_deref() != Some("DEP001")),
                "{name}: {:?}",
                result.diagnostics
            );
        }

        let mapping = scratch.path().join("mapping");
        let mapping_site = mapping.join(".venv/lib/python3.13/site-packages");
        fs::create_dir_all(&mapping).unwrap();
        fs::create_dir_all(&mapping_site).unwrap();
        fs::create_dir_all(mapping_site.join("yaml")).unwrap();
        write_distribution(
            &mapping_site,
            "PyYAML-6.0.3",
            "Metadata-Version: 2.4\nName: PyYAML\nVersion: 6.0.3\n",
            Some("yaml\n"),
            "yaml/__init__.py,,\n",
        );
        let result = run_probe(
            &python,
            &mapping,
            &mapping_site,
            "[project]\nname='mapping'\nversion='0.1.0'\ndependencies=['PyYAML==6.0.3']\n",
            "import yaml\n",
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.notes.is_empty(), "{:?}", result.notes);

        let warning = scratch.path().join("warning");
        let warning_site = warning.join(".venv/lib/python3.13/site-packages");
        fs::create_dir_all(&warning).unwrap();
        let result = run_probe(
            &python,
            &warning,
            &warning_site,
            "[project]\nname='warning'\nversion='0.1.0'\ndependencies=['PyYAML==6.0.3']\n",
            "import yaml\n",
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("Assuming the corresponding module name")),
            "{:?}",
            result.notes
        );
    }
}
