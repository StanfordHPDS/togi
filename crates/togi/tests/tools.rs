//! Integration tests for `togi tools list|update|clean` against a fake
//! tool cache: a `TOGI_DATA_DIR` tempdir populated with hand-built
//! manifest layouts. No test here touches the real network — installs are
//! served by a local fixture HTTP server via the internal
//! `TOGI_RELEASE_BASE_URL` override, and offline behavior is simulated by
//! pointing that override at a closed port.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use predicates::prelude::*;

/// Baked default versions (mirrors `src/tools/versions.rs`; drift fails
/// the assertions below loudly).
const AIR_DEFAULT: &str = "0.10.0";
const RUFF_DEFAULT: &str = "0.14.0";
const PANACHE_DEFAULT: &str = "2.60.0";
const SQLFLUFF_DEFAULT: &str = "3.4.0";
const UV_DEFAULT: &str = "0.9.5";

/// A throwaway project dir, isolated user-config dir, and fake tool cache
/// data dir.
struct Sandbox {
    _root: tempfile::TempDir,
    project: PathBuf,
    user_dir: PathBuf,
    data_dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox tempdir");
        let project = root.path().join("project");
        let user_dir = root.path().join("user-config");
        let data_dir = root.path().join("data");
        // `.git` marker stops config discovery from walking out of the
        // sandbox into some real togi.toml.
        fs::create_dir_all(project.join(".git")).expect("create project/.git");
        fs::create_dir_all(&user_dir).expect("create user config dir");
        Sandbox {
            _root: root,
            project,
            user_dir,
            data_dir,
        }
    }

    fn write_project_config(&self, contents: &str) {
        fs::write(self.project.join("togi.toml"), contents).expect("write togi.toml");
    }

    /// The platform binary filename for a tool.
    fn binary_name(name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    }

    /// Hand-build one installed tool in the fake cache: binary plus
    /// `manifest.json`, exactly as a completed install lays them out.
    fn install_fake_tool(&self, name: &str, version: &str, installed_at: &str) {
        let dir = self.data_dir.join("tools").join(name).join(version);
        fs::create_dir_all(&dir).expect("create fake tool dir");
        fs::write(dir.join(Self::binary_name(name)), b"#!/bin/sh\nexit 0\n")
            .expect("write fake binary");
        let manifest = format!(
            r#"{{
  "version": "{version}",
  "source_url": "https://example.test/{name}-{version}.tar.gz",
  "installed_at": "{installed_at}"
}}"#
        );
        fs::write(dir.join("manifest.json"), manifest).expect("write fake manifest");
    }

    /// Every managed tool pre-installed at its baked default version.
    fn install_all_defaults(&self) {
        for (name, version) in [
            ("air", AIR_DEFAULT),
            ("ruff", RUFF_DEFAULT),
            ("panache", PANACHE_DEFAULT),
            ("sqlfluff", SQLFLUFF_DEFAULT),
            ("uv", UV_DEFAULT),
        ] {
            self.install_fake_tool(name, version, "2026-07-01T09:00:00Z");
        }
    }

    /// `togi tools <args...>` in the sandbox, network pointed at
    /// `base_url` (a closed port unless a test serves fixtures).
    fn tools_cmd(&self, args: &[&str], base_url: &str) -> Command {
        let mut cmd = Command::cargo_bin("togi").expect("togi binary should build");
        cmd.current_dir(&self.project)
            .env("TOGI_CONFIG_DIR", &self.user_dir)
            .env("TOGI_DATA_DIR", &self.data_dir)
            .env("TOGI_RELEASE_BASE_URL", base_url)
            .arg("tools")
            .args(args);
        cmd
    }
}

/// A `http://127.0.0.1:<port>` URL nothing listens on: any request fails
/// with connection refused, like a machine with no network.
fn dead_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// ---------------------------------------------------------------------------
// list

#[test]
fn list_shows_installed_tools_with_version_kind_and_date() {
    let sb = Sandbox::new();
    sb.install_fake_tool("air", "0.10.0", "2026-06-15T08:30:00Z");
    sb.install_fake_tool("sqlfluff", "3.4.0", "2026-06-16T10:00:00Z");

    sb.tools_cmd(&["list"], &dead_url())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("air")
                .and(predicate::str::contains("0.10.0"))
                .and(predicate::str::contains("github release"))
                .and(predicate::str::contains("2026-06-15"))
                .and(predicate::str::contains("sqlfluff"))
                .and(predicate::str::contains("uv (PyPI)"))
                .and(predicate::str::contains("2026-06-16")),
        );
}

#[test]
fn list_shows_baked_defaults_for_tools_not_installed() {
    let sb = Sandbox::new();
    sb.install_fake_tool("air", "0.10.0", "2026-06-15T08:30:00Z");

    sb.tools_cmd(&["list"], &dead_url())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("panache")
                .and(predicate::str::contains("not installed"))
                .and(predicate::str::contains(PANACHE_DEFAULT))
                .and(predicate::str::contains(RUFF_DEFAULT))
                .and(predicate::str::contains(SQLFLUFF_DEFAULT))
                .and(predicate::str::contains(UV_DEFAULT)),
        );
}

