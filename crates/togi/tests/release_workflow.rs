//! Structural checks on the release workflow and dist configuration.
//!
//! Releases are cut by pushing a `vX.Y.Z` tag, which triggers the
//! dist-generated workflow to build every supported target and publish
//! tarballs, installers, and a Homebrew formula. These tests keep the
//! generated workflow and the dist config honest without needing the
//! `dist` binary at test time.

use std::path::Path;

/// A path relative to the workspace root (two levels up from this crate).
fn repo_file(rel: &[&str]) -> std::path::PathBuf {
    let mut path = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    path.pop();
    path.pop();
    for part in rel {
        path.push(part);
    }
    path
}

fn read(rel: &[&str]) -> String {
    let path = repo_file(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

fn release_workflow() -> String {
    read(&[".github", "workflows", "release.yml"])
}

fn tool_update_workflow() -> String {
    read(&[".github", "workflows", "update-managed-tools.yml"])
}

fn tool_update_release_workflow() -> String {
    read(&[".github", "workflows", "release-managed-tools.yml"])
}

fn ci_workflow() -> String {
    read(&[".github", "workflows", "ci.yml"])
}

fn workflow(yml: &str) -> serde_yaml::Value {
    serde_yaml::from_str(yml).expect("valid workflow YAML")
}

fn field<'a>(value: &'a serde_yaml::Value, key: &str) -> &'a serde_yaml::Value {
    value
        .as_mapping()
        .and_then(|mapping| mapping.get(serde_yaml::Value::String(key.to_string())))
        .unwrap_or_else(|| panic!("missing workflow key {key}"))
}

fn assert_permissions(value: &serde_yaml::Value, expected: &[(&str, &str)]) {
    let permissions = field(value, "permissions")
        .as_mapping()
        .expect("permissions must be a mapping");
    assert_eq!(
        permissions.len(),
        expected.len(),
        "permissions must be scoped"
    );
    for (name, access) in expected {
        assert_eq!(
            permissions.get(serde_yaml::Value::String((*name).to_string())),
            Some(&serde_yaml::Value::String((*access).to_string())),
            "permission {name}"
        );
    }
}

fn dist_config() -> String {
    read(&["dist-workspace.toml"])
}

/// Every target triple the release must build for.
const RELEASE_TARGETS: [&str; 7] = [
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
];

#[test]
fn release_workflow_exists() {
    assert!(
        repo_file(&[".github", "workflows", "release.yml"]).is_file(),
        "expected .github/workflows/release.yml to exist"
    );
}

#[test]
fn release_workflow_parses_as_yaml() {
    let yml = release_workflow();
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&yml);
    assert!(
        parsed.is_ok(),
        "release.yml must be valid YAML: {:?}",
        parsed.err()
    );
}

#[test]
fn release_workflow_accepts_an_explicit_dispatch() {
    let parsed = workflow(&release_workflow());
    let triggers = field(&parsed, "on");
    let dispatch = field(triggers, "workflow_dispatch");
    let tag = field(field(dispatch, "inputs"), "tag");
    assert_eq!(field(tag, "required").as_bool(), Some(true));
    let trigger_names: Vec<&str> = triggers
        .as_mapping()
        .expect("trigger mapping")
        .keys()
        .filter_map(serde_yaml::Value::as_str)
        .collect();
    assert_eq!(trigger_names, ["workflow_dispatch"]);
}

#[test]
fn managed_tool_updates_run_monthly_or_manually_with_narrow_permissions() {
    let yml = tool_update_workflow();
    let parsed = workflow(&yml);
    let triggers = field(&parsed, "on");
    let schedules = field(triggers, "schedule")
        .as_sequence()
        .expect("schedule sequence");
    assert_eq!(schedules.len(), 1, "one monthly schedule");
    let cron = field(&schedules[0], "cron").as_str().expect("cron string");
    let fields: Vec<&str> = cron.split_whitespace().collect();
    assert_eq!(fields.len(), 5, "standard cron expression");
    let minute = fields[0].parse::<u8>().expect("numeric cron minute");
    let hour = fields[1].parse::<u8>().expect("numeric cron hour");
    assert!(
        minute > 0
            && minute < 60
            && hour < 24
            && fields[2] == "1"
            && fields[3] == "*"
            && fields[4] == "*",
        "schedule must run off the hour on the first day of each month: {cron}"
    );
    field(triggers, "workflow_dispatch");
    assert_permissions(&parsed, &[("contents", "read")]);
    let jobs = field(&parsed, "jobs").as_mapping().expect("jobs mapping");
    let discover = jobs
        .get(serde_yaml::Value::String("discover".to_string()))
        .expect("read-only discovery and validation job");
    let publish = jobs
        .get(serde_yaml::Value::String("publish".to_string()))
        .expect("PR publishing job");
    assert_permissions(discover, &[("contents", "read")]);
    assert_permissions(
        publish,
        &[
            ("actions", "write"),
            ("contents", "write"),
            ("pull-requests", "write"),
        ],
    );
    assert_eq!(field(publish, "needs").as_str(), Some("discover"));
    assert!(
        yml.contains("branch: automated/managed-tool-updates"),
        "a stable update branch must refresh the existing PR"
    );
    let concurrency = field(&parsed, "concurrency");
    assert!(
        field(concurrency, "group")
            .as_str()
            .is_some_and(|group| group.contains("managed-tool")),
        "monthly and manual runs must share one concurrency group"
    );
    assert!(
        yml.contains("cargo test --workspace --locked --features online-tests -- --ignored"),
        "the update PR must validate real managed-tool downloads"
    );
    assert!(
        yml.contains("path: ${{ runner.temp }}/managed-tool-update"),
        "the downloaded patch must stay outside the checkout"
    );
    assert!(
        yml.contains("gh workflow run ci.yml --ref automated/managed-tool-updates"),
        "GITHUB_TOKEN-created PRs must explicitly dispatch CI for their commit"
    );
    let ci = workflow(&ci_workflow());
    field(field(&ci, "on"), "workflow_dispatch");
}

