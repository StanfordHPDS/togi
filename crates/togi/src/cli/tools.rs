//! `togi tools` — manage togi-installed formatter/linter tools.
//!
//! `list` and `clean` work entirely offline against the tool cache;
//! `update` needs network only for tools whose wanted version (config pin,
//! else baked default) is not cached yet, and a failure on one tool never
//! stops the others.

use std::path::Path;

use anyhow::Context;
use clap::{Args, Subcommand};

use togi_core::config::{self, Layer, ToolsConfig};
use togi_core::term;
use togi_core::term::HintExt;
use togi_core::tools::{
    self, InstallContext, InstalledTool, Platform, ToolCache, ToolKind, ToolSpec,
};

#[derive(Debug, Args)]
pub struct ToolsArgs {
    #[command(subcommand)]
    pub command: ToolsCommand,
}

#[derive(Debug, Subcommand)]
pub enum ToolsCommand {
    /// List installed tools and their versions
    ///
    /// Prints each installed tool version from the cache (name, version,
    /// source, install date) plus the baked default versions for managed
    /// tools that are not installed. Never touches the network.
    List,
    /// Refresh tools to release defaults or config pins
    ///
    /// For each managed tool, resolves the wanted version (config pin, else
    /// this release's default) and installs it if it is not already cached.
    /// A failure on one tool is reported and the run continues.
    Update,
    /// Remove the tool cache
    ///
    /// Deletes the entire managed-tool cache and reports the space freed;
    /// tools are re-downloaded on next use. Prompts for confirmation unless
    /// --yes is given.
    Clean {
        /// Delete without asking for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

pub fn run(args: ToolsArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    match args.command {
        ToolsCommand::List => list(),
        ToolsCommand::Update => update(global),
        ToolsCommand::Clean { yes } => clean(yes),
    }
}

/// `togi tools list`: every installed tool version from the cache (name,
/// version, source kind, install date), plus the baked default versions
/// for managed tools that are not installed. Never touches the network.
fn list() -> anyhow::Result<()> {
    let cache = ToolCache::from_env()?;
    let installed = cache.installed()?;

    for spec in ToolSpec::builtins() {
        let versions: Vec<&InstalledTool> = installed
            .iter()
            .filter(|tool| tool.name == spec.name)
            .collect();
        if versions.is_empty() {
            term::println(&format!(
                "{:<10} not installed (default {})",
                spec.name, spec.default_version
            ));
            continue;
        }
        for tool in versions {
            term::println(&installed_line(tool, kind_str(spec.kind)));
        }
    }

    // Leftovers from other togi releases still take up space; show them so
    // `togi tools clean` is an informed decision.
    for tool in &installed {
        if ToolSpec::builtin(&tool.name).is_none() {
            term::println(&installed_line(tool, "unknown"));
        }
    }
    Ok(())
}

/// One `list` line for an installed tool version.
fn installed_line(tool: &InstalledTool, kind: &str) -> String {
    let date = tool
        .manifest
        .as_ref()
        .map(|manifest| install_date(&manifest.installed_at))
        .unwrap_or_else(|| "unknown (corrupt manifest; run `togi tools clean`)".to_string());
    format!(
        "{:<10} {:<10} {:<16} installed {date}",
        tool.name, tool.version, kind
    )
}

/// The date half of an RFC 3339 timestamp (`2026-07-02T12:00:00Z` →
/// `2026-07-02`); the full string when it is too short to split.
fn install_date(installed_at: &str) -> String {
    installed_at.get(..10).unwrap_or(installed_at).to_string()
}

/// Human name for how a tool is installed.
fn kind_str(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::GithubBinary { .. } => "github release",
        ToolKind::UvTool { .. } => "uv (PyPI)",
    }
}

/// `togi tools update`: for each managed tool, resolve the wanted version
/// (config pin, else baked default) and install it if it is not cached.
/// A failure on one tool is reported and the run continues; the command
/// fails at the end if anything failed.
fn update(global: &super::GlobalArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let loaded = config::load(&cwd, global.config.as_deref(), Layer::default())?;
    for warning in &loaded.warnings {
        term::warn(warning);
    }

    let cache = ToolCache::from_env()?;
    let platform = Platform::current()?;
    let installed = cache.installed()?;

    let mut failed = 0usize;
    for spec in ToolSpec::builtins() {
        match update_one(
            &spec,
            &loaded.config.tools,
            &cache,
            platform,
            &installed,
            global,
        ) {
            Ok(report) => term::success(&report),
            Err(err) => {
                failed += 1;
                term::error(&err.context(format!("could not update {}", spec.name)));
            }
        }
    }

    if failed > 0 {
        return Err(anyhow::anyhow!(
            "{failed} of {} tools failed to update",
            ToolSpec::builtins().len()
        ))
        .hint(
            "see the errors above; `togi tools update` needs network access \
             for tools that are not cached yet",
        );
    }
    Ok(())
}

/// Bring one tool to its wanted version; the returned string is the
/// per-tool report line (up to date / updated X -> Y / installed Y).
fn update_one(
    spec: &ToolSpec,
    tools_config: &ToolsConfig,
    cache: &ToolCache,
    platform: Platform,
    installed: &[InstalledTool],
    global: &super::GlobalArgs,
) -> anyhow::Result<String> {
    let wanted = tools::resolve_version(tools_config, spec).to_string();

    let cached = cache.binary_path(spec.name, &wanted, platform).is_file()
        && cache.manifest_path(spec.name, &wanted).is_file();
    if cached {
        return Ok(format!("{} {wanted} is up to date", spec.name));
    }

    // `installed` is sorted version-ascending per tool, so the last entry
    // is the newest version already on disk.
    let previous = installed
        .iter()
        .rfind(|tool| tool.name == spec.name)
        .map(|tool| tool.version.clone());

    let ctx = InstallContext {
        label: tools::label_for(spec.name),
        command: "togi tools update",
        verbose: global.verbose,
    };
    tools::ensure_installed(spec, tools_config, &ctx)?;

    Ok(match previous {
        Some(old) => format!("{} updated {old} -> {wanted}", spec.name),
        None => format!("{} installed {wanted}", spec.name),
    })
}

/// `togi tools clean`: delete the whole tool cache directory (after a
/// confirmation prompt unless `--yes`) and report the bytes freed. Never
/// touches the network.
fn clean(yes: bool) -> anyhow::Result<()> {
    let cache = ToolCache::from_env()?;
    let root = cache.root();
    if !root.exists() {
        term::println("nothing to clean: the tool cache is empty");
        return Ok(());
    }

    let bytes = dir_size(root);
    if !yes {
        let prompt = format!(
            "Delete the togi tool cache at {} ({})?",
            root.display(),
            human_bytes(bytes)
        );
        // The generic prompt refusal points at "the flag that answers this
        // prompt"; here that flag deserves to be named outright.
        let confirmed = match term::confirm(&prompt, false) {
            Ok(answer) => answer,
            Err(err) => {
                return Err(anyhow::anyhow!("{err}"))
                    .hint("pass `--yes` (`togi tools clean --yes`) to delete without a prompt");
            }
        };
        if !confirmed {
            term::println("nothing deleted");
            return Ok(());
        }
    }

    std::fs::remove_dir_all(root)
        .with_context(|| format!("could not delete the tool cache at `{}`", root.display()))
        .hint("close other togi processes and check the directory's permissions, then retry")?;
    term::success(&format!("tool cache deleted, freed {}", human_bytes(bytes)));
    Ok(())
}

/// Total size in bytes of every file under `dir` (best effort: unreadable
/// entries count as zero rather than failing the cleanup).
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                dir_size(&path)
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// `1234` → `1.2 KB`; whole bytes below 1 KB.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use togi_core::tools::Manifest;

