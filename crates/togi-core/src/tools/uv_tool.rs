//! Installs Python tools with a private copy of uv.
//!
//! `uv tool install <package>==<version>` runs with `UV_TOOL_DIR` and
//! `UV_TOOL_BIN_DIR` pointed inside the togi tool cache, so these installs
//! never touch (or collide with) the user's own uv tools. The executable
//! lands at `<data_dir>/tools/<name>/<version>/<binary>` — the same layout
//! as GitHub-release installs — with the venv beside it and a
//! `manifest.json` written last as the completion marker. uv itself is
//! bootstrapped on demand through the GitHub release downloader.
//!
//! Unlike archive installs there is no staging + rename: the venv scripts
//! embed absolute paths, so moving a finished install would break it.
//! Instead the install runs in place under the per-tool lock, and anything
//! at the final path without a manifest is treated as interrupted and
//! rebuilt.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use crate::term;
use crate::term::HintExt;
use crate::tools::cache::ToolCache;
use crate::tools::download::{Downloader, InstallContext, ToolLock, fetch_message};
use crate::tools::manifest::Manifest;
use crate::tools::platform::Platform;
use crate::tools::spec::{ToolKind, ToolSpec};

/// Subdirectory of a tool's version directory holding the uv-managed
/// virtualenvs (`UV_TOOL_DIR`); executables land in the version directory
/// itself (`UV_TOOL_BIN_DIR`).
const VENVS_DIR: &str = "uv-tools";

/// Where a private uv executable comes from.
enum UvSource {
    /// Bootstrap this version of uv from its GitHub release on first use
    /// (the resolved version: config pin, else the baked default).
    Bootstrap {
        downloader: Downloader,
        version: String,
    },
    /// Use this uv executable directly (tests inject fakes here).
    Binary(PathBuf),
}

/// Installs `uv tool` packages into a [`ToolCache`].
pub struct UvToolInstaller {
    cache: ToolCache,
    platform: Platform,
    uv: UvSource,
}

impl UvToolInstaller {
    /// An installer for `platform` into `cache`, bootstrapping its private
    /// uv at `uv_version` from GitHub when it is not cached yet.
    pub fn new(cache: ToolCache, platform: Platform, uv_version: String) -> UvToolInstaller {
        let downloader = Downloader::new(cache.clone(), platform);
        UvToolInstaller {
            cache,
            platform,
            uv: UvSource::Bootstrap {
                downloader,
                version: uv_version,
            },
        }
    }

    /// An installer that runs `uv_binary` instead of bootstrapping uv
    /// (tests inject fake uv executables here).
    #[cfg(test)]
    fn with_uv_binary(cache: ToolCache, platform: Platform, uv_binary: PathBuf) -> UvToolInstaller {
        UvToolInstaller {
            cache,
            platform,
            uv: UvSource::Binary(uv_binary),
        }
    }

    /// An installer bootstrapping uv at `uv_version` through `downloader`
    /// (tests point it at a local fixture server).
    #[cfg(test)]
    fn with_downloader(
        cache: ToolCache,
        platform: Platform,
        downloader: Downloader,
        uv_version: String,
    ) -> UvToolInstaller {
        UvToolInstaller {
            cache,
            platform,
            uv: UvSource::Bootstrap {
                downloader,
                version: uv_version,
            },
        }
    }