#[test]
fn managed_tool_release_waits_for_successful_post_merge_ci() {
    let yml = tool_update_release_workflow();
    let parsed = workflow(&yml);
    let workflow_run = field(field(&parsed, "on"), "workflow_run");
    let workflows = field(workflow_run, "workflows")
        .as_sequence()
        .expect("workflow names");
    assert!(
        workflows.iter().any(|name| name.as_str() == Some("CI")),
        "release must wait for the main CI workflow"
    );
    assert!(
        field(workflow_run, "types")
            .as_sequence()
            .is_some_and(|types| types.iter().any(|kind| kind.as_str() == Some("completed"))),
        "release must wait for CI completion"
    );
    assert!(
        yml.contains("github.event.workflow_run.conclusion == 'success'"),
        "failed or cancelled CI must not release"
    );
    assert!(
        yml.contains("github.event.workflow_run.head_branch == 'main'"),
        "only post-merge main CI may release"
    );
    assert!(
        yml.contains("github.event.workflow_run.head_sha"),
        "the release decision and tag must use the exact CI commit"
    );
    assert!(
        yml.contains("merge_commit_sha"),
        "the associated merged PR must match the CI commit"
    );
    assert!(
        yml.contains("managed-tool-update"),
        "the workflow must verify that the merge came from the updater PR"
    );
    assert!(
        yml.contains("scripts/update-managed-tools.py"),
        "the workflow must use the tested release decision logic"
    );
    assert!(
        yml.contains("validate-pins --repo ."),
        "release must validate the committed pins rather than moving latest versions"
    );
    assert!(
        yml.contains("cargo test --workspace --locked --features online-tests -- --ignored"),
        "the exact merge commit must pass real managed-tool tests before release"
    );
    assert!(
        yml.contains("gh workflow run release.yml --ref \"$TAG\" -f \"tag=$TAG\""),
        "cargo-dist must run from and release the exact validated tag"
    );
    assert!(
        !yml.contains("uses: actions/checkout@v6\n        with:\n          ref: ${{ github.event.workflow_run.head_sha }}\n          fetch-depth: 0\n          persist-credentials: true"),
        "the write-capable publish job must not check out workflow-run code"
    );
    assert!(
        yml.contains("git/ref/tags/$TAG") && yml.contains("$EXISTING\" != \"$MERGE_SHA"),
        "a failed-job retry must accept only an existing tag at the exact merge commit"
    );
    assert_permissions(&parsed, &[("contents", "read")]);
    let jobs = field(&parsed, "jobs").as_mapping().expect("jobs mapping");
    let inspect = jobs
        .get(serde_yaml::Value::String("inspect".to_string()))
        .expect("read-only release inspection job");
    let publish = jobs
        .get(serde_yaml::Value::String("publish".to_string()))
        .expect("release publishing job");
    assert_permissions(inspect, &[("contents", "read"), ("pull-requests", "read")]);
    assert_permissions(publish, &[("actions", "write"), ("contents", "write")]);
    assert_eq!(field(publish, "needs").as_str(), Some("inspect"));
    assert_eq!(
        field(field(publish, "env"), "GH_REPO").as_str(),
        Some("${{ github.repository }}"),
        "gh must know the repository without a privileged checkout"
    );
}

#[test]
fn release_workflow_publishes_to_github_releases() {
    let yml = release_workflow();
    assert!(
        yml.contains("contents: write") || yml.contains("gh release"),
        "release workflow must be able to publish GitHub Releases"
    );
}

