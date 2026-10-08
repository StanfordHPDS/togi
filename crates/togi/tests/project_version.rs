//! Project version selection and dispatch at the executable boundary.

use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use predicates::prelude::*;

const SELECTED_VERSION: &str = "7.8.9";

struct Sandbox {
    _temp: tempfile::TempDir,
    project: PathBuf,
    data: PathBuf,
    config: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create sandbox");
        let project = temp.path().join("project");
        let data = temp.path().join("data");
        let config = temp.path().join("config");
        fs::create_dir_all(project.join(".git")).expect("create repository marker");
        fs::create_dir_all(&config).expect("create config directory");
        Self {
            _temp: temp,
            project,
            data,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("togi").expect("togi binary");
        command
            .current_dir(&self.project)
            .env("TOGI_DATA_DIR", &self.data)
            .env("TOGI_CONFIG_DIR", &self.config)
            .env("TOGI_RELEASE_BASE_URL", "http://127.0.0.1:9")
            .env("TOGI_GITHUB_API_BASE_URL", "http://127.0.0.1:9");
        command
    }

    fn pin(&self, value: &str) {
        fs::write(self.project.join(".togi-version"), value).expect("write version lock");
    }

    fn install_probe(&self, version: &str) -> PathBuf {
        let install = self.data.join("versions").join("togi").join(version);
        fs::create_dir_all(&install).expect("create version cache");
        let source = self._temp.path().join(format!("probe-{version}.rs"));
        fs::write(
            &source,
            r#"
use std::env;
use std::fs;
use std::io::{self, Read};
use std::process::ExitCode;

fn main() -> ExitCode {
    let record = env::var_os("TOGI_PROBE_RECORD").expect("record path");
    let args = env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let value = env::var("TOGI_PROBE_VALUE").expect("forwarded environment");
    let cwd = env::current_dir().expect("cwd");
    let mut stdin = String::new();
    io::stdin().read_to_string(&mut stdin).expect("read stdin");
    fs::write(record, format!("version={}\ncwd={}\nenv={}\nstdin={stdin:?}\nargs={args:?}\n", env!("TOGI_PROBE_VERSION"), cwd.display(), value))
        .expect("write record");
    print!("probe stdout");
    eprint!("probe stderr");
    let code = env::var("TOGI_PROBE_EXIT")
        .expect("exit code")
        .parse::<u8>()
        .expect("numeric exit code");
    ExitCode::from(code)
}
"#,
        )
        .expect("write probe source");
        let binary = install.join(binary_name());
        let status = ProcessCommand::new("rustc")
            .args(["--edition=2024", "--crate-name", "togi_version_probe", "-o"])
            .arg(&binary)
            .arg(&source)
            .env("TOGI_PROBE_VERSION", version)
            .status()
            .expect("run rustc");
        assert!(status.success(), "compile probe");
        fs::write(
            install.join("manifest.json"),
            format!(
                "{{\"version\":\"{version}\",\"source_url\":\"https://example.test/togi\",\"checksum\":\"fixture\",\"installed_at\":\"2026-10-08T00:00:00Z\"}}"
            ),
        )
        .expect("write manifest");
        binary
    }
}

fn binary_name() -> &'static str {
    if cfg!(windows) { "togi.exe" } else { "togi" }
}