#[test]
fn list_with_empty_cache_names_every_managed_tool() {
    let sb = Sandbox::new();
    sb.tools_cmd(&["list"], &dead_url())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("air")
                .and(predicate::str::contains("ruff"))
                .and(predicate::str::contains("panache"))
                .and(predicate::str::contains("sqlfluff"))
                .and(predicate::str::contains("uv")),
        )
        .stderr(predicate::str::is_empty());
}

#[test]
fn list_shows_every_installed_version_of_a_tool() {
    let sb = Sandbox::new();
    sb.install_fake_tool("air", "0.9.0", "2026-05-01T08:00:00Z");
    sb.install_fake_tool("air", "0.10.0", "2026-06-15T08:30:00Z");

    sb.tools_cmd(&["list"], &dead_url())
        .assert()
        .success()
        .stdout(predicate::str::contains("0.9.0").and(predicate::str::contains("0.10.0")));
}

// ---------------------------------------------------------------------------
// update

#[test]
fn update_reports_up_to_date_for_cached_tools_without_network() {
    let sb = Sandbox::new();
    sb.install_all_defaults();

    // The dead base URL proves no network is needed when everything is
    // cached at its wanted version.
    sb.tools_cmd(&["update"], &dead_url())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("air")
                .and(predicate::str::contains("ruff"))
                .and(predicate::str::contains("panache"))
                .and(predicate::str::contains("sqlfluff"))
                .and(predicate::str::contains("uv"))
                .and(predicate::str::contains("up to date")),
        );
}

#[test]
fn update_installs_a_pinned_version_and_reports_the_transition() {
    let sb = Sandbox::new();
    sb.install_all_defaults();
    sb.write_project_config("[tools]\nair = \"0.11.0\"\n");

    // Serve air 0.11.0 for every platform tuple so the test passes on any
    // host. No checksum assets: the installer warns and proceeds.
    let server = FixtureServer::serve(air_release_routes("0.11.0"));

    sb.tools_cmd(&["update"], &server.base_url)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("air")
                .and(predicate::str::contains("0.10.0 -> 0.11.0"))
                .and(predicate::str::contains("up to date")),
        );

    // The pinned version really landed in the cache.
    assert!(
        sb.data_dir
            .join("tools")
            .join("air")
            .join("0.11.0")
            .join("manifest.json")
            .is_file(),
        "pinned air version must be installed into the cache"
    );
}

#[test]
fn update_installs_missing_tools_from_scratch() {
    let sb = Sandbox::new();
    // Everything cached except air.
    for (name, version) in [
        ("ruff", RUFF_DEFAULT),
        ("panache", PANACHE_DEFAULT),
        ("sqlfluff", SQLFLUFF_DEFAULT),
        ("uv", UV_DEFAULT),
    ] {
        sb.install_fake_tool(name, version, "2026-07-01T09:00:00Z");
    }
    let server = FixtureServer::serve(air_release_routes(AIR_DEFAULT));

    sb.tools_cmd(&["update"], &server.base_url)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("air")
                .and(predicate::str::contains(format!("installed {AIR_DEFAULT}"))),
        );
}

#[test]
fn update_offline_fails_per_tool_and_continues_to_the_next() {
    let sb = Sandbox::new();
    // air cached at its wanted version; everything else missing.
    sb.install_fake_tool("air", AIR_DEFAULT, "2026-07-01T09:00:00Z");

    sb.tools_cmd(&["update"], &dead_url())
        .assert()
        .failure()
        .code(1)
        // The cached tool still reports before and after the failures:
        // one dead tool must not abort the whole run.
        .stdout(predicate::str::contains("air").and(predicate::str::contains("up to date")))
        .stderr(
            predicate::str::contains("ruff")
                .and(predicate::str::contains("panache"))
                .and(predicate::str::contains("sqlfluff"))
                .and(predicate::str::contains("togi tools update"))
                .and(predicate::str::contains("hint:")),
        );
}

#[test]
fn update_honors_a_uv_pin_from_config() {
    let sb = Sandbox::new();
    sb.install_all_defaults();
    sb.write_project_config("[tools]\nuv = \"0.9.9\"\n");

    let server = FixtureServer::serve(release_routes("astral-sh/uv", "uv", "0.9.9"));

    sb.tools_cmd(&["update"], &server.base_url)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("{UV_DEFAULT} -> 0.9.9")));

    assert!(
        sb.data_dir
            .join("tools")
            .join("uv")
            .join("0.9.9")
            .join("manifest.json")
            .is_file(),
        "pinned uv version must be installed into the cache"
    );
}

