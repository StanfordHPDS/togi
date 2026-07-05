//! Downloads GitHub-released tool binaries into the tool cache.
//!
//! Installs are atomic: the archive is downloaded and extracted into a
//! staging directory on the same filesystem as the cache, verified against
//! the published sha256 when there is one, and only then renamed into
//! `<data_dir>/tools/<name>/<version>/`. A per-tool advisory lock keeps
//! concurrent togi processes from corrupting the cache; the fast path
//! (already installed) takes no lock and touches no network.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Context;
use sha2::Digest;

use crate::term;
use crate::term::HintExt;
use crate::tools::cache::ToolCache;
use crate::tools::manifest::Manifest;
use crate::tools::platform::Platform;
use crate::tools::spec::{ToolKind, ToolSpec};

/// Where release assets are downloaded from.
const GITHUB_BASE: &str = "https://github.com";

/// Prefix of staging directories inside `<tools>/<name>/`; anything with
/// this prefix is a leftover from an interrupted install and is swept away
/// under the tool lock.
const STAGING_PREFIX: &str = ".staging-";

/// Name of the per-tool advisory lock file inside `<tools>/<name>/`.
const LOCK_FILE: &str = ".lock";

/// Serializes the sections that draw a download progress bar. Parallel
/// adapter threads can each hit a first-run tool download at once, and
/// two live bars interleave on stderr into garbage. First-run downloads
/// are rare and short, so a plain mutex (one bar at a time, the rest
/// wait) keeps the rendering simple — no `MultiProgress` coordination.
static PROGRESS_SECTION: Mutex<()> = Mutex::new(());

/// Hold the progress-bar section. A poisoned lock is reclaimed rather
/// than panicking: the guarded value is `()`, so there is no state a
/// panic could have corrupted.
pub(crate) fn progress_section() -> MutexGuard<'static, ()> {
    PROGRESS_SECTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The per-tool advisory lock: one lock file per `<tools>/<name>/`
/// directory, shared by every install strategy, so concurrent togi
/// processes queue up instead of clobbering each other's installs.
pub(crate) struct ToolLock {
    lock: fd_lock::RwLock<fs::File>,
}

impl ToolLock {
    /// Open (creating as needed) the lock file inside `name_dir`, creating
    /// the directory itself first when it does not exist yet.
    pub(crate) fn open(name_dir: &Path) -> anyhow::Result<ToolLock> {
        fs::create_dir_all(name_dir)
            .with_context(|| format!("could not create tool directory `{}`", name_dir.display()))
            .hint("check that the togi data directory is writable")?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(name_dir.join(LOCK_FILE))
            .with_context(|| format!("could not open the lock file in `{}`", name_dir.display()))
            .hint("check that the togi data directory is writable")?;
        Ok(ToolLock {
            lock: fd_lock::RwLock::new(file),
        })
    }

    /// Block until this process holds the exclusive lock; the returned
    /// guard releases it on drop.
    pub(crate) fn exclusive(&mut self) -> anyhow::Result<fd_lock::RwLockWriteGuard<'_, fs::File>> {
        self.lock
            .write()
            .context("could not lock the tool cache")
            .hint("another togi process may have crashed; retry, or run `togi tools clean`")
    }
}

/// How an install run should talk to the user.
#[derive(Debug, Clone, Copy)]
pub struct InstallContext<'a> {
    /// Human label for progress output, e.g. `"R formatter"`.
    pub label: &'a str,
    /// The togi command that triggered the install, e.g. `"togi format"`;
    /// named in the error when the download needs network and has none.
    pub command: &'a str,
    /// Whether to include the tool name and version in progress output.
    pub verbose: bool,
}

/// Downloads and installs tools into a [`ToolCache`].
pub struct Downloader {
    cache: ToolCache,
    platform: Platform,
    base_url: String,
    agent: ureq::Agent,
}

impl Downloader {
    /// A downloader fetching from GitHub for `platform` into `cache`,
    /// honoring the internal `TOGI_RELEASE_BASE_URL` override (used by
    /// tests to point installs at a local fixture server, mirroring
    /// `TOGI_DATA_DIR`). Checksum files come from the same host, so with
    /// the override set, verification only guards against corrupt
    /// transfers, not a hostile host.
    pub fn new(cache: ToolCache, platform: Platform) -> Downloader {
        let base_url =
            std::env::var("TOGI_RELEASE_BASE_URL").unwrap_or_else(|_| GITHUB_BASE.to_string());
        Downloader::at_base_url(cache, platform, base_url)
    }

    /// A downloader against an arbitrary release host (tests point this at
    /// a local fixture server).
    pub(crate) fn at_base_url(
        cache: ToolCache,
        platform: Platform,
        base_url: String,
    ) -> Downloader {
        Downloader {
            cache,
            platform,
            base_url,
            agent: github_agent(),
        }
    }

