//! Where installed tools live on disk:
//! `<data_dir>/tools/<name>/<version>/<binary>` plus a `manifest.json` next
//! to each installed binary.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::term::HintExt;
use crate::tools::manifest::Manifest;
use crate::tools::platform::Platform;

/// One completed install found in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledTool {
    pub name: String,
    pub version: String,
    /// Parsed `manifest.json`; `None` when the file is corrupt (the
    /// install itself is still listed so the user can see and clean it).
    pub manifest: Option<Manifest>,
}

/// The on-disk tool cache, rooted at `<data_dir>/tools`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCache {
    root: PathBuf,
}

impl ToolCache {
    /// Cache under the platform data directory for togi
    /// (`~/Library/Application Support/togi`, `~/.local/share/togi`,
    /// `%APPDATA%\togi`), honoring the internal `TOGI_DATA_DIR` override
    /// used for test isolation.
    pub fn from_env() -> anyhow::Result<ToolCache> {
        match data_dir(std::env::var_os("TOGI_DATA_DIR")) {
            Some(dir) => Ok(ToolCache::at(&dir)),
            None => Err(anyhow::anyhow!(
                "could not determine a data directory for togi tool installs"
            ))
            .hint(
                "make sure your home directory is set (HOME on macOS/Linux, \
                 APPDATA on Windows)",
            ),
        }
    }

    /// Cache rooted under an explicit togi data directory.
    pub fn at(data_dir: &Path) -> ToolCache {
        ToolCache {
            root: data_dir.join("tools"),
        }
    }

    /// `<data_dir>/tools`: parent of every per-tool directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<data_dir>/tools/<name>/<version>`: one installed tool version.
    pub fn tool_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(name).join(version)
    }

    /// The installed binary for one tool version on `platform`.
    pub fn binary_path(&self, name: &str, version: &str, platform: Platform) -> PathBuf {
        self.tool_dir(name, version)
            .join(platform.binary_name(name))
    }

    /// The `manifest.json` describing one installed tool version.
    pub fn manifest_path(&self, name: &str, version: &str) -> PathBuf {
        self.tool_dir(name, version).join("manifest.json")
    }

    /// Every completed install in the cache: a `<name>/<version>` directory
    /// holding a `manifest.json` (the manifest is written last, so its
    /// presence marks a finished install). Sorted by name, then version
    /// ascending, so per tool the newest install comes last. An absent
    /// cache is simply empty. Requires no network.
    pub fn installed(&self) -> anyhow::Result<Vec<InstalledTool>> {
        let mut found = Vec::new();
        for name_dir in read_subdirs(&self.root)? {
            let name = match name_dir.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue, // non-UTF-8 stray; not one of ours
            };
            for version_dir in read_subdirs(&name_dir)? {
                let version = match version_dir.file_name().and_then(|v| v.to_str()) {
                    Some(version) => version.to_string(),
                    None => continue,
                };
                let manifest_path = self.manifest_path(&name, &version);
                if !manifest_path.is_file() {
                    continue; // interrupted install; not completed
                }
                found.push(InstalledTool {
                    manifest: Manifest::load(&manifest_path).ok(),
                    name: name.clone(),
                    version,
                });
            }
        }
        found.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| compare_versions(&a.version, &b.version))
        });
        Ok(found)
    }
}

/// The subdirectories of `dir`; empty when `dir` does not exist.
fn read_subdirs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("could not read the tool cache at `{}`", dir.display()))
        .hint("check the directory's permissions, or run `togi tools clean` to reset it")?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry
            .with_context(|| format!("could not read the tool cache at `{}`", dir.display()))
            .hint("check the directory's permissions, or run `togi tools clean` to reset it")?;
        if entry.path().is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// Order versions numerically where possible (`0.9.0` < `0.10.0`), falling
/// back to a lexicographic tiebreak for non-numeric parts.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let nums = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    nums(a).cmp(&nums(b)).then_with(|| a.cmp(b))
}