    /// Return the path to the installed binary for `spec` at `version`,
    /// installing it (and uv itself, when missing) first when it is not
    /// cached. Cached tools are returned without taking the lock or
    /// touching the network.
    pub fn ensure_installed(
        &self,
        spec: &ToolSpec,
        version: &str,
        ctx: &InstallContext,
    ) -> anyhow::Result<PathBuf> {
        let ToolKind::UvTool { package } = spec.kind else {
            // Internal misrouting, not a user mistake — but still degrade
            // to a clear error rather than a panic.
            return Err(anyhow::anyhow!(
                "`{}` installs from a GitHub release, not via `uv tool install`",
                spec.name
            ))
            .hint("this is an togi bug; please report it");
        };

        let binary = self.cache.binary_path(spec.name, version, self.platform);
        if self.is_installed(spec.name, version, &binary) {
            return Ok(binary);
        }

        // Resolve uv before taking this tool's lock: bootstrapping takes
        // uv's own lock, and resolving it first keeps the locks un-nested.
        // The breadcrumb matters: the user asked for this tool, not for
        // uv, so a bootstrap failure must say why uv is involved at all.
        let uv = self.uv_binary(ctx).with_context(|| {
            format!(
                "{} installs via togi's private uv, which could not be set up",
                spec.name
            )
        })?;

        let name_dir = self.cache.root().join(spec.name);
        let mut lock = ToolLock::open(&name_dir)?;
        let _guard = lock.exclusive()?;

        // Another process may have finished the install while we waited.
        if self.is_installed(spec.name, version, &binary) {
            return Ok(binary);
        }

        let tool_dir = self.cache.tool_dir(spec.name, version);
        if let Err(err) = self.install(&uv, package, spec, version, ctx, &tool_dir, &binary) {
            // Leave nothing half-installed behind: without a manifest the
            // directory would be treated as corrupt anyway.
            let _ = fs::remove_dir_all(&tool_dir);
            return Err(err);
        }
        Ok(binary)
    }

    /// Whether `binary` (plus its manifest) is already installed. The
    /// manifest is written last, so its presence means the install
    /// completed.
    fn is_installed(&self, name: &str, version: &str, binary: &Path) -> bool {
        binary.is_file() && self.cache.manifest_path(name, version).is_file()
    }

    /// The private uv executable, bootstrapping it from its GitHub release
    /// when it is not cached yet.
    fn uv_binary(&self, ctx: &InstallContext) -> anyhow::Result<PathBuf> {
        match &self.uv {
            UvSource::Binary(path) => Ok(path.clone()),
            UvSource::Bootstrap {
                downloader,
                version,
            } => {
                // Static data: uv is always in the built-in tool set.
                let uv = ToolSpec::builtin("uv").expect("uv is a built-in tool");
                let uv_ctx = InstallContext {
                    label: "uv (Python tool installer)",
                    command: ctx.command,
                    verbose: ctx.verbose,
                };
                downloader.ensure_installed(&uv, version, &uv_ctx)
            }
        }
    }

    /// Run `uv tool install` into `tool_dir` and write the manifest.
    /// Caller holds the tool lock and cleans `tool_dir` up on error.
    #[allow(clippy::too_many_arguments)] // internal plumbing below ensure_installed
    fn install(
        &self,
        uv: &Path,
        package: &str,
        spec: &ToolSpec,
        version: &str,
        ctx: &InstallContext,
        tool_dir: &Path,
        binary: &Path,
    ) -> anyhow::Result<()> {
        // Anything already at the final path has no manifest (checked
        // above), so it is an interrupted install: clear it and rebuild.
        if tool_dir.exists() {
            fs::remove_dir_all(tool_dir)
                .with_context(|| {
                    format!("could not remove corrupt install `{}`", tool_dir.display())
                })
                .hint("remove the directory by hand, or run `togi tools clean`")?;
        }
        fs::create_dir_all(tool_dir)
            .with_context(|| format!("could not create tool directory `{}`", tool_dir.display()))
            .hint("check that the togi data directory is writable")?;

        self.run_uv_install(uv, package, spec, version, ctx, tool_dir)?;

        if !binary.is_file() {
            return Err(anyhow::anyhow!(
                "`uv tool install {package}=={version}` succeeded but produced no `{}`",
                binary.display()
            ))
            .hint(
                "the package's entry points may have changed; pin a different \
                 version in togi.toml or report an togi bug",
            );
        }

        // The manifest lands last: its presence marks the install complete.
        Manifest::new(
            version.to_string(),
            format!("https://pypi.org/project/{package}/{version}/"),
            None,
        )
        .save(&self.cache.manifest_path(spec.name, version))
    }

