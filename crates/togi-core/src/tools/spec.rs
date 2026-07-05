//! What togi knows about each managed tool: where it comes from and how its
//! release assets are named per platform.

use crate::tools::platform::Platform;
use crate::tools::versions;

/// One managed tool: its name, the version baked into this togi release,
/// and how to install it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub default_version: &'static str,
    pub kind: ToolKind,
}

/// How a tool is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// A prebuilt binary from a GitHub release. Patterns may use the
    /// placeholders `{version}`, `{arch}`, `{os}`, and `{ext}`.
    GithubBinary {
        /// `owner/repo` on GitHub.
        repo: &'static str,
        /// Release asset filename pattern for the archive.
        asset_pattern: &'static str,
        /// Release asset filename pattern for the sha256 checksum, when the
        /// project publishes one (verification is skipped with a warning
        /// when it does not).
        checksum_pattern: Option<&'static str>,
    },
    /// A Python tool installed via `uv tool install` into an togi-owned
    /// tool directory.
    UvTool {
        /// PyPI package name.
        package: &'static str,
    },
}

impl ToolSpec {
    /// The release archive filename for `version` on `platform`;
    /// `None` for tools that are not GitHub binaries.
    pub fn asset_name(&self, platform: Platform, version: &str) -> Option<String> {
        match self.kind {
            ToolKind::GithubBinary { asset_pattern, .. } => {
                Some(resolve_pattern(asset_pattern, platform, version))
            }
            ToolKind::UvTool { .. } => None,
        }
    }

    /// The checksum asset filename for `version` on `platform`; `None` for
    /// non-GitHub tools and for tools without published checksums.
    pub fn checksum_asset_name(&self, platform: Platform, version: &str) -> Option<String> {
        match self.kind {
            ToolKind::GithubBinary {
                checksum_pattern, ..
            } => checksum_pattern.map(|pattern| resolve_pattern(pattern, platform, version)),
            ToolKind::UvTool { .. } => None,
        }
    }

    /// All tools this togi release knows how to install.
    pub fn builtins() -> [ToolSpec; 5] {
        [
            ToolSpec {
                name: "air",
                default_version: versions::AIR,
                kind: ToolKind::GithubBinary {
                    repo: "posit-dev/air",
                    asset_pattern: "air-{arch}-{os}.{ext}",
                    checksum_pattern: Some("air-{arch}-{os}.{ext}.sha256"),
                },
            },
            ToolSpec {
                name: "ruff",
                default_version: versions::RUFF,
                kind: ToolKind::GithubBinary {
                    repo: "astral-sh/ruff",
                    asset_pattern: "ruff-{arch}-{os}.{ext}",
                    checksum_pattern: Some("ruff-{arch}-{os}.{ext}.sha256"),
                },
            },
            ToolSpec {
                name: "panache",
                default_version: versions::PANACHE,
                kind: ToolKind::GithubBinary {
                    repo: "jolars/panache",
                    asset_pattern: "panache-{arch}-{os}.{ext}",
                    checksum_pattern: Some("panache-{arch}-{os}.{ext}.sha256"),
                },
            },
            ToolSpec {
                name: "sqlfluff",
                default_version: versions::SQLFLUFF,
                kind: ToolKind::UvTool {
                    package: "sqlfluff",
                },
            },
            ToolSpec {
                name: "uv",
                default_version: versions::UV,
                kind: ToolKind::GithubBinary {
                    repo: "astral-sh/uv",
                    asset_pattern: "uv-{arch}-{os}.{ext}",
                    checksum_pattern: Some("uv-{arch}-{os}.{ext}.sha256"),
                },
            },
        ]
    }

    /// Look up a built-in tool by name.
    pub fn builtin(name: &str) -> Option<ToolSpec> {
        ToolSpec::builtins().into_iter().find(|s| s.name == name)
    }
}