    /// Return the path to the installed binary for `spec` at `version`,
    /// downloading and installing it first when it is not cached.
    ///
    /// Cached tools are returned without taking the lock or touching the
    /// network.
    pub fn ensure_installed(
        &self,
        spec: &ToolSpec,
        version: &str,
        ctx: &InstallContext,
    ) -> anyhow::Result<PathBuf> {
        let ToolKind::GithubBinary { repo, .. } = spec.kind else {
            // Internal misrouting, not a user mistake — but still degrade
            // to a clear error rather than a panic.
            return Err(anyhow::anyhow!(
                "`{}` installs via `uv tool install`, not from a GitHub release",
                spec.name
            ))
            .hint("this is an togi bug; please report it");
        };

        let binary = self.cache.binary_path(spec.name, version, self.platform);
        if self.is_installed(spec.name, version, &binary) {
            return Ok(binary);
        }

        // One lock per tool directory: concurrent togi processes queue up
        // here instead of clobbering each other's downloads.
        let name_dir = self.cache.root().join(spec.name);
        let mut lock = ToolLock::open(&name_dir)?;
        let _guard = lock.exclusive()?;

        // Another process may have finished the install while we waited.
        if self.is_installed(spec.name, version, &binary) {
            return Ok(binary);
        }

        sweep_stale_staging(&name_dir);

        // Anything at the final path without a manifest is an interrupted
        // or corrupt install: clear it and re-download.
        let tool_dir = self.cache.tool_dir(spec.name, version);
        if tool_dir.exists() {
            fs::remove_dir_all(&tool_dir)
                .with_context(|| {
                    format!("could not remove corrupt install `{}`", tool_dir.display())
                })
                .hint("remove the directory by hand, or run `togi tools clean`")?;
        }

        self.install(spec, repo, version, ctx, &name_dir, &tool_dir)?;
        Ok(binary)
    }

    /// Download `spec`'s release archive at `version` into `dest_dir`,
    /// verifying it against the published checksum, and return the archive
    /// path. Nothing is cached or extracted: callers that install a whole
    /// archive tree (rather than a single cached binary) take it from here.
    pub fn fetch_archive(
        &self,
        spec: &ToolSpec,
        version: &str,
        ctx: &InstallContext,
        dest_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let ToolKind::GithubBinary { repo, .. } = spec.kind else {
            // Internal misrouting, not a user mistake — but still degrade
            // to a clear error rather than a panic.
            return Err(anyhow::anyhow!(
                "`{}` installs via `uv tool install`, not from a GitHub release",
                spec.name
            ))
            .hint("this is an togi bug; please report it");
        };
        let asset = spec
            .asset_name(self.platform, version)
            .expect("GithubBinary specs always resolve an asset name");
        let archive_path = dest_dir.join(&asset);
        let message = fetch_message(ctx.label, spec.name, version, ctx.verbose);
        let (_url, tag, actual_sha256) =
            self.download_archive(spec, repo, version, &asset, &archive_path, &message, ctx)?;
        self.verify_checksum(spec, repo, version, &tag, &asset, &actual_sha256, ctx)?;
        Ok(archive_path)
    }

    /// Whether `binary` (plus its manifest) is already installed. The
    /// manifest is written last, inside the same atomic rename, so its
    /// presence means the install completed.
    fn is_installed(&self, name: &str, version: &str, binary: &Path) -> bool {
        binary.is_file() && self.cache.manifest_path(name, version).is_file()
    }