    #[test]
    fn human_bytes_scales_through_the_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(532), "532 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a").join("b")).expect("create dirs");
        std::fs::write(dir.path().join("top.bin"), vec![0u8; 100]).expect("write");
        std::fs::write(dir.path().join("a").join("mid.bin"), vec![0u8; 50]).expect("write");
        std::fs::write(
            dir.path().join("a").join("b").join("leaf.bin"),
            vec![0u8; 7],
        )
        .expect("write");
        assert_eq!(dir_size(dir.path()), 157);
    }

    #[test]
    fn dir_size_of_a_missing_dir_is_zero() {
        assert_eq!(dir_size(Path::new("no/such/dir")), 0);
    }

    #[test]
    fn installed_line_holds_name_version_kind_and_date() {
        let tool = InstalledTool {
            name: "air".to_string(),
            version: "0.10.0".to_string(),
            manifest: Some(Manifest {
                version: "0.10.0".to_string(),
                source_url: "https://example.test/air.tar.gz".to_string(),
                checksum: None,
                installed_at: "2026-07-02T12:00:00Z".to_string(),
            }),
        };
        let line = installed_line(&tool, "github release");
        assert!(line.contains("air"), "{line}");
        assert!(line.contains("0.10.0"), "{line}");
        assert!(line.contains("github release"), "{line}");
        assert!(line.contains("installed 2026-07-02"), "{line}");
        assert!(!line.contains("12:00:00"), "date only: {line}");
    }

    #[test]
    fn installed_line_flags_a_corrupt_manifest() {
        let tool = InstalledTool {
            name: "air".to_string(),
            version: "0.10.0".to_string(),
            manifest: None,
        };
        let line = installed_line(&tool, "github release");
        assert!(line.contains("corrupt manifest"), "{line}");
        assert!(line.contains("togi tools clean"), "{line}");
    }

    #[test]
    fn install_date_survives_malformed_timestamps() {
        assert_eq!(install_date("2026-07-02T12:00:00Z"), "2026-07-02");
        assert_eq!(install_date("bogus"), "bogus");
        assert_eq!(install_date(""), "");
    }

    #[test]
    fn kind_str_names_both_install_paths() {
        let air = ToolSpec::builtin("air").expect("air is built in");
        let sqlfluff = ToolSpec::builtin("sqlfluff").expect("sqlfluff is built in");
        assert_eq!(kind_str(air.kind), "github release");
        assert_eq!(kind_str(sqlfluff.kind), "uv (PyPI)");
    }
}
