//! The `manifest.json` written next to each installed tool binary: what was
//! installed, from where, and when.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::term::HintExt;

/// Metadata for one installed tool version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Installed tool version, e.g. `0.10.0`.
    pub version: String,
    /// URL the archive (or package) was fetched from.
    pub source_url: String,
    /// Verified sha256 of the downloaded archive; `None` when the project
    /// publishes no checksum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Install time as UTC RFC 3339, e.g. `2026-07-02T12:00:00Z`.
    pub installed_at: String,
}

impl Manifest {
    /// A manifest stamped with the current time.
    pub fn new(version: String, source_url: String, checksum: Option<String>) -> Manifest {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0); // a pre-1970 clock only stamps the wrong timestamp
        Manifest {
            version,
            source_url,
            checksum,
            installed_at: rfc3339_utc(now),
        }
    }

    /// Read and parse a manifest file.
    pub fn load(path: &Path) -> anyhow::Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read tool manifest `{}`", path.display()))
            .hint("run `togi tools clean` to reset the tool cache, then retry")?;
        serde_json::from_str(&text)
            .with_context(|| format!("could not parse tool manifest `{}`", path.display()))
            .hint("run `togi tools clean` to reset the tool cache, then retry")
    }

    /// Write the manifest as pretty-printed JSON, creating parent
    /// directories as needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("could not create tool directory `{}`", parent.display())
            })?;
        }
        let json = serde_json::to_string_pretty(self).context("could not serialize manifest")?;
        std::fs::write(path, json)
            .with_context(|| format!("could not write tool manifest `{}`", path.display()))
            .hint("check that the togi data directory is writable")
    }
}

/// Format seconds since the Unix epoch as UTC RFC 3339
/// (`1970-01-01T00:00:00Z`).
fn rfc3339_utc(unix_secs: u64) -> String {
    const SECS_PER_DAY: u64 = 24 * 60 * 60;
    let (year, month, day) = civil_from_days((unix_secs / SECS_PER_DAY) as i64);
    let rem = unix_secs % SECS_PER_DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Gregorian date for a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days` algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153; // March-based month, [0, 11]
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            version: "0.10.0".to_string(),
            source_url: "https://example.test/air.tar.gz".to_string(),
            checksum: Some("abc123".to_string()),
            installed_at: "2026-07-02T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Parent directories are created on save, like a fresh tool dir.
        let path = dir.path().join("air").join("0.10.0").join("manifest.json");
        let original = manifest();
        original.save(&path).expect("save");
        assert_eq!(Manifest::load(&path).expect("load"), original);
    }

    #[test]
    fn serialized_field_names_are_stable() {
        let json = serde_json::to_string(&manifest()).expect("serialize");
        for field in ["version", "source_url", "checksum", "installed_at"] {
            assert!(json.contains(&format!("\"{field}\"")), "{field}: {json}");
        }
    }

    #[test]
    fn omits_checksum_when_none() {
        let mut m = manifest();
        m.checksum = None;
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(!json.contains("checksum"), "{json}");
        // ...and older manifests without the field still parse.
        let parsed: Manifest = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.checksum, None);
    }

    #[test]
    fn corrupt_manifest_error_says_what_to_do() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, "not json").expect("write");
        let err = Manifest::load(&path).expect_err("corrupt manifest");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("manifest.json"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
        assert!(rendered.contains("togi tools clean"), "{rendered}");
    }

    #[test]
    fn missing_manifest_error_names_the_file() {
        let err = Manifest::load(Path::new("no/such/manifest.json"))
            .expect_err("missing manifest must fail");
        assert!(err.to_string().contains("manifest.json"), "{err}");
    }

    #[test]
    fn new_stamps_a_plausible_utc_timestamp() {
        let m = Manifest::new(
            "1.0.0".to_string(),
            "https://example.test/t.tar.gz".to_string(),
            None,
        );
        // 2026-.. or later, shaped like RFC 3339 UTC.
        assert!(m.installed_at.as_str() >= "2026", "{}", m.installed_at);
        assert!(m.installed_at.ends_with('Z'), "{}", m.installed_at);
        assert_eq!(m.installed_at.len(), "2026-07-02T12:00:00Z".len());
    }

    #[test]
    fn formats_known_epochs_as_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339_utc(1_735_689_600), "2025-01-01T00:00:00Z");
        // Leap-year day.
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