    /// Download, verify, extract, and atomically move one tool version
    /// into place. Caller holds the tool lock.
    fn install(
        &self,
        spec: &ToolSpec,
        repo: &str,
        version: &str,
        ctx: &InstallContext,
        name_dir: &Path,
        tool_dir: &Path,
    ) -> anyhow::Result<()> {
        let asset = spec
            .asset_name(self.platform, version)
            .expect("GithubBinary specs always resolve an asset name");

        // Staging lives inside the tool's own cache directory so the final
        // rename never crosses a filesystem boundary.
        let staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(name_dir)
            .with_context(|| {
                format!(
                    "could not create a staging directory in `{}`",
                    name_dir.display()
                )
            })
            .hint("check that the togi data directory is writable")?;

        let archive_path = staging.path().join(&asset);
        let message = fetch_message(ctx.label, spec.name, version, ctx.verbose);
        let (url, tag, actual_sha256) =
            self.download_archive(spec, repo, version, &asset, &archive_path, &message, ctx)?;

        let checksum =
            self.verify_checksum(spec, repo, version, &tag, &asset, &actual_sha256, ctx)?;

        let install_dir = staging.path().join("install");
        fs::create_dir(&install_dir).context("could not create the install staging directory")?;
        let binary_name = self.platform.binary_name(spec.name);
        let staged_binary = install_dir.join(&binary_name);
        extract_binary(&archive_path, &asset, &binary_name, &staged_binary)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged_binary, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("could not mark `{binary_name}` executable"))?;
        }

        // The manifest lands before the rename: once the final directory
        // exists it is complete by construction.
        Manifest::new(version.to_string(), url, checksum)
            .save(&install_dir.join("manifest.json"))?;

        fs::rename(&install_dir, tool_dir)
            .with_context(|| {
                format!(
                    "could not move the finished install into `{}`",
                    tool_dir.display()
                )
            })
            .hint("run `togi tools clean` to reset the tool cache, then retry")?;
        Ok(())
    }

    /// Fetch the release archive to `dest`, probing the bare tag and then
    /// the `v`-prefixed tag (projects differ on which they use). Returns
    /// the URL that worked, its tag, and the sha256 of the downloaded
    /// bytes.
    #[allow(clippy::too_many_arguments)] // internal plumbing below ensure_installed
    fn download_archive(
        &self,
        spec: &ToolSpec,
        repo: &str,
        version: &str,
        asset: &str,
        dest: &Path,
        message: &str,
        ctx: &InstallContext,
    ) -> anyhow::Result<(String, String, String)> {
        let mut tried = Vec::new();
        for tag in [version.to_string(), format!("v{version}")] {
            let url = self.release_url(repo, &tag, asset);
            match self.agent.get(&url).call() {
                Ok(mut response) => {
                    let sha256 = stream_to_file(&mut response, dest, message)
                        .with_context(|| format!("download of `{url}` was interrupted"))
                        .hint(format!("rerun `{}` to retry the download", ctx.command))?;
                    return Ok((url, tag, sha256));
                }
                Err(ureq::Error::StatusCode(404)) => tried.push(url),
                Err(ureq::Error::StatusCode(code)) => {
                    return Err(anyhow::anyhow!("GitHub returned HTTP {code} for `{url}`"))
                        .hint("GitHub may be rate-limiting or down; retry in a few minutes");
                }
                Err(err) => {
                    return Err(anyhow::Error::new(err))
                        .with_context(|| {
                            format!("could not download {} {version} from `{url}`", spec.name)
                        })
                        .hint(format!(
                            "`{}` needs network access to install {} the first time; \
                             check your connection (or HTTPS_PROXY) and rerun",
                            ctx.command, spec.name
                        ));
                }
            }
        }
        Err(anyhow::anyhow!(
            "no release asset `{asset}` for {} {version} (tried {})",
            spec.name,
            tried.join(" and ")
        ))
        .hint(format!(
            "check the `[tools.{}]` version pin in togi.toml, or upgrade togi \
             for newer default versions",
            spec.name
        ))
    }

    /// Verify the downloaded archive against the release's published
    /// sha256. Returns the verified digest, or `None` (after a visible
    /// warning) when the release publishes no checksum.
    #[allow(clippy::too_many_arguments)] // internal plumbing below ensure_installed
    fn verify_checksum(
        &self,
        spec: &ToolSpec,
        repo: &str,
        version: &str,
        tag: &str,
        asset: &str,
        actual_sha256: &str,
        ctx: &InstallContext,
    ) -> anyhow::Result<Option<String>> {
        let skip = |reason: &str| {
            term::warn(&format!(
                "{reason}; installing {} {version} without checksum verification",
                spec.name
            ));
        };
        let Some(checksum_asset) = spec.checksum_asset_name(self.platform, version) else {
            skip("this tool publishes no checksums");
            return Ok(None);
        };
        let url = self.release_url(repo, tag, &checksum_asset);
        let text = match self.agent.get(&url).call() {
            Ok(mut response) => response
                .body_mut()
                .read_to_string()
                .with_context(|| format!("could not read the checksum file at `{url}`"))
                .hint(format!("rerun `{}` to retry the download", ctx.command))?,
            // Only a 404 means the release publishes no checksum asset;
            // any other status (403 rate limit, 5xx) is transient and must
            // not quietly downgrade to an unverified install.
            Err(ureq::Error::StatusCode(404)) => {
                skip("this release publishes no checksum asset");
                return Ok(None);
            }
            Err(ureq::Error::StatusCode(code)) => {
                return Err(anyhow::anyhow!("GitHub returned HTTP {code} for `{url}`"))
                    .hint("GitHub may be rate-limiting or down; retry in a few minutes");
            }
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("could not download the checksum file `{url}`"))
                    .hint(format!(
                        "`{}` needs network access to install {}; check your \
                         connection (or HTTPS_PROXY) and rerun",
                        ctx.command, spec.name
                    ));
            }
        };
        let Some(expected) = parse_checksum(&text, asset) else {
            return Err(anyhow::anyhow!(
                "the checksum file `{url}` holds no sha256 digest"
            ))
            .hint("the release looks malformed; retry, or pin a different version in togi.toml");
        };
        if expected != actual_sha256 {
            return Err(anyhow::anyhow!(
                "sha256 mismatch for `{asset}`: expected {expected}, got {actual_sha256}"
            ))
            .hint(format!(
                "the download was corrupted in transit; rerun `{}` to download it again",
                ctx.command
            ));
        }
        Ok(Some(expected))
    }

    /// `<base>/<repo>/releases/download/<tag>/<asset>`.
    fn release_url(&self, repo: &str, tag: &str, asset: &str) -> String {
        format!("{}/{repo}/releases/download/{tag}/{asset}", self.base_url)
    }
}

