//! Managed tool installs: which tools togi knows about, where their
//! binaries live on disk, what platform archive to fetch for each, and the
//! downloader that installs them.
//!
//! Terminal output (progress bars, warnings) always goes through `term`.

// NOTE: dead_code allowed per module: these modules form the crate's
// tool-management API, and within this crate only the `togi tools`
// subcommands and unit tests consume it, so parts of the surface have no
// non-test callers.
#[allow(dead_code)]
mod cache;
#[allow(dead_code)]
mod download;
#[allow(dead_code)]
mod manifest;
#[allow(dead_code)]
mod platform;
#[allow(dead_code)]
mod spec;
#[cfg(test)]
pub(crate) mod test_support;
#[allow(dead_code)]
mod uv_tool;
#[allow(dead_code)]
pub mod versions;

use std::path::PathBuf;

use crate::config::ToolsConfig;

// NOTE: unused_imports allowed: these re-exports are the module's public
// surface, and within this crate some of them have only unit-test callers.
#[allow(unused_imports)]
pub use cache::{InstalledTool, ToolCache};
#[allow(unused_imports)]
pub use download::{Downloader, InstallContext};
#[allow(unused_imports)]
pub use download::{extract_binary, github_agent};
#[allow(unused_imports)]
pub use manifest::Manifest;
#[allow(unused_imports)]
pub use platform::{Arch, Os, Platform, UnsupportedPlatform};
#[allow(unused_imports)]
pub use spec::{ToolKind, ToolSpec};
#[allow(unused_imports)]
pub use uv_tool::UvToolInstaller;

/// Progress-bar label for each managed tool (tool names themselves appear
/// only at `-v`).
pub fn label_for(name: &str) -> &'static str {
    match name {
        "air" => "R formatter",
        "ruff" => "Python formatter/linter",
        "panache" => "Markdown formatter",
        "sqlfluff" => "SQL formatter/linter",
        "uv" => "uv (Python tool installer)",
        _ => "tool",
    }
}

/// The version of `spec` a run should use: the `[tools]` pin from config
/// when there is one, else the default baked into this togi release.
///
/// Every code path that resolves a tool version goes through here, so pins
/// win everywhere or nowhere.
pub fn resolve_version<'a>(tools: &'a ToolsConfig, spec: &ToolSpec) -> &'a str {
    tools
        .pins
        .get(spec.name)
        .map(String::as_str)
        .unwrap_or(spec.default_version)
}

/// Install `spec` at its resolved version (config pin, else baked default)
/// into the default cache for this machine and return the path to its
/// binary, dispatching on the tool's kind so callers never care how a tool
/// is installed. Cached tools are returned with zero network.
pub fn ensure_installed(
    spec: &ToolSpec,
    tools: &ToolsConfig,
    ctx: &InstallContext,
) -> anyhow::Result<PathBuf> {
    let version = resolve_version(tools, spec);
    let cache = ToolCache::from_env()?;
    let platform = Platform::current()?;
    match spec.kind {
        ToolKind::GithubBinary { .. } => {
            Downloader::new(cache, platform).ensure_installed(spec, version, ctx)
        }
        ToolKind::UvTool { .. } => {
            // The private uv bootstrap honors a `[tools] uv` pin too.
            let uv = ToolSpec::builtin("uv").expect("uv is a built-in tool");
            let uv_version = resolve_version(tools, &uv).to_string();
            UvToolInstaller::new(cache, platform, uv_version).ensure_installed(spec, version, ctx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn every_builtin_tool_has_a_human_label() {
        for spec in ToolSpec::builtins() {
            assert_ne!(label_for(spec.name), "tool", "{} needs a label", spec.name);
        }
    }

    #[test]
    fn resolve_version_prefers_the_config_pin() {
        let air = ToolSpec::builtin("air").expect("air is built in");
        let tools = ToolsConfig {
            pins: BTreeMap::from([("air".to_string(), "0.11.0".to_string())]),
            args: BTreeMap::new(),
        };
        assert_eq!(resolve_version(&tools, &air), "0.11.0");
    }

    #[test]
    fn resolve_version_falls_back_to_the_baked_default() {
        let air = ToolSpec::builtin("air").expect("air is built in");
        assert_eq!(
            resolve_version(&ToolsConfig::default(), &air),
            air.default_version
        );
    }

    #[test]
    fn resolve_version_ignores_pins_for_other_tools() {
        let ruff = ToolSpec::builtin("ruff").expect("ruff is built in");
        let tools = ToolsConfig {
            pins: BTreeMap::from([("air".to_string(), "0.11.0".to_string())]),
            args: BTreeMap::new(),
        };
        assert_eq!(resolve_version(&tools, &ruff), ruff.default_version);
    }
}
