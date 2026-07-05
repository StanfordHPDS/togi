//! Release-pinned default tool versions.
//!
//! These are the versions togi installs when a project pins nothing in
//! `togi.toml`. They are updated (and re-verified against the upstream
//! releases) as part of cutting a togi release, not at runtime.

pub const AIR: &str = "0.10.0";
pub const RUFF: &str = "0.14.0";
pub const PANACHE: &str = "2.60.0";
pub const SQLFLUFF: &str = "3.4.0";
pub const UV: &str = "0.9.5";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_versions_are_bare_semver() {
        // Bare `X.Y.Z` — no `v` prefix, so they can drop straight into
        // asset patterns and `uv tool install pkg==X.Y.Z`.
        for (name, version) in [
            ("air", AIR),
            ("ruff", RUFF),
            ("panache", PANACHE),
            ("sqlfluff", SQLFLUFF),
            ("uv", UV),
        ] {
            let parts: Vec<&str> = version.split('.').collect();
            assert_eq!(parts.len(), 3, "{name}: {version}");
            for part in parts {
                part.parse::<u64>()
                    .unwrap_or_else(|_| panic!("{name}: {version}"));
            }
        }
    }
}