/// HTTP agent for release downloads: honors `HTTPS_PROXY`/`HTTP_PROXY`/
/// `ALL_PROXY`/`NO_PROXY` and bounds every network phase so a dead or
/// stalled connection fails instead of hanging the progress bar forever.
pub fn github_agent() -> ureq::Agent {
    agent_with_proxy(ureq::Proxy::try_from_env())
}

/// The agent [`github_agent`] builds, with an explicit proxy (tests pass
/// one directly instead of mutating process-global env vars).
fn agent_with_proxy(proxy: Option<ureq::Proxy>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(proxy)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        // Generous total-body budget: release archives are a few MB, so
        // this only trips on a transfer that has effectively stalled.
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
        .into()
}

/// Stream a response body into `dest`, drawing a progress bar and hashing
/// the bytes as they arrive. Returns the sha256 of what was written.
fn stream_to_file(
    response: &mut ureq::http::Response<ureq::Body>,
    dest: &Path,
    message: &str,
) -> anyhow::Result<String> {
    let total = response.body().content_length().unwrap_or(0);
    // One live bar at a time: see `PROGRESS_SECTION`.
    let _bar_section = progress_section();
    let bar = term::progress_bar(total, message.to_string());
    let mut reader = response.body_mut().as_reader();
    let mut file =
        fs::File::create(dest).with_context(|| format!("could not create `{}`", dest.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .context("read from the release server")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("could not write `{}`", dest.display()))?;
        bar.inc(n as u64);
    }
    bar.finish_and_clear();
    Ok(hex(&hasher.finalize()))
}

/// Extract the tool binary named `binary_name` out of `archive` to `dest`.
/// Only that one entry is written; nothing else in the archive touches the
/// filesystem.
pub fn extract_binary(
    archive: &Path,
    asset: &str,
    binary_name: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let found = if asset.ends_with(".tar.gz") || asset.ends_with(".tgz") {
        extract_from_tar_gz(archive, binary_name, dest)?
    } else if asset.ends_with(".zip") {
        extract_from_zip(archive, binary_name, dest)?
    } else {
        return Err(anyhow::anyhow!(
            "cannot extract `{asset}`: unsupported archive type"
        ))
        .hint("this is an togi bug (unexpected release asset pattern); please report it");
    };
    if !found {
        return Err(anyhow::anyhow!(
            "the release archive `{asset}` does not contain a `{binary_name}` binary"
        ))
        .hint(
            "the tool's release layout may have changed; pin a different version \
             in togi.toml or report an togi bug",
        );
    }
    Ok(())
}