/// Expand `{version}`, `{arch}`, `{alt-arch}`, `{os}`, and `{ext}` in an
/// asset pattern.
fn resolve_pattern(pattern: &str, platform: Platform, version: &str) -> String {
    pattern
        .replace("{version}", version)
        .replace("{alt-arch}", platform.arch.alt_asset_str())
        .replace("{arch}", platform.arch.asset_str())
        .replace("{os}", platform.os.asset_str())
        .replace("{ext}", platform.os.archive_ext())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::platform::{Arch, Os};

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            kind: ToolKind::GithubBinary {
                repo: "example/tool",
                asset_pattern: "tool-{arch}-{os}.{ext}",
                checksum_pattern: Some("tool-{arch}-{os}.{ext}.sha256"),
            },
        }
    }

    #[test]
    fn resolves_asset_names_across_all_six_platform_tuples() {
        let cases = [
            (Os::Mac, Arch::X86_64, "tool-x86_64-apple-darwin.tar.gz"),
            (Os::Mac, Arch::Aarch64, "tool-aarch64-apple-darwin.tar.gz"),
            (
                Os::Linux,
                Arch::X86_64,
                "tool-x86_64-unknown-linux-gnu.tar.gz",
            ),
            (
                Os::Linux,
                Arch::Aarch64,
                "tool-aarch64-unknown-linux-gnu.tar.gz",
            ),
            (Os::Windows, Arch::X86_64, "tool-x86_64-pc-windows-msvc.zip"),
            (
                Os::Windows,
                Arch::Aarch64,
                "tool-aarch64-pc-windows-msvc.zip",
            ),
        ];
        for (os, arch, want) in cases {
            let platform = Platform { os, arch };
            assert_eq!(
                tool().asset_name(platform, "1.2.3").as_deref(),
                Some(want),
                "{os:?}/{arch:?}"
            );
        }
    }

    #[test]
    fn resolves_checksum_asset_names() {
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        assert_eq!(
            tool().checksum_asset_name(platform, "1.2.3").as_deref(),
            Some("tool-x86_64-unknown-linux-gnu.tar.gz.sha256")
        );
    }

    #[test]
    fn substitutes_version_placeholder() {
        let spec = ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            kind: ToolKind::GithubBinary {
                repo: "example/tool",
                asset_pattern: "tool-{version}-{arch}-{os}.{ext}",
                checksum_pattern: None,
            },
        };
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        assert_eq!(
            spec.asset_name(platform, "9.9.9").as_deref(),
            Some("tool-9.9.9-aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(spec.checksum_asset_name(platform, "9.9.9"), None);
    }

    #[test]
    fn substitutes_alt_arch_placeholder_with_go_style_names() {
        // Tools from the Go ecosystem (gh, duckdb) name assets with
        // `amd64`/`arm64` instead of the Rust-triple arch names.
        let spec = ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            kind: ToolKind::GithubBinary {
                repo: "example/tool",
                asset_pattern: "tool_{version}_linux_{alt-arch}.tar.gz",
                checksum_pattern: None,
            },
        };
        let cases = [
            (Arch::X86_64, "tool_1.2.3_linux_amd64.tar.gz"),
            (Arch::Aarch64, "tool_1.2.3_linux_arm64.tar.gz"),
        ];
        for (arch, want) in cases {
            let platform = Platform {
                os: Os::Linux,
                arch,
            };
            assert_eq!(
                spec.asset_name(platform, "1.2.3").as_deref(),
                Some(want),
                "{arch:?}"
            );
        }
    }

    #[test]
    fn uv_tools_have_no_release_assets() {
        let spec = ToolSpec {
            name: "sqlfluff",
            default_version: "1.0.0",
            kind: ToolKind::UvTool {
                package: "sqlfluff",
            },
        };
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        assert_eq!(spec.asset_name(platform, "1.0.0"), None);
        assert_eq!(spec.checksum_asset_name(platform, "1.0.0"), None);
    }

    #[test]
    fn builtins_cover_the_five_managed_tools() {
        let names: Vec<&str> = ToolSpec::builtins().iter().map(|s| s.name).collect();
        assert_eq!(names, ["air", "ruff", "panache", "sqlfluff", "uv"]);
    }

    #[test]
    fn builtin_github_tools_point_at_their_repos() {
        for (name, want_repo, want_version) in [
            ("air", "posit-dev/air", versions::AIR),
            ("ruff", "astral-sh/ruff", versions::RUFF),
            ("panache", "jolars/panache", versions::PANACHE),
            ("uv", "astral-sh/uv", versions::UV),
        ] {
            let spec = ToolSpec::builtin(name).expect(name);
            assert_eq!(spec.default_version, want_version, "{name}");
            match spec.kind {
                ToolKind::GithubBinary { repo, .. } => assert_eq!(repo, want_repo, "{name}"),
                other => panic!("{name} must be a GithubBinary, got {other:?}"),
            }
        }
    }

    #[test]
    fn builtin_sqlfluff_installs_via_uv() {
        let spec = ToolSpec::builtin("sqlfluff").expect("sqlfluff");
        assert_eq!(spec.default_version, versions::SQLFLUFF);
        assert_eq!(
            spec.kind,
            ToolKind::UvTool {
                package: "sqlfluff"
            }
        );
    }

    #[test]
    fn builtin_asset_patterns_resolve_on_every_platform() {
        // Each GitHub tool's patterns must expand cleanly (no leftover
        // placeholders) on every supported platform.
        for spec in ToolSpec::builtins() {
            let ToolKind::GithubBinary { .. } = spec.kind else {
                continue;
            };
            for platform in Platform::ALL {
                let asset = spec
                    .asset_name(platform, spec.default_version)
                    .expect("github tools resolve an asset");
                assert!(!asset.contains(['{', '}']), "{}: {asset}", spec.name);
                assert!(
                    asset.ends_with(platform.os.archive_ext()),
                    "{}: {asset}",
                    spec.name
                );
                if let Some(checksum) = spec.checksum_asset_name(platform, spec.default_version) {
                    assert!(!checksum.contains(['{', '}']), "{}: {checksum}", spec.name);
                }
            }
        }
    }

    #[test]
    fn unknown_builtin_is_none() {
        assert_eq!(ToolSpec::builtin("black"), None);
    }
}