/// The togi data directory: `override_dir` when set (the internal
/// `TOGI_DATA_DIR` escape hatch, mirroring `TOGI_CONFIG_DIR` for config),
/// else the platform location from `directories`. `None` when no home
/// directory can be determined.
fn data_dir(override_dir: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "togi").map(|dirs| dirs.data_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::platform::{Arch, Os};

    #[test]
    fn layout_is_tools_name_version_under_the_data_dir() {
        let data = Path::new("data");
        let cache = ToolCache::at(data);
        assert_eq!(cache.root(), data.join("tools"));
        assert_eq!(
            cache.tool_dir("air", "0.10.0"),
            data.join("tools").join("air").join("0.10.0")
        );
    }

    #[test]
    fn binary_path_uses_the_platform_binary_name() {
        let cache = ToolCache::at(Path::new("data"));
        let unix = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let windows = Platform {
            os: Os::Windows,
            arch: Arch::Aarch64,
        };
        assert_eq!(
            cache.binary_path("ruff", "0.14.0", unix),
            cache.tool_dir("ruff", "0.14.0").join("ruff")
        );
        assert_eq!(
            cache.binary_path("ruff", "0.14.0", windows),
            cache.tool_dir("ruff", "0.14.0").join("ruff.exe")
        );
    }

    #[test]
    fn manifest_lives_next_to_the_binary() {
        let cache = ToolCache::at(Path::new("data"));
        assert_eq!(
            cache.manifest_path("air", "0.10.0"),
            cache.tool_dir("air", "0.10.0").join("manifest.json")
        );
    }

    #[test]
    fn data_dir_override_wins() {
        let dir = data_dir(Some("/isolated/togi-data".into())).expect("override is set");
        assert_eq!(dir, PathBuf::from("/isolated/togi-data"));
    }

    /// Lay one completed install (binary + manifest) into `cache`.
    fn fake_install(cache: &ToolCache, name: &str, version: &str) {
        let dir = cache.tool_dir(name, version);
        std::fs::create_dir_all(&dir).expect("create tool dir");
        std::fs::write(dir.join(name), b"fake").expect("write binary");
        Manifest::new(
            version.to_string(),
            format!("https://example.test/{name}.tar.gz"),
            None,
        )
        .save(&cache.manifest_path(name, version))
        .expect("write manifest");
    }

    #[test]
    fn installed_lists_completed_installs_sorted_by_name_then_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        fake_install(&cache, "ruff", "0.14.0");
        fake_install(&cache, "air", "0.10.0");
        fake_install(&cache, "air", "0.9.0");

        let installed = cache.installed().expect("list installs");
        let summary: Vec<(String, String)> = installed
            .iter()
            .map(|t| (t.name.clone(), t.version.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("air".to_string(), "0.9.0".to_string()),
                ("air".to_string(), "0.10.0".to_string()),
                ("ruff".to_string(), "0.14.0".to_string()),
            ]
        );
        assert!(installed.iter().all(|t| t.manifest.is_some()));
    }

    #[test]
    fn installed_is_empty_when_the_cache_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(&dir.path().join("never-created"));
        assert_eq!(cache.installed().expect("list installs"), Vec::new());
    }

    #[test]
    fn installed_skips_version_dirs_without_a_manifest() {
        // A version directory without a manifest is an interrupted
        // install, not an installed tool.
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        std::fs::create_dir_all(cache.tool_dir("air", "0.10.0")).expect("create tool dir");
        assert_eq!(cache.installed().expect("list installs"), Vec::new());
    }

    #[test]
    fn installed_ignores_lock_and_staging_files_in_the_tool_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        fake_install(&cache, "air", "0.10.0");
        // The advisory lock file lives next to version dirs.
        std::fs::write(cache.root().join("air").join(".lock"), b"").expect("write lock");

        let installed = cache.installed().expect("list installs");
        assert_eq!(installed.len(), 1);
    }

    #[test]
    fn installed_lists_corrupt_manifests_with_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let tool_dir = cache.tool_dir("air", "0.10.0");
        std::fs::create_dir_all(&tool_dir).expect("create tool dir");
        std::fs::write(tool_dir.join("manifest.json"), "not json").expect("write manifest");

        let installed = cache.installed().expect("list installs");
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].manifest, None);
    }

    #[test]
    fn versions_compare_numerically_not_lexicographically() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("0.9.0", "0.10.0"), Ordering::Less);
        assert_eq!(compare_versions("0.10.0", "0.10.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "0.99.99"), Ordering::Greater);
    }

    #[test]
    fn data_dir_falls_back_to_the_platform_location() {
        // On dev/CI machines a home directory always exists. The exact path
        // is platform-specific, but it is always inside an `togi` directory.
        let dir = data_dir(None).expect("platform data dir");
        assert!(dir.iter().any(|part| part == "togi"), "{}", dir.display());
    }
}