/// Scan a `.tar.gz` for a file named `binary_name` and write it to `dest`.
fn extract_from_tar_gz(archive: &Path, binary_name: &str, dest: &Path) -> anyhow::Result<bool> {
    let file = fs::File::open(archive)
        .with_context(|| format!("could not reopen `{}`", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar
        .entries()
        .context("could not read the release archive")?
    {
        let mut entry = entry.context("could not read the release archive")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let matches = entry
            .path()
            .is_ok_and(|path| path.file_name() == Some(binary_name.as_ref()));
        if matches {
            let mut out = fs::File::create(dest)
                .with_context(|| format!("could not create `{}`", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .context("could not extract the tool binary from the archive")?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Scan a `.zip` for a file named `binary_name` and write it to `dest`.
fn extract_from_zip(archive: &Path, binary_name: &str, dest: &Path) -> anyhow::Result<bool> {
    let file = fs::File::open(archive)
        .with_context(|| format!("could not reopen `{}`", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("could not read the release archive")?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .context("could not read the release archive")?;
        if !entry.is_file() {
            continue;
        }
        let matches = entry
            .enclosed_name()
            .is_some_and(|path| path.file_name() == Some(binary_name.as_ref()));
        if matches {
            let mut out = fs::File::create(dest)
                .with_context(|| format!("could not create `{}`", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .context("could not extract the tool binary from the archive")?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Best-effort removal of staging directories a crashed process left
/// behind. Runs under the tool lock, so nothing here is in use.
fn sweep_stale_staging(name_dir: &Path) {
    let Ok(entries) = fs::read_dir(name_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// The progress-bar label for an install: the human label alone, plus the
/// tool name and version when verbose.
pub(crate) fn fetch_message(label: &str, name: &str, version: &str, verbose: bool) -> String {
    if verbose {
        format!("Fetching {label} ({name} {version})…")
    } else {
        format!("Fetching {label}…")
    }
}

/// Pull the expected sha256 out of a published checksum file.
///
/// Accepts a bare hex digest, `sha256sum` style `<hex>  <file>` lines, and
/// multi-asset lists (the line mentioning `asset` wins). Falls back to a
/// bare digest (a line holding nothing else) only — a list that names its
/// assets differently must not be matched against the wrong digest. `None`
/// when no line yields a 64-char hex digest.
fn parse_checksum(text: &str, asset: &str) -> Option<String> {
    let line = text.lines().find(|line| line.contains(asset)).or_else(|| {
        text.lines()
            .find(|line| !line.trim().is_empty())
            .filter(|line| line.split_whitespace().nth(1).is_none())
    })?;
    let token = line.split_whitespace().next()?.to_ascii_lowercase();
    let is_sha256 = token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit());
    is_sha256.then_some(token)
}

/// Lowercase hex of a sha256 digest.
fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::tools::platform::{Arch, Os};
    use crate::tools::test_support::{FixtureServer, sha256_hex_of, targz_with, zip_with};

    const FAKE_BINARY: &[u8] = b"#!/bin/sh\necho fake tool 1.2.3\n";

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    fn windows() -> Platform {
        Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            kind: ToolKind::GithubBinary {
                repo: "example/tool",
                asset_pattern: "tool-{version}-{arch}-{os}.{ext}",
                checksum_pattern: Some("tool-{version}-{arch}-{os}.{ext}.sha256"),
            },
        }
    }

    fn spec_without_checksums() -> ToolSpec {
        ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            kind: ToolKind::GithubBinary {
                repo: "example/tool",
                asset_pattern: "tool-{version}-{arch}-{os}.{ext}",
                checksum_pattern: None,
            },
        }
    }

    fn ctx() -> InstallContext<'static> {
        InstallContext {
            label: "R formatter",
            command: "togi format",
            verbose: false,
        }
    }

    const ARCHIVE_PATH: &str =
        "/example/tool/releases/download/1.2.3/tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
    const CHECKSUM_PATH: &str =
        "/example/tool/releases/download/1.2.3/tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz.sha256";

    /// Routes for a well-behaved release: archive plus matching checksum.
    fn release_routes(archive: &[u8]) -> HashMap<String, Vec<u8>> {
        let checksum = format!(
            "{}  tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz\n",
            sha256_hex_of(archive)
        );
        HashMap::from([
            (ARCHIVE_PATH.to_string(), archive.to_vec()),
            (CHECKSUM_PATH.to_string(), checksum.into_bytes()),
        ])
    }

    fn downloader_at(server: &FixtureServer, data_dir: &Path, platform: Platform) -> Downloader {
        Downloader::at_base_url(ToolCache::at(data_dir), platform, server.base_url.clone())
    }

    #[test]
    fn installs_a_tar_gz_release_end_to_end() {
        let archive = targz_with("tool-1.2.3/tool", FAKE_BINARY);
        let server = FixtureServer::serve(release_routes(&archive));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let binary = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect("install");

        let cache = ToolCache::at(dir.path());
        assert_eq!(binary, cache.binary_path("tool", "1.2.3", linux()));
        assert_eq!(fs::read(&binary).expect("read binary"), FAKE_BINARY);

        let manifest = Manifest::load(&cache.manifest_path("tool", "1.2.3")).expect("manifest");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(
            manifest.source_url,
            format!("{}{ARCHIVE_PATH}", server.base_url)
        );
        assert_eq!(
            manifest.checksum.as_deref(),
            Some(&*sha256_hex_of(&archive))
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_binary_is_executable() {
        use std::os::unix::fs::PermissionsExt;

        let archive = targz_with("tool-1.2.3/tool", FAKE_BINARY);
        let server = FixtureServer::serve(release_routes(&archive));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let binary = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect("install");
        let mode = fs::metadata(&binary)
            .expect("metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "binary must be executable, mode {mode:o}");
    }

    #[test]
    fn installs_a_zip_release_for_windows() {
        let archive = zip_with("tool-1.2.3/tool.exe", FAKE_BINARY);
        let path = "/example/tool/releases/download/1.2.3/tool-1.2.3-x86_64-pc-windows-msvc.zip";
        let server = FixtureServer::serve(HashMap::from([(path.to_string(), archive.clone())]));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), windows());

        let binary = downloader
            .ensure_installed(&spec_without_checksums(), "1.2.3", &ctx())
            .expect("install");
        assert!(binary.ends_with("tool.exe"), "{}", binary.display());
        assert_eq!(fs::read(&binary).expect("read binary"), FAKE_BINARY);
    }

    #[test]
    fn cache_hit_makes_no_network_requests() {
        // An unroutable base URL: any network attempt would error out.
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(cache_dir.path());
        let binary = cache.binary_path("tool", "1.2.3", linux());
        fs::create_dir_all(binary.parent().expect("tool dir")).expect("create tool dir");
        fs::write(&binary, FAKE_BINARY).expect("write binary");
        Manifest::new(
            "1.2.3".to_string(),
            "https://example.test/tool.tar.gz".to_string(),
            None,
        )
        .save(&cache.manifest_path("tool", "1.2.3"))
        .expect("write manifest");

        let downloader = Downloader::at_base_url(cache, linux(), "http://127.0.0.1:1".to_string());
        let installed = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect("cache hit needs no network");
        assert_eq!(installed, binary);
    }

    #[test]
    fn probes_the_v_prefixed_tag_when_the_bare_tag_is_missing() {
        let archive = targz_with("tool", FAKE_BINARY);
        let v_archive_path =
            "/example/tool/releases/download/v1.2.3/tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        let server = FixtureServer::serve(HashMap::from([(
            v_archive_path.to_string(),
            archive.clone(),
        )]));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let binary = downloader
            .ensure_installed(&spec_without_checksums(), "1.2.3", &ctx())
            .expect("install from v-prefixed tag");
        assert_eq!(fs::read(&binary).expect("read binary"), FAKE_BINARY);

        let hits = server.hits();
        assert_eq!(
            hits,
            vec![ARCHIVE_PATH.to_string(), v_archive_path.to_string()],
            "must probe the bare tag first, then the v-prefixed tag"
        );
    }

    #[test]
    fn missing_checksum_asset_installs_with_a_warning_and_no_recorded_checksum() {
        // Archive is served but the .sha256 asset 404s: install proceeds
        // (with a ui warning) and the manifest records no checksum.
        let archive = targz_with("tool", FAKE_BINARY);
        let server =
            FixtureServer::serve(HashMap::from([(ARCHIVE_PATH.to_string(), archive.clone())]));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect("install without published checksum");
        let manifest = Manifest::load(&ToolCache::at(dir.path()).manifest_path("tool", "1.2.3"))
            .expect("manifest");
        assert_eq!(manifest.checksum, None);
    }

    #[test]
    fn checksum_server_error_fails_instead_of_installing_unverified() {
        // Only a 404 means "this release publishes no checksum"; a 500 (or
        // 403 rate limit) is transient and must not silently skip
        // verification.
        let archive = targz_with("tool", FAKE_BINARY);
        let server = FixtureServer::serve_responses(HashMap::from([
            (ARCHIVE_PATH.to_string(), (200, archive)),
            (CHECKSUM_PATH.to_string(), (500, Vec::new())),
        ]));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let err = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect_err("a server error on the checksum asset must fail the install");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("500"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
        assert!(
            !ToolCache::at(dir.path()).tool_dir("tool", "1.2.3").exists(),
            "a failed install must leave no tool directory behind"
        );
    }

    #[test]
    fn checksum_mismatch_fails_and_leaves_nothing_installed() {
        let archive = targz_with("tool", FAKE_BINARY);
        let wrong = format!(
            "{}  tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz\n",
            "0".repeat(64)
        );
        let server = FixtureServer::serve(HashMap::from([
            (ARCHIVE_PATH.to_string(), archive),
            (CHECKSUM_PATH.to_string(), wrong.into_bytes()),
        ]));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let err = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect_err("checksum mismatch must fail");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("sha256"), "{rendered}");
        assert!(
            !ToolCache::at(dir.path()).tool_dir("tool", "1.2.3").exists(),
            "a failed install must leave no tool directory behind"
        );
    }

    #[test]
    fn corrupt_partial_install_is_cleanly_redownloaded() {
        let archive = targz_with("tool", FAKE_BINARY);
        let server = FixtureServer::serve(release_routes(&archive));
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());

        // A binary without a manifest: the mark of an interrupted install.
        let binary = cache.binary_path("tool", "1.2.3", linux());
        fs::create_dir_all(binary.parent().expect("tool dir")).expect("create tool dir");
        fs::write(&binary, b"truncated garbage").expect("write corrupt binary");
        // Plus a stale staging dir from a killed process.
        let stale = cache.root().join("tool").join(".staging-stale");
        fs::create_dir_all(&stale).expect("create stale staging dir");

        let downloader = downloader_at(&server, dir.path(), linux());
        let installed = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect("re-download over corrupt install");

        assert!(!server.hits().is_empty(), "must re-download");
        assert_eq!(fs::read(&installed).expect("read binary"), FAKE_BINARY);
        Manifest::load(&cache.manifest_path("tool", "1.2.3")).expect("manifest rewritten");
        assert!(!stale.exists(), "stale staging dirs must be swept");
    }

    #[test]
    fn concurrent_installs_download_once_and_both_succeed() {
        let archive = targz_with("tool", FAKE_BINARY);
        let server = FixtureServer::serve(release_routes(&archive));
        let dir = tempfile::tempdir().expect("tempdir");

        let (a, b) = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                downloader_at(&server, dir.path(), linux()).ensure_installed(
                    &spec(),
                    "1.2.3",
                    &ctx(),
                )
            });
            let second = scope.spawn(|| {
                downloader_at(&server, dir.path(), linux()).ensure_installed(
                    &spec(),
                    "1.2.3",
                    &ctx(),
                )
            });
            (
                first.join().expect("thread"),
                second.join().expect("thread"),
            )
        });

        let a = a.expect("first install");
        let b = b.expect("second install");
        assert_eq!(a, b);
        assert_eq!(fs::read(&a).expect("read binary"), FAKE_BINARY);
        let archive_hits = server
            .hits()
            .iter()
            .filter(|url| url.ends_with(".tar.gz"))
            .count();
        assert_eq!(
            archive_hits, 1,
            "the archive must be downloaded exactly once"
        );
    }

    #[test]
    fn offline_without_cache_names_the_command_that_needs_network() {
        // A port with nothing listening: connection refused, like no network.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = Downloader::at_base_url(
            ToolCache::at(dir.path()),
            linux(),
            format!("http://127.0.0.1:{port}"),
        );

        let err = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect_err("no network and no cache must fail");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("tool"), "{rendered}");
        assert!(rendered.contains("togi format"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    #[test]
    fn missing_release_asset_on_both_tags_reports_the_version() {
        // Server up, but no routes: every asset URL 404s on both tag forms.
        let server = FixtureServer::serve(HashMap::new());
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let err = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect_err("missing release asset must fail");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("1.2.3"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
        assert_eq!(server.hits().len(), 2, "must probe both tag forms");
    }

    #[test]
    fn archive_missing_the_binary_fails_with_guidance() {
        let archive = targz_with("README.md", b"docs only");
        let server = FixtureServer::serve(release_routes(&archive));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, dir.path(), linux());

        let err = downloader
            .ensure_installed(&spec(), "1.2.3", &ctx())
            .expect_err("archive without the binary must fail");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("tool"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    #[test]
    fn uv_tools_are_not_downloadable_from_github() {
        let uv_spec = ToolSpec {
            name: "sqlfluff",
            default_version: "3.4.0",
            kind: ToolKind::UvTool {
                package: "sqlfluff",
            },
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = Downloader::at_base_url(
            ToolCache::at(dir.path()),
            linux(),
            "http://127.0.0.1:1".to_string(),
        );
        let err = downloader
            .ensure_installed(&uv_spec, "3.4.0", &ctx())
            .expect_err("uv tools take a different install path");
        assert!(err.to_string().contains("uv"), "{err}");
    }

    #[test]
    fn agent_config_carries_the_given_proxy() {
        // github_agent feeds Proxy::try_from_env into agent_with_proxy;
        // testing the explicit-proxy plumbing avoids mutating process-wide
        // env vars under parallel tests (ureq proxies *all* schemes, so an
        // HTTPS_PROXY leak would break sibling http fixture tests).
        let proxy = ureq::Proxy::new("http://127.0.0.1:39999").expect("parse proxy url");
        assert!(
            agent_with_proxy(Some(proxy)).config().proxy().is_some(),
            "an explicit proxy must land in the agent config"
        );
        assert!(
            agent_with_proxy(None).config().proxy().is_none(),
            "no proxy given, none configured"
        );
    }

    #[test]
    fn fetch_message_hides_tool_names_unless_verbose() {
        assert_eq!(
            fetch_message("R formatter", "air", "0.10.0", false),
            "Fetching R formatter…"
        );
        assert_eq!(
            fetch_message("R formatter", "air", "0.10.0", true),
            "Fetching R formatter (air 0.10.0)…"
        );
    }

    #[test]
    fn parses_bare_and_sha256sum_style_checksum_files() {
        let digest = "a".repeat(64);
        let asset = "tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        assert_eq!(parse_checksum(&digest, asset).as_deref(), Some(&*digest));
        assert_eq!(
            parse_checksum(&format!("{digest}  {asset}\n"), asset).as_deref(),
            Some(&*digest)
        );
        // Uppercase digests normalize to lowercase.
        assert_eq!(
            parse_checksum(&digest.to_uppercase(), asset).as_deref(),
            Some(&*digest)
        );
    }

    #[test]
    fn picks_the_matching_line_from_a_multi_asset_checksum_list() {
        let asset = "tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        let text = format!(
            "{}  tool-1.2.3-aarch64-apple-darwin.tar.gz\n{}  {asset}\n",
            "b".repeat(64),
            "c".repeat(64),
        );
        assert_eq!(
            parse_checksum(&text, asset).as_deref(),
            Some(&*"c".repeat(64))
        );
    }

    #[test]
    fn refuses_to_guess_from_a_multi_asset_list_that_never_names_the_asset() {
        // A digest-plus-filename line for some *other* asset must not be
        // used as a fallback: better to report a malformed release than to
        // compare against the wrong asset's digest.
        let text = format!(
            "{}  tool-1.2.3-aarch64-apple-darwin.tar.gz\n",
            "d".repeat(64)
        );
        assert_eq!(
            parse_checksum(&text, "tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz"),
            None
        );
    }

    #[test]
    fn rejects_checksum_files_that_hold_no_digest() {
        let asset = "tool.tar.gz";
        assert_eq!(parse_checksum("", asset), None);
        assert_eq!(parse_checksum("not a digest\n", asset), None);
        assert_eq!(parse_checksum("deadbeef\n", asset), None); // too short
    }

    // --- fetch_archive (whole-archive downloads for tree installs) --------

    #[test]
    fn fetch_archive_downloads_and_verifies_into_the_given_dir() {
        let archive = targz_with("tool-1.2.3/bin/tool", FAKE_BINARY);
        let server = FixtureServer::serve(release_routes(&archive));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, &dir.path().join("cache"), linux());

        let dest = dir.path().join("staging");
        fs::create_dir_all(&dest).expect("create dest");
        let fetched = downloader
            .fetch_archive(&spec(), "1.2.3", &ctx(), &dest)
            .expect("fetch the archive");

        assert_eq!(
            fetched,
            dest.join("tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(fs::read(&fetched).expect("read archive"), archive);
        // The checksum asset was fetched and checked, nothing else.
        assert_eq!(server.hits().len(), 2, "{:?}", server.hits());
    }

    #[test]
    fn fetch_archive_rejects_a_corrupt_download() {
        let archive = targz_with("tool-1.2.3/bin/tool", FAKE_BINARY);
        let checksum = format!(
            "{}  tool-1.2.3-x86_64-unknown-linux-gnu.tar.gz\n",
            sha256_hex_of(b"different bytes")
        );
        let server = FixtureServer::serve(HashMap::from([
            (ARCHIVE_PATH.to_string(), archive),
            (CHECKSUM_PATH.to_string(), checksum.into_bytes()),
        ]));
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = downloader_at(&server, &dir.path().join("cache"), linux());

        let err = downloader
            .fetch_archive(&spec(), "1.2.3", &ctx(), dir.path())
            .expect_err("checksum mismatch must fail");
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
    }

    #[test]
    fn fetch_archive_refuses_a_non_release_tool() {
        let uv_tool = ToolSpec {
            name: "sqlfluff",
            default_version: "1.0.0",
            kind: ToolKind::UvTool {
                package: "sqlfluff",
            },
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = Downloader::at_base_url(
            ToolCache::at(dir.path()),
            linux(),
            "http://127.0.0.1:1".to_string(),
        );
        let err = downloader
            .fetch_archive(&uv_tool, "1.0.0", &ctx(), dir.path())
            .expect_err("uv tools have no release archive");
        assert!(err.to_string().contains("uv tool install"), "{err}");
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;

    /// Downloads a real air release from GitHub and runs `--version`.
    /// Run with: `cargo test --features online-tests -- --ignored`
    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn downloads_real_air_and_runs_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let platform = Platform::current().expect("supported platform");
        let spec = ToolSpec::builtin("air").expect("air is built in");
        let ctx = InstallContext {
            label: "R formatter",
            command: "togi format",
            verbose: true,
        };

        let binary = Downloader::new(cache, platform)
            .ensure_installed(&spec, spec.default_version, &ctx)
            .expect("download and install air");
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .expect("run air --version");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(spec.default_version),
            "air --version must report {}: {stdout}",
            spec.default_version
        );
    }
}