#[test]
fn pin_from_nested_directory_writes_exact_current_version_only_at_project_root() {
    let sandbox = Sandbox::new();
    let nested = sandbox.project.join("src").join("nested");
    let workflow = sandbox.project.join(".github/workflows/ci.yml");
    fs::create_dir_all(&nested).expect("create nested directory");
    fs::create_dir_all(workflow.parent().unwrap()).expect("create workflows directory");
    fs::write(&workflow, "name: ci\n").expect("write workflow");

    sandbox
        .command()
        .current_dir(&nested)
        .arg("pin")
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(sandbox.project.join(".togi-version")).expect("read lock"),
        format!("{}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(!nested.join(".togi-version").exists());
    assert_eq!(fs::read_to_string(workflow).unwrap(), "name: ci\n");
}

#[test]
fn unpin_removes_only_the_lock_and_is_idempotent() {
    let sandbox = Sandbox::new();
    sandbox.pin("1.2.3\n");
    let sentinel = sandbox.project.join("keep.txt");
    fs::write(&sentinel, "keep").unwrap();

    sandbox.command().arg("unpin").assert().success();
    sandbox.command().arg("unpin").assert().success();

    assert!(!sandbox.project.join(".togi-version").exists());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[test]
fn explicit_pin_normalizes_a_leading_v_and_repairs_a_malformed_lock() {
    let sandbox = Sandbox::new();
    sandbox.install_probe(SELECTED_VERSION);
    sandbox.pin("malformed\n");

    sandbox.command().args(["pin", "v7.8.9"]).assert().success();

    assert_eq!(
        fs::read_to_string(sandbox.project.join(".togi-version")).unwrap(),
        "7.8.9\n"
    );
}

#[test]
fn unpin_bypasses_a_malformed_selection() {
    let sandbox = Sandbox::new();
    sandbox.pin("malformed\n");

    sandbox.command().arg("unpin").assert().success();

    assert!(!sandbox.project.join(".togi-version").exists());
}

#[test]
fn invalid_lock_content_is_an_actionable_usage_error() {
    for value in ["", "latest\n", "1.2\n", "not-a-version\n"] {
        let sandbox = Sandbox::new();
        sandbox.pin(value);
        sandbox
            .command()
            .arg("version")
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(
                predicate::str::contains(".togi-version").and(predicate::str::contains("hint:")),
            );
    }
}

#[test]
fn leading_v_is_normalized_when_selecting_a_cached_version() {
    let sandbox = Sandbox::new();
    sandbox.install_probe(SELECTED_VERSION);
    sandbox.pin(&format!("v{SELECTED_VERSION}\n"));
    let record = sandbox._temp.path().join("record");

    sandbox
        .command()
        .env("TOGI_PROBE_RECORD", &record)
        .env("TOGI_PROBE_VALUE", "present")
        .env("TOGI_PROBE_EXIT", "0")
        .arg("version")
        .assert()
        .success()
        .stdout("probe stdout")
        .stderr("probe stderr");

    assert!(record.is_file(), "normalized cache entry was dispatched");
}

#[test]
fn nearest_lock_wins_for_a_nested_invocation() {
    let sandbox = Sandbox::new();
    sandbox.install_probe("1.2.3");
    sandbox.install_probe("4.5.6");
    sandbox.pin("1.2.3\n");
    let nested = sandbox.project.join("packages").join("analysis");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join(".togi-version"), "4.5.6\n").unwrap();
    let record = sandbox._temp.path().join("nested-record");

    sandbox
        .command()
        .current_dir(&nested)
        .env("TOGI_PROBE_RECORD", &record)
        .env("TOGI_PROBE_VALUE", "present")
        .env("TOGI_PROBE_EXIT", "0")
        .arg("version")
        .assert()
        .success();

    assert!(
        fs::read_to_string(record)
            .unwrap()
            .contains("version=4.5.6"),
        "the closest lock should select the executable"
    );
}

#[test]
fn git_directory_and_file_stop_lock_discovery() {
    for git_is_file in [false, true] {
        let sandbox = Sandbox::new();
        fs::write(sandbox._temp.path().join(".togi-version"), "99.98.97\n").unwrap();
        if git_is_file {
            fs::remove_dir(sandbox.project.join(".git")).unwrap();
            fs::write(sandbox.project.join(".git"), "gitdir: elsewhere\n").unwrap();
        }

        sandbox
            .command()
            .arg("version")
            .assert()
            .success()
            .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
    }
}

#[test]
fn lock_discovery_is_independent_of_explicit_config_location() {
    let sandbox = Sandbox::new();
    sandbox.install_probe(SELECTED_VERSION);
    sandbox.pin(&format!("{SELECTED_VERSION}\n"));
    let elsewhere = sandbox._temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let config = elsewhere.join("togi.toml");
    fs::write(&config, "").unwrap();
    let record = sandbox._temp.path().join("config-record");

    sandbox
        .command()
        .env("TOGI_PROBE_RECORD", &record)
        .env("TOGI_PROBE_VALUE", "present")
        .env("TOGI_PROBE_EXIT", "0")
        .arg("--config")
        .arg(config)
        .arg("version")
        .assert()
        .success();

    assert!(
        fs::read_to_string(record)
            .unwrap()
            .contains("version=7.8.9")
    );
}

#[test]
fn exact_override_precedes_the_lock_and_applies_before_clap_version_handling() {
    let sandbox = Sandbox::new();
    sandbox.pin("1.2.3\n");
    sandbox.install_probe(SELECTED_VERSION);
    let record = sandbox._temp.path().join("record");

    sandbox
        .command()
        .env("TOGI_PROBE_RECORD", &record)
        .env("TOGI_PROBE_VALUE", "present")
        .env("TOGI_PROBE_EXIT", "0")
        .args(["--with-version", SELECTED_VERSION, "--version"])
        .assert()
        .success()
        .stdout("probe stdout");

    let recorded = fs::read_to_string(record).expect("read probe record");
    assert!(recorded.contains("\"--version\""), "{recorded}");
    assert!(!recorded.contains("\"--with-version\""), "{recorded}");
    assert!(
        !recorded.contains(&format!("\"{SELECTED_VERSION}\"")),
        "{recorded}"
    );
}

#[test]
fn cached_dispatch_preserves_process_inputs_and_outputs() {
    let sandbox = Sandbox::new();
    sandbox.install_probe(SELECTED_VERSION);
    sandbox.pin(&format!("{SELECTED_VERSION}\n"));
    let nested = sandbox.project.join("nested directory");
    fs::create_dir_all(&nested).unwrap();
    let record = sandbox._temp.path().join("record");

    let output = sandbox
        .command()
        .current_dir(&nested)
        .env("TOGI_PROBE_RECORD", &record)
        .env("TOGI_PROBE_VALUE", "forwarded value")
        .env("TOGI_PROBE_EXIT", "37")
        .write_stdin("forwarded stdin")
        .args(["lint", "--", "path with spaces.py", "", "--with-version"])
        .assert()
        .code(37)
        .stdout("probe stdout")
        .stderr("probe stderr");
    drop(output);

    let recorded = fs::read_to_string(record).expect("read probe record");
    assert!(
        recorded.contains(&format!("cwd={}", nested.display())),
        "{recorded}"
    );
    assert!(recorded.contains("env=forwarded value"), "{recorded}");
    assert!(recorded.contains("stdin=\"forwarded stdin\""), "{recorded}");
    assert!(
        recorded
            .contains("args=[\"lint\", \"--\", \"path with spaces.py\", \"\", \"--with-version\"]"),
        "{recorded}"
    );
}

#[test]
fn two_projects_dispatch_distinct_versions_from_one_data_directory() {
    let sandbox = Sandbox::new();
    let other = sandbox._temp.path().join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    sandbox.install_probe("1.2.3");
    sandbox.install_probe("4.5.6");
    sandbox.pin("1.2.3\n");
    fs::write(other.join(".togi-version"), "4.5.6\n").unwrap();

    for (project, version) in [(&sandbox.project, "1.2.3"), (&other, "4.5.6")] {
        let record = sandbox._temp.path().join(format!("record-{version}"));
        sandbox
            .command()
            .current_dir(project)
            .env("TOGI_PROBE_RECORD", &record)
            .env("TOGI_PROBE_VALUE", "shared")
            .env("TOGI_PROBE_EXIT", "0")
            .arg("version")
            .assert()
            .success();
        assert!(
            fs::read_to_string(record)
                .unwrap()
                .contains(&format!("version={version}")),
            "project should use its selected cached version"
        );
    }
}

#[test]
fn unavailable_exact_version_never_falls_back_to_the_invoking_binary() {
    let sandbox = Sandbox::new();
    sandbox.pin("99.98.97\n");

    sandbox
        .command()
        .arg("version")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("99.98.97").and(predicate::str::contains("hint:")));
}

#[test]
fn unpinned_version_succeeds_without_release_network_access() {
    let sandbox = Sandbox::new();

    sandbox
        .command()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn selecting_the_running_version_short_circuits_without_a_cached_runtime() {
    let sandbox = Sandbox::new();

    sandbox
        .command()
        .args(["--with-version", env!("CARGO_PKG_VERSION"), "--version"])
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));

    assert!(!sandbox.data.join("versions").exists());
}

#[test]
fn override_is_global_but_text_after_double_dash_is_forwarded_unchanged() {
    let sandbox = Sandbox::new();
    sandbox.install_probe(SELECTED_VERSION);
    let record = sandbox._temp.path().join("record");

    sandbox
        .command()
        .env("TOGI_PROBE_RECORD", &record)
        .env("TOGI_PROBE_VALUE", "present")
        .env("TOGI_PROBE_EXIT", "0")
        .args([
            "lint",
            "--with-version",
            SELECTED_VERSION,
            "--",
            "--with-version",
            "literal.py",
        ])
        .assert()
        .success();

    let recorded = fs::read_to_string(record).unwrap();
    assert!(
        recorded.contains("args=[\"lint\", \"--\", \"--with-version\", \"literal.py\"]"),
        "{recorded}"
    );
}
