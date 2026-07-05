//! Platform detection for tool downloads: which OS/arch this process runs
//! on, and how each platform shows up in GitHub release asset names.

use thiserror::Error;

/// Operating systems togi ships managed tools for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

/// CPU architectures togi ships managed tools for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

/// One supported OS/arch pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

/// This machine is not one of the six supported OS/arch pairs.
#[derive(Debug, Error)]
#[error(
    "togi cannot manage tool installs on `{os}/{arch}`; supported platforms are \
     x86_64 and aarch64 macOS, Linux, and Windows — install the tools yourself \
     and put them on PATH"
)]
pub struct UnsupportedPlatform {
    os: String,
    arch: String,
}

impl Os {
    /// How this OS appears in release asset names (the vendor-OS part of the
    /// Rust target triple, which the tools we manage all use).
    pub fn asset_str(self) -> &'static str {
        match self {
            Os::Mac => "apple-darwin",
            Os::Linux => "unknown-linux-gnu",
            Os::Windows => "pc-windows-msvc",
        }
    }

    /// Archive format the tools publish for this OS.
    pub fn archive_ext(self) -> &'static str {
        match self {
            Os::Mac | Os::Linux => "tar.gz",
            Os::Windows => "zip",
        }
    }

    /// Executable filename suffix on this OS.
    pub fn exe_suffix(self) -> &'static str {
        match self {
            Os::Mac | Os::Linux => "",
            Os::Windows => ".exe",
        }
    }
}

impl Arch {
    /// How this architecture appears in release asset names.
    pub fn asset_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The Go-ecosystem spelling used by some projects' release assets
    /// (gh, duckdb): `amd64`/`arm64`.
    pub fn alt_asset_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }
}

impl Platform {
    /// All six supported OS/arch pairs.
    pub const ALL: [Platform; 6] = [
        Platform {
            os: Os::Mac,
            arch: Arch::X86_64,
        },
        Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        },
        Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        },
        Platform {
            os: Os::Linux,
            arch: Arch::Aarch64,
        },
        Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        },
        Platform {
            os: Os::Windows,
            arch: Arch::Aarch64,
        },
    ];

    /// Detect the platform this process is running on.
    pub fn current() -> Result<Platform, UnsupportedPlatform> {
        Platform::from_parts(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Build a platform from `std::env::consts::OS` / `ARCH` style names.
    pub fn from_parts(os: &str, arch: &str) -> Result<Platform, UnsupportedPlatform> {
        let unsupported = || UnsupportedPlatform {
            os: os.to_string(),
            arch: arch.to_string(),
        };
        let os_kind = match os {
            "macos" => Os::Mac,
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            _ => return Err(unsupported()),
        };
        let arch_kind = match arch {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            _ => return Err(unsupported()),
        };
        Ok(Platform {
            os: os_kind,
            arch: arch_kind,
        })
    }

    /// Filename of a tool binary on this platform (`air` / `air.exe`).
    pub fn binary_name(self, tool: &str) -> String {
        format!("{tool}{}", self.os.exe_suffix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_all_six_supported_tuples() {
        let cases = [
            ("macos", "x86_64", Os::Mac, Arch::X86_64),
            ("macos", "aarch64", Os::Mac, Arch::Aarch64),
            ("linux", "x86_64", Os::Linux, Arch::X86_64),
            ("linux", "aarch64", Os::Linux, Arch::Aarch64),
            ("windows", "x86_64", Os::Windows, Arch::X86_64),
            ("windows", "aarch64", Os::Windows, Arch::Aarch64),
        ];
        for (os, arch, want_os, want_arch) in cases {
            let platform = Platform::from_parts(os, arch)
                .unwrap_or_else(|_| panic!("{os}/{arch} must be supported"));
            assert_eq!(platform.os, want_os, "{os}/{arch}");
            assert_eq!(platform.arch, want_arch, "{os}/{arch}");
        }
    }

    #[test]
    fn rejects_unsupported_os_or_arch_with_guidance() {
        for (os, arch) in [("freebsd", "x86_64"), ("linux", "riscv64"), ("", "")] {
            let err = Platform::from_parts(os, arch).expect_err("must be unsupported");
            let message = err.to_string();
            assert!(message.contains("supported platforms"), "{message}");
            assert!(message.contains("PATH"), "says what to do next: {message}");
        }
    }

    #[test]
    fn current_platform_is_supported_here() {
        // The dev/CI machines this test runs on are all in the supported set.
        Platform::current().expect("current platform must be detected");
    }

    #[test]
    fn asset_strings_match_release_naming() {
        assert_eq!(Os::Mac.asset_str(), "apple-darwin");
        assert_eq!(Os::Linux.asset_str(), "unknown-linux-gnu");
        assert_eq!(Os::Windows.asset_str(), "pc-windows-msvc");
        assert_eq!(Arch::X86_64.asset_str(), "x86_64");
        assert_eq!(Arch::Aarch64.asset_str(), "aarch64");
    }

    #[test]
    fn alt_arch_strings_match_go_style_release_naming() {
        assert_eq!(Arch::X86_64.alt_asset_str(), "amd64");
        assert_eq!(Arch::Aarch64.alt_asset_str(), "arm64");
    }

    #[test]
    fn archives_are_tarballs_except_zip_on_windows() {
        assert_eq!(Os::Mac.archive_ext(), "tar.gz");
        assert_eq!(Os::Linux.archive_ext(), "tar.gz");
        assert_eq!(Os::Windows.archive_ext(), "zip");
    }

    #[test]
    fn binary_name_appends_exe_only_on_windows() {
        for platform in Platform::ALL {
            let name = platform.binary_name("air");
            match platform.os {
                Os::Windows => assert_eq!(name, "air.exe"),
                _ => assert_eq!(name, "air"),
            }
        }
    }
}