#[test]
fn update_announces_downloads_on_non_tty_stderr_labels_only() {
    // Progress bars cannot render on a piped stderr; a download must still
    // announce itself with the one-line "Fetching …" notice. Without -v the
    // notice carries the friendly label only, never the tool name.
    let sb = Sandbox::new();
    sb.install_all_defaults();
    sb.write_project_config("[tools]\nair = \"0.11.0\"\n");
    let server = FixtureServer::serve(air_release_routes("0.11.0"));

    sb.tools_cmd(&["update"], &server.base_url)
        .assert()
        .success()
        .stderr(
            predicate::str::contains("Fetching R formatter…")
                .and(predicate::str::contains("(air 0.11.0)").not()),
        );
}

#[test]
fn update_verbose_download_notice_names_the_tool() {
    let sb = Sandbox::new();
    sb.install_all_defaults();
    sb.write_project_config("[tools]\nair = \"0.11.0\"\n");
    let server = FixtureServer::serve(air_release_routes("0.11.0"));

    sb.tools_cmd(&["update", "--verbose"], &server.base_url)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Fetching R formatter (air 0.11.0)…",
        ));
}

// ---------------------------------------------------------------------------
// clean

#[test]
fn clean_with_yes_deletes_the_cache_and_reports_bytes_freed() {
    let sb = Sandbox::new();
    sb.install_fake_tool("air", "0.10.0", "2026-06-15T08:30:00Z");
    let cache_root = sb.data_dir.join("tools");
    assert!(cache_root.is_dir());

    sb.tools_cmd(&["clean", "--yes"], &dead_url())
        .assert()
        .success()
        .stdout(predicate::str::contains("freed").and(predicate::str::contains("B")));

    assert!(!cache_root.exists(), "the tool cache must be deleted");
}

#[test]
fn clean_without_yes_refuses_when_non_interactive() {
    let sb = Sandbox::new();
    sb.install_fake_tool("air", "0.10.0", "2026-06-15T08:30:00Z");

    // stdin is not a TTY under assert_cmd, so the confirm prompt must fail
    // with guidance that names the exact flag instead of hanging.
    sb.tools_cmd(&["clean"], &dead_url())
        .assert()
        .failure()
        .stderr(predicate::str::contains("hint:").and(predicate::str::contains("--yes")));

    assert!(
        sb.data_dir.join("tools").is_dir(),
        "a refused prompt must not delete anything"
    );
}

#[test]
fn clean_with_no_cache_reports_nothing_to_clean() {
    let sb = Sandbox::new();
    sb.tools_cmd(&["clean", "--yes"], &dead_url())
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to clean"));
}

// ---------------------------------------------------------------------------
// fixture plumbing

/// Release-asset routes (tar.gz and zip, no checksums) for `tool` at
/// `version` under `repo`, for every supported platform tuple, so the
/// test binary works on whatever host runs it.
fn release_routes(repo: &str, tool: &str, version: &str) -> HashMap<String, Vec<u8>> {
    let targz = targz_with(tool, b"#!/bin/sh\nexit 0\n");
    let zip = zip_with(&format!("{tool}.exe"), b"fake exe");
    let mut routes = HashMap::new();
    for (arch, os, ext) in [
        ("x86_64", "apple-darwin", "tar.gz"),
        ("aarch64", "apple-darwin", "tar.gz"),
        ("x86_64", "unknown-linux-gnu", "tar.gz"),
        ("aarch64", "unknown-linux-gnu", "tar.gz"),
        ("x86_64", "pc-windows-msvc", "zip"),
        ("aarch64", "pc-windows-msvc", "zip"),
    ] {
        let bytes = if ext == "zip" {
            zip.clone()
        } else {
            targz.clone()
        };
        routes.insert(
            format!("/{repo}/releases/download/{version}/{tool}-{arch}-{os}.{ext}"),
            bytes,
        );
    }
    routes
}

fn air_release_routes(version: &str) -> HashMap<String, Vec<u8>> {
    release_routes("posit-dev/air", "air", version)
}

/// A `.tar.gz` holding one file.
fn targz_with(entry_path: &str, contents: &[u8]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, entry_path, contents)
        .expect("append tar entry");
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

/// A `.zip` holding one file.
fn zip_with(entry_path: &str, contents: &[u8]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file(entry_path, zip::write::SimpleFileOptions::default())
        .expect("start zip entry");
    writer.write_all(contents).expect("write zip entry");
    writer.finish().expect("finish zip").into_inner()
}

/// A local HTTP server handing out fixture release assets; unknown paths
/// get a 404, like GitHub.
struct FixtureServer {
    server: Arc<tiny_http::Server>,
    base_url: String,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FixtureServer {
    fn serve(routes: HashMap<String, Vec<u8>>) -> FixtureServer {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind"));
        let addr = server.server_addr().to_ip().expect("ip listener");
        let routes = Arc::new(Mutex::new(routes));
        let handle = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    let url = request.url().to_string();
                    let response = match routes.lock().expect("routes lock").get(&url) {
                        Some(bytes) => {
                            tiny_http::Response::from_data(bytes.clone()).with_status_code(200)
                        }
                        None => tiny_http::Response::from_data(Vec::new()).with_status_code(404),
                    };
                    let _ = request.respond(response);
                }
            })
        };
        FixtureServer {
            server,
            base_url: format!("http://{addr}"),
            handle: Some(handle),
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