    /// Run `uv tool install <package>==<version>` with the tool and bin
    /// directories pointed inside `tool_dir`, so nothing touches the
    /// user's own uv tools.
    fn run_uv_install(
        &self,
        uv: &Path,
        package: &str,
        spec: &ToolSpec,
        version: &str,
        ctx: &InstallContext,
        tool_dir: &Path,
    ) -> anyhow::Result<()> {
        let install_spec = format!("{package}=={version}");
        let message = fetch_message(ctx.label, spec.name, version, ctx.verbose);
        // uv reports no byte-level progress here, so the bar is a static
        // "Fetching …" line that clears when uv finishes. One live bar at
        // a time: see `download::PROGRESS_SECTION`.
        let _bar_section = crate::tools::download::progress_section();
        let bar = term::progress_bar(1, message);
        let output = Command::new(uv)
            .args(["tool", "install", &install_spec])
            .env("UV_TOOL_DIR", tool_dir.join(VENVS_DIR))
            .env("UV_TOOL_BIN_DIR", tool_dir)
            .output();
        bar.finish_and_clear();

        let output = output
            .with_context(|| format!("could not run `{}`", uv.display()))
            .hint("run `togi tools clean` to reset the tool cache, then retry")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "`uv tool install {install_spec}` failed:\n{}",
                stderr.trim()
            ))
            .hint(format!(
                "{} installs via togi's private uv; `{}` needs network access \
                 to install it the first time; check your connection (or \
                 HTTPS_PROXY) and the `[tools.{}]` version pin in togi.toml, \
                 then rerun",
                spec.name, ctx.command, spec.name
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::platform::{Arch, Os};

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn sql_spec() -> ToolSpec {
        ToolSpec {
            name: "sqlfluff",
            default_version: "3.4.0",
            kind: ToolKind::UvTool {
                package: "sqlfluff",
            },
        }
    }

    fn ctx() -> InstallContext<'static> {
        InstallContext {
            label: "SQL linter",
            command: "togi lint",
            verbose: false,
        }
    }

    /// Write an executable `uv` shell script whose body is `body`.
    #[cfg(unix)]
    fn fake_uv(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("uv");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake uv");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake uv");
        path
    }

    /// Script body that records its args and tool env vars to `record`,
    /// then creates the executable uv would have installed.
    #[cfg(unix)]
    fn recording_uv_body(record: &Path) -> String {
        format!(
            r#"record="{record}"
echo "args:$@" > "$record"
echo "UV_TOOL_DIR=$UV_TOOL_DIR" >> "$record"
echo "UV_TOOL_BIN_DIR=$UV_TOOL_BIN_DIR" >> "$record"
mkdir -p "$UV_TOOL_BIN_DIR"
printf 'fake sqlfluff' > "$UV_TOOL_BIN_DIR/sqlfluff"
chmod +x "$UV_TOOL_BIN_DIR/sqlfluff""#,
            record = record.display()
        )
    }

    #[cfg(unix)]
    #[test]
    fn runs_uv_with_togi_owned_tool_dirs_and_a_pinned_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let uv = fake_uv(dir.path(), &recording_uv_body(&record));
        let cache = ToolCache::at(dir.path());
        let installer = UvToolInstaller::with_uv_binary(cache.clone(), linux(), uv);

        let binary = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("install");

        assert_eq!(binary, cache.binary_path("sqlfluff", "3.4.0", linux()));
        assert_eq!(fs::read(&binary).expect("read binary"), b"fake sqlfluff");

        let tool_dir = cache.tool_dir("sqlfluff", "3.4.0");
        let recorded = fs::read_to_string(&record).expect("uv must have been run");
        assert!(
            recorded.contains("args:tool install sqlfluff==3.4.0"),
            "{recorded}"
        );
        assert!(
            recorded.contains(&format!(
                "UV_TOOL_DIR={}",
                tool_dir.join(VENVS_DIR).display()
            )),
            "{recorded}"
        );
        assert!(
            recorded.contains(&format!("UV_TOOL_BIN_DIR={}", tool_dir.display())),
            "{recorded}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn records_a_manifest_like_github_installs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let uv = fake_uv(dir.path(), &recording_uv_body(&record));
        let cache = ToolCache::at(dir.path());
        let installer = UvToolInstaller::with_uv_binary(cache.clone(), linux(), uv);

        installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("install");

        let manifest = Manifest::load(&cache.manifest_path("sqlfluff", "3.4.0")).expect("manifest");
        assert_eq!(manifest.version, "3.4.0");
        assert!(manifest.source_url.contains("sqlfluff"), "{manifest:?}");
        assert_eq!(manifest.checksum, None);
    }

    #[cfg(unix)]
    #[test]
    fn pins_whatever_version_it_is_asked_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let uv = fake_uv(dir.path(), &recording_uv_body(&record));
        let installer = UvToolInstaller::with_uv_binary(ToolCache::at(dir.path()), linux(), uv);

        installer
            .ensure_installed(&sql_spec(), "9.9.9", &ctx())
            .expect("install");

        let recorded = fs::read_to_string(&record).expect("uv must have been run");
        assert!(
            recorded.contains("args:tool install sqlfluff==9.9.9"),
            "{recorded}"
        );
    }

    #[test]
    fn cached_install_short_circuits_without_running_uv() {
        // The injected "uv" does not exist: any attempt to run it would
        // error out, so success proves the fast path is exec- and
        // network-free.
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let binary = cache.binary_path("sqlfluff", "3.4.0", linux());
        fs::create_dir_all(binary.parent().expect("tool dir")).expect("create tool dir");
        fs::write(&binary, b"fake sqlfluff").expect("write binary");
        Manifest::new(
            "3.4.0".to_string(),
            "https://pypi.org/project/sqlfluff/3.4.0/".to_string(),
            None,
        )
        .save(&cache.manifest_path("sqlfluff", "3.4.0"))
        .expect("write manifest");

        let missing_uv = dir.path().join("no-such-uv");
        let installer = UvToolInstaller::with_uv_binary(cache, linux(), missing_uv);
        let installed = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("cache hit runs nothing");
        assert_eq!(installed, binary);
    }

    #[cfg(unix)]
    #[test]
    fn second_install_does_not_rerun_uv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let counter = dir.path().join("count.txt");
        let body = format!(
            "echo run >> \"{}\"\n{}",
            counter.display(),
            recording_uv_body(&record)
        );
        let uv = fake_uv(dir.path(), &body);
        let installer = UvToolInstaller::with_uv_binary(ToolCache::at(dir.path()), linux(), uv);

        installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("first install");
        installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("second install");

        let runs = fs::read_to_string(&counter).expect("counter");
        assert_eq!(runs.lines().count(), 1, "uv must run exactly once");
    }

    #[cfg(unix)]
    #[test]
    fn uv_failure_propagates_its_stderr_and_names_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uv = fake_uv(
            dir.path(),
            "echo 'No solution found when resolving tool dependencies' >&2\nexit 2",
        );
        let cache = ToolCache::at(dir.path());
        let installer = UvToolInstaller::with_uv_binary(cache.clone(), linux(), uv);

        let err = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect_err("uv failure must fail the install");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("No solution found"), "{rendered}");
        assert!(rendered.contains("togi lint"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
        // The user never asked for uv; the error must explain where it
        // comes from.
        assert!(
            rendered.contains("sqlfluff installs via togi's private uv"),
            "{rendered}"
        );
        assert!(
            !cache.tool_dir("sqlfluff", "3.4.0").exists(),
            "a failed install must leave no tool directory behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uv_success_without_the_expected_binary_fails_with_guidance() {
        // A uv that exits 0 but installs nothing (wrong package layout,
        // renamed entry point, ...) must not be recorded as installed.
        let dir = tempfile::tempdir().expect("tempdir");
        let uv = fake_uv(dir.path(), "exit 0");
        let cache = ToolCache::at(dir.path());
        let installer = UvToolInstaller::with_uv_binary(cache.clone(), linux(), uv);

        let err = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect_err("missing binary must fail");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("sqlfluff"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
        assert!(
            !cache.tool_dir("sqlfluff", "3.4.0").exists(),
            "a failed install must leave no tool directory behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_install_without_a_manifest_is_rebuilt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let uv = fake_uv(dir.path(), &recording_uv_body(&record));
        let cache = ToolCache::at(dir.path());

        // A binary without a manifest: the mark of an interrupted install.
        let binary = cache.binary_path("sqlfluff", "3.4.0", linux());
        fs::create_dir_all(binary.parent().expect("tool dir")).expect("create tool dir");
        fs::write(&binary, b"truncated garbage").expect("write corrupt binary");

        let installer = UvToolInstaller::with_uv_binary(cache.clone(), linux(), uv);
        let installed = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("rebuild over interrupted install");

        assert_eq!(fs::read(&installed).expect("read binary"), b"fake sqlfluff");
        Manifest::load(&cache.manifest_path("sqlfluff", "3.4.0")).expect("manifest rewritten");
    }

    #[test]
    fn uv_bootstrap_failure_breadcrumbs_the_private_uv() {
        // Offline first run: fetching uv itself fails before sqlfluff is
        // ever attempted. The error must say sqlfluff is the tool that
        // dragged uv in.
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let downloader = Downloader::at_base_url(cache.clone(), linux(), dead_url());
        let installer =
            UvToolInstaller::with_downloader(cache, linux(), downloader, "0.9.0".to_string());

        let err = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect_err("an unreachable release host must fail the bootstrap");
        let rendered = crate::term::render_error(&err, false);
        assert!(
            rendered.contains("sqlfluff installs via togi's private uv"),
            "{rendered}"
        );
    }

    /// A `http://127.0.0.1:<port>` URL nothing listens on, so any
    /// download attempt fails like a machine with no network.
    fn dead_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn missing_uv_binary_error_says_what_to_do() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_uv = dir.path().join("no-such-uv");
        let installer =
            UvToolInstaller::with_uv_binary(ToolCache::at(dir.path()), linux(), missing_uv);

        let err = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect_err("unrunnable uv must fail");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    #[test]
    fn github_binaries_are_not_installable_via_uv() {
        let spec = ToolSpec {
            name: "air",
            default_version: "0.10.0",
            kind: ToolKind::GithubBinary {
                repo: "posit-dev/air",
                asset_pattern: "air-{arch}-{os}.{ext}",
                checksum_pattern: None,
            },
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let installer = UvToolInstaller::with_uv_binary(
            ToolCache::at(dir.path()),
            linux(),
            dir.path().join("uv"),
        );
        let err = installer
            .ensure_installed(&spec, "0.10.0", &ctx())
            .expect_err("github binaries take a different install path");
        assert!(err.to_string().contains("GitHub"), "{err}");
    }

    /// End to end offline: uv itself is bootstrapped from a fixture server
    /// through the GitHub downloader, then run to install the tool.
    #[cfg(unix)]
    #[test]
    fn bootstraps_a_private_uv_before_installing() {
        use std::collections::HashMap;

        use crate::tools::test_support::{FixtureServer, sha256_hex_of, targz_with};
        use crate::tools::versions;

        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let script = format!("#!/bin/sh\n{}\n", recording_uv_body(&record));
        let archive = targz_with("uv-x86_64-unknown-linux-gnu/uv", script.as_bytes());

        let asset_path = format!(
            "/astral-sh/uv/releases/download/{}/uv-x86_64-unknown-linux-gnu.tar.gz",
            versions::UV
        );
        let checksum = format!(
            "{}  uv-x86_64-unknown-linux-gnu.tar.gz\n",
            sha256_hex_of(&archive)
        );
        let server = FixtureServer::serve(HashMap::from([
            (asset_path.clone(), archive),
            (format!("{asset_path}.sha256"), checksum.into_bytes()),
        ]));

        let cache = ToolCache::at(dir.path());
        let downloader = Downloader::at_base_url(cache.clone(), linux(), server.base_url.clone());
        let installer = UvToolInstaller::with_downloader(
            cache.clone(),
            linux(),
            downloader,
            versions::UV.to_string(),
        );

        let binary = installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("bootstrap uv, then install");

        assert_eq!(fs::read(&binary).expect("read binary"), b"fake sqlfluff");
        // The private uv landed in the cache like any GitHub tool...
        Manifest::load(&cache.manifest_path("uv", versions::UV)).expect("uv manifest");
        // ...and a second install touches neither uv nor the network.
        let hits_after_first = server.hits().len();
        installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("cached install");
        assert_eq!(server.hits().len(), hits_after_first, "no new downloads");
    }

    /// The bootstrap fetches the uv version it was constructed with — a
    /// `[tools] uv` pin must reach all the way down here, not silently
    /// fall back to the baked default.
    #[cfg(unix)]
    #[test]
    fn bootstrap_honors_a_pinned_uv_version() {
        use std::collections::HashMap;

        use crate::tools::test_support::{FixtureServer, sha256_hex_of, targz_with};

        let pinned = "9.9.8";
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record.txt");
        let script = format!("#!/bin/sh\n{}\n", recording_uv_body(&record));
        let archive = targz_with("uv-x86_64-unknown-linux-gnu/uv", script.as_bytes());

        let asset_path =
            format!("/astral-sh/uv/releases/download/{pinned}/uv-x86_64-unknown-linux-gnu.tar.gz");
        let checksum = format!(
            "{}  uv-x86_64-unknown-linux-gnu.tar.gz\n",
            sha256_hex_of(&archive)
        );
        let server = FixtureServer::serve(HashMap::from([
            (asset_path.clone(), archive),
            (format!("{asset_path}.sha256"), checksum.into_bytes()),
        ]));

        let cache = ToolCache::at(dir.path());
        let downloader = Downloader::at_base_url(cache.clone(), linux(), server.base_url.clone());
        let installer = UvToolInstaller::with_downloader(
            cache.clone(),
            linux(),
            downloader,
            pinned.to_string(),
        );

        installer
            .ensure_installed(&sql_spec(), "3.4.0", &ctx())
            .expect("bootstrap the pinned uv, then install");

        let manifest = Manifest::load(&cache.manifest_path("uv", pinned))
            .expect("the pinned uv version must land in the cache");
        assert_eq!(manifest.version, pinned);
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;

    /// Bootstraps a real uv from GitHub, installs real sqlfluff from PyPI,
    /// and runs `--version`.
    /// Run with: `cargo test --features online-tests -- --ignored`
    #[test]
    #[ignore = "downloads real uv and sqlfluff from the network"]
    fn installs_real_sqlfluff_and_runs_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let platform = Platform::current().expect("supported platform");
        let spec = ToolSpec::builtin("sqlfluff").expect("sqlfluff is built in");
        let ctx = InstallContext {
            label: "SQL linter",
            command: "togi lint",
            verbose: true,
        };

        let binary = UvToolInstaller::new(cache, platform, crate::tools::versions::UV.to_string())
            .ensure_installed(&spec, spec.default_version, &ctx)
            .expect("bootstrap uv and install sqlfluff");
        let output = Command::new(&binary)
            .arg("--version")
            .output()
            .expect("run sqlfluff --version");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(spec.default_version),
            "sqlfluff --version must report {}: {stdout}",
            spec.default_version
        );
    }
}
