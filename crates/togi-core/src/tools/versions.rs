//! Release-pinned default tool versions.
//!
//! These are the versions togi installs when a project pins nothing in
//! `togi.toml`. They are updated (and re-verified against the upstream
//! releases) as part of cutting a togi release, not at runtime.

pub const AIR: &str = "0.12.0";
pub const DEPTRY: &str = "0.25.1";
pub const RUFF: &str = "0.17.3";
pub const PANACHE: &str = "3.17.2";
pub const SQLFLUFF: &str = "4.5.0";
pub const UV: &str = "0.13.6";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_versions_are_bare_semver() {
        // Bare `X.Y.Z` — no `v` prefix, so they can drop straight into
        // asset patterns and `uv tool install pkg==X.Y.Z`.
        for (name, version) in [
            ("air", AIR),
            ("deptry", DEPTRY),
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

    #[test]
    fn panache_defaults_to_the_safe_fix_release() {
        let parts: Vec<u64> = PANACHE
            .split('.')
            .map(|part| part.parse().expect("validated semver component"))
            .collect();
        assert!(
            parts.as_slice() >= &[3, 12, 0],
            "panache {PANACHE} predates opt-in unsafe Ruff fixes"
        );
    }
}
