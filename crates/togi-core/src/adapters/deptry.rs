#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{DeptryAdapter, DeptryEnv, DeptryPlatform, parse_output};
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
        let diagnostics = parse_output(&fixture("violations.json"), root).expect("valid fixture");
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
        let rendered = format!("{error:?}");
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
        assert!(
            call.contains(&format!("cwd={}\n", root.display())),
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
            let rendered = format!("{error:?}");
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