#[test]
fn dist_config_exists_and_parses_as_toml() {
    let toml_src = dist_config();
    let parsed: Result<toml::Value, _> = toml::from_str(&toml_src);
    assert!(
        parsed.is_ok(),
        "dist-workspace.toml must be valid TOML: {:?}",
        parsed.err()
    );
}

#[test]
fn dist_config_lists_all_release_targets() {
    let toml_src = dist_config();
    let parsed: toml::Value = toml::from_str(&toml_src).expect("valid TOML");
    let targets = parsed
        .get("dist")
        .and_then(|d| d.get("targets"))
        .and_then(|t| t.as_array())
        .expect("dist-workspace.toml must set [dist] targets");
    let targets: Vec<&str> = targets.iter().filter_map(|t| t.as_str()).collect();
    for expected in RELEASE_TARGETS {
        assert!(
            targets.contains(&expected),
            "dist targets must include {expected}, got {targets:?}"
        );
    }
}

#[test]
fn dist_config_declares_all_installers() {
    let toml_src = dist_config();
    let parsed: toml::Value = toml::from_str(&toml_src).expect("valid TOML");
    let installers = parsed
        .get("dist")
        .and_then(|d| d.get("installers"))
        .and_then(|i| i.as_array())
        .expect("dist-workspace.toml must set [dist] installers");
    let installers: Vec<&str> = installers.iter().filter_map(|i| i.as_str()).collect();
    for expected in ["shell", "powershell", "homebrew"] {
        assert!(
            installers.contains(&expected),
            "dist installers must include {expected}, got {installers:?}"
        );
    }
}

#[test]
fn dist_config_points_homebrew_at_stanford_tap() {
    let toml_src = dist_config();
    let parsed: toml::Value = toml::from_str(&toml_src).expect("valid TOML");
    let tap = parsed
        .get("dist")
        .and_then(|d| d.get("tap"))
        .and_then(|t| t.as_str())
        .expect("dist-workspace.toml must set [dist] tap");
    assert_eq!(tap, "StanfordHPDS/homebrew-tap");
    let publish_jobs = parsed
        .get("dist")
        .and_then(|d| d.get("publish-jobs"))
        .and_then(|p| p.as_array())
        .expect("dist-workspace.toml must set [dist] publish-jobs");
    assert!(
        publish_jobs.iter().any(|j| j.as_str() == Some("homebrew")),
        "publish-jobs must include homebrew so the formula is pushed to the tap"
    );
}

#[test]
fn dist_config_skips_pull_request_runs() {
    let toml_src = dist_config();
    let parsed: toml::Value = toml::from_str(&toml_src).expect("valid TOML");
    let mode = parsed
        .get("dist")
        .and_then(|d| d.get("pr-run-mode"))
        .and_then(|m| m.as_str())
        .expect("dist-workspace.toml must set [dist] pr-run-mode");
    assert_eq!(
        mode, "skip",
        "release plumbing must skip pull requests; ci.yml owns PR checks"
    );
    let dispatch = parsed
        .get("dist")
        .and_then(|d| d.get("dispatch-releases"))
        .and_then(|value| value.as_bool());
    assert_eq!(
        dispatch,
        Some(true),
        "releases must support exact-tag dispatch"
    );
}

/// The shell installer artifact dist publishes is named after the package:
/// `<package>-installer.sh`. Anything that points users at the installer
/// (README one-liners, docs) hardcodes `togi-installer.sh`, so a crate
/// rename would silently change the published artifact out from under them.
#[test]
fn shell_installer_artifact_is_derived_from_the_crate_name() {
    let toml_src = dist_config();
    let parsed: toml::Value = toml::from_str(&toml_src).expect("valid TOML");
    let installers = parsed
        .get("dist")
        .and_then(|d| d.get("installers"))
        .and_then(|i| i.as_array())
        .expect("dist-workspace.toml declares [dist] installers");
    assert!(
        installers.iter().any(|i| i.as_str() == Some("shell")),
        "the shell installer must be enabled to publish {}",
        installer_artifact()
    );
    assert_eq!(
        installer_artifact(),
        "togi-installer.sh",
        "the crate name determines the published installer artifact; renaming \
         the crate renames the artifact users download"
    );
}

/// The name of the shell installer artifact dist publishes for this crate:
/// `<package>-installer.sh`.
fn installer_artifact() -> String {
    format!("{}-installer.sh", env!("CARGO_PKG_NAME"))
}

#[test]
fn ci_workflow_runs_dist_plan_as_allowed_to_fail_job() {
    let yml = read(&[".github", "workflows", "ci.yml"]);
    let job_start = yml
        .find("dist-plan:")
        .expect("ci.yml must have a dist-plan job that checks the dist config");
    let job = &yml[job_start..];
    assert!(
        job.contains("continue-on-error: true"),
        "dist-plan job must be allowed to fail"
    );
    assert!(
        job.contains("dist plan"),
        "dist-plan job must run `dist plan`"
    );
}
