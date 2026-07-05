//! The normalized lint diagnostic every adapter parses its tool's output
//! into.
//!
//! One shape regardless of tool means `togi lint` output (human and
//! `--format json`) is uniform and tools stay swappable. The JSON layout of
//! [`Diagnostic`] is a stable machine interface: every field is always
//! present (`null` rather than omitted), so consumers can rely on the keys.
//! The `path` is always project-root-relative — the command layer
//! normalizes each tool's paths (ruff, for one, reports absolute paths) so
//! the schema is uniform.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One normalized lint finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// File the finding is in, relative to the project root. The command
    /// layer normalizes each tool's paths (some report absolute paths) so
    /// this key is uniform across languages.
    pub path: PathBuf,
    /// Where in the file, when the tool says; `None` for file-level findings.
    pub range: Option<Range>,
    /// Tool rule code (e.g. `F401`), when the tool has one.
    pub code: Option<String>,
    pub severity: Severity,
    pub message: String,
    /// Whether the tool can fix this automatically (`togi lint --fix`).
    pub fixable: bool,
}

/// A source span: start position, and an end when the tool reports one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Option<Position>,
}

/// A 1-based line/column position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub col: u32,
}

/// How bad a finding is, normalized across tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_diagnostic() -> Diagnostic {
        Diagnostic {
            path: PathBuf::from("analysis/model.py"),
            range: Some(Range {
                start: Position { line: 3, col: 8 },
                end: Some(Position { line: 3, col: 10 }),
            }),
            code: Some("F401".to_string()),
            severity: Severity::Warning,
            message: "`os` imported but unused".to_string(),
            fixable: true,
        }
    }

    #[test]
    fn diagnostic_json_shape_is_stable() {
        // This is the `togi lint --format json` payload: a machine
        // interface, so the exact shape is pinned here.
        let value = serde_json::to_value(full_diagnostic()).expect("serialize");
        assert_eq!(
            value,
            json!({
                "path": "analysis/model.py",
                "range": {
                    "start": { "line": 3, "col": 8 },
                    "end": { "line": 3, "col": 10 },
                },
                "code": "F401",
                "severity": "warning",
                "message": "`os` imported but unused",
                "fixable": true,
            })
        );
    }

    #[test]
    fn optional_fields_serialize_as_null_not_omitted() {
        // Consumers of the JSON schema get every key every time.
        let diagnostic = Diagnostic {
            path: PathBuf::from("query.sql"),
            range: None,
            code: None,
            severity: Severity::Error,
            message: "unparsable".to_string(),
            fixable: false,
        };
        let value = serde_json::to_value(diagnostic).expect("serialize");
        assert_eq!(
            value,
            json!({
                "path": "query.sql",
                "range": null,
                "code": null,
                "severity": "error",
                "message": "unparsable",
                "fixable": false,
            })
        );
    }

    #[test]
    fn severity_serializes_lowercase() {
        for (severity, expected) in [
            (Severity::Error, "\"error\""),
            (Severity::Warning, "\"warning\""),
            (Severity::Info, "\"info\""),
        ] {
            assert_eq!(
                serde_json::to_string(&severity).expect("serialize"),
                expected
            );
        }
    }

    #[test]
    fn diagnostic_roundtrips_through_json() {
        let diagnostic = full_diagnostic();
        let text = serde_json::to_string(&diagnostic).expect("serialize");
        let back: Diagnostic = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, diagnostic);
    }
}
