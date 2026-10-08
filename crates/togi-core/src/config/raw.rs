//! Parse one TOML config file into a [`Layer`], collecting unknown keys.
//!
//! Unknown keys warn instead of erroring (forward compatibility), so each
//! table captures unrecognized entries via `#[serde(flatten)]` and we
//! surface them as dotted key paths. Type errors (e.g. `dialect = 3`) are
//! real errors — a wrong type is a mistake, not a future key.

use anyhow::bail;
use serde::Deserialize;

use super::Layer;

/// A parsed file: the layer it contributes plus its unknown key paths
/// (dotted, e.g. `format.frobnicate`).
#[derive(Debug)]
pub(crate) struct Parsed {
    pub(crate) layer: Layer,
    pub(crate) unknown_keys: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawConfig {
    format: Option<RawSelection>,
    lint: Option<RawSelection>,
    python: Option<RawPython>,
    sql: Option<RawSql>,
    tools: Option<toml::Table>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawSelection {
    languages: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawSql {
    dialect: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawPython {
    dependencies: Option<bool>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// Parse a config file's contents. TOML syntax and type errors fail;
/// unrecognized keys are returned for the caller to warn about.
pub(crate) fn parse(text: &str) -> anyhow::Result<Parsed> {
    let raw: RawConfig = toml::from_str(text)?;
    let mut layer = Layer::default();
    let mut unknown_keys: Vec<String> = raw.unknown.keys().cloned().collect();

    let note_unknown = |table: &toml::Table, prefix: &str| {
        table
            .keys()
            .map(|k| format!("{prefix}.{k}"))
            .collect::<Vec<_>>()
    };

    if let Some(format) = raw.format {
        layer.format_languages = format.languages;
        layer.format_exclude = format.exclude;
        unknown_keys.extend(note_unknown(&format.unknown, "format"));
    }
    if let Some(lint) = raw.lint {
        layer.lint_languages = lint.languages;
        layer.lint_exclude = lint.exclude;
        unknown_keys.extend(note_unknown(&lint.unknown, "lint"));
    }
    if let Some(python) = raw.python {
        layer.python_dependencies = python.dependencies;
        unknown_keys.extend(note_unknown(&python.unknown, "python"));
    }
    if let Some(sql) = raw.sql {
        layer.sql_dialect = sql.dialect;
        unknown_keys.extend(note_unknown(&sql.unknown, "sql"));
    }
    if let Some(tools) = raw.tools {
        parse_tools(tools, &mut layer, &mut unknown_keys)?;
    }

    Ok(Parsed {
        layer,
        unknown_keys,
    })
}

/// `[tools]` mixes two shapes: `air = "0.10.0"` version pins and
/// `[tools.air]` tables carrying `args` (and optionally `version`), so it is
/// parsed by hand rather than through serde.
fn parse_tools(
    tools: toml::Table,
    layer: &mut Layer,
    unknown_keys: &mut Vec<String>,
) -> anyhow::Result<()> {
    for (name, value) in tools {
        match value {
            toml::Value::String(version) => {
                layer.tool_pins.insert(name, version);
            }
            toml::Value::Table(table) => {
                for (key, value) in table {
                    match (key.as_str(), value) {
                        ("version", toml::Value::String(version)) => {
                            layer.tool_pins.insert(name.clone(), version);
                        }
                        ("version", _) => {
                            bail!("`tools.{name}.version` must be a string, e.g. \"1.2.3\"")
                        }
                        ("args", toml::Value::Array(items)) => {
                            let mut args = Vec::with_capacity(items.len());
                            for item in items {
                                let toml::Value::String(arg) = item else {
                                    bail!(
                                        "`tools.{name}.args` must be an array of strings, \
                                         e.g. args = [\"--flag\"]"
                                    );
                                };
                                args.push(arg);
                            }
                            layer.tool_args.insert(name.clone(), args);
                        }
                        ("args", _) => {
                            bail!(
                                "`tools.{name}.args` must be an array of strings, \
                                 e.g. args = [\"--flag\"]"
                            )
                        }
                        (other, _) => unknown_keys.push(format!("tools.{name}.{other}")),
                    }
                }
            }
            _ => bail!(
                "`tools.{name}` must be a version string (e.g. \"1.2.3\") \
                 or a table with `args`"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_unknown(parsed: &Parsed) {
        assert!(
            parsed.unknown_keys.is_empty(),
            "expected no unknown keys, got {:?}",
            parsed.unknown_keys
        );
    }

    #[test]
    fn parses_a_config_using_every_documented_key() {
        let parsed = parse(
            r#"
            [format]
            languages = ["r", "python", "quarto", "sql", "markdown"]
            exclude = ["renv/**"]

            [lint]
            languages = ["r", "python", "quarto", "sql"]
            exclude = []

            [sql]
            dialect = "bigquery"

            [python]
            dependencies = true

            # NOTE: TOML forbids `air = "..."` under [tools] AND a
            # [tools.air] table (duplicate key); a pin plus args for the
            # same tool uses `[tools.air] version/args` instead.
            [tools]
            ruff = "0.14.0"

            [tools.air]
            args = ["--verbose"]
            "#,
        )
        .expect("a config using every documented key must parse");
        assert_no_unknown(&parsed);

        let layer = parsed.layer;
        assert_eq!(
            layer.format_languages,
            Some(vec![
                "r".to_string(),
                "python".to_string(),
                "quarto".to_string(),
                "sql".to_string(),
                "markdown".to_string()
            ])
        );
        assert_eq!(layer.format_exclude, Some(vec!["renv/**".to_string()]));
        assert_eq!(layer.lint_exclude, Some(vec![]));
        assert_eq!(layer.python_dependencies, Some(true));
        assert_eq!(layer.sql_dialect.as_deref(), Some("bigquery"));
        assert_eq!(layer.tool_pins["ruff"], "0.14.0");
        assert_eq!(layer.tool_args["air"], vec!["--verbose".to_string()]);
    }

    #[test]
    fn empty_file_parses_to_an_empty_layer() {
        let parsed = parse("").expect("empty file is valid");
        assert_no_unknown(&parsed);
        assert_eq!(parsed.layer, Layer::default());
    }

    #[test]
    fn unknown_keys_are_collected_not_errors() {
        let parsed = parse(
            r#"
            future-section = { x = 1 }
            top-level = true

            [format]
            shiny = "yes"

            [lint]
            frobnicate = 1

            [sql]
            dialect = "bigquery"
            engine = "warp"

            [python]
            dependencies = false
            environment = "automatic"

            [tools.air]
            args = []
            turbo = true
            "#,
        )
        .expect("unknown keys must not fail the parse");
        let mut keys = parsed.unknown_keys.clone();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "format.shiny",
                "future-section",
                "lint.frobnicate",
                "python.environment",
                "sql.engine",
                "tools.air.turbo",
                "top-level",
            ]
        );
        // known keys around the unknown ones still land
        assert_eq!(parsed.layer.sql_dialect.as_deref(), Some("bigquery"));
        assert_eq!(parsed.layer.python_dependencies, Some(false));
    }

    #[test]
    fn parses_python_dependencies_as_an_optional_boolean() {
        let enabled = parse("[python]\ndependencies = true\n").expect("parse true");
        let disabled = parse("[python]\ndependencies = false\n").expect("parse false");
        let absent = parse("[python]\n").expect("parse absent key");

        assert_eq!(enabled.layer.python_dependencies, Some(true));
        assert_eq!(disabled.layer.python_dependencies, Some(false));
        assert_eq!(absent.layer.python_dependencies, None);
    }

    #[test]
    fn deptry_pin_and_passthrough_args_parse_together() {
        let parsed = parse(
            r#"
            [tools.deptry]
            version = "0.24.0"
            args = ["--ignore", "DEP002"]
            "#,
        )
        .expect("deptry settings parse");

        assert_no_unknown(&parsed);
        assert_eq!(parsed.layer.tool_pins["deptry"], "0.24.0");
        assert_eq!(
            parsed.layer.tool_args["deptry"],
            vec!["--ignore".to_string(), "DEP002".to_string()]
        );
    }

    #[test]
    fn tool_table_version_key_acts_as_a_pin() {
        let parsed = parse(
            r#"
            [tools.air]
            version = "0.10.0"
            args = ["--fast"]
            "#,
        )
        .expect("version-in-table must parse");
        assert_no_unknown(&parsed);
        assert_eq!(parsed.layer.tool_pins["air"], "0.10.0");
        assert_eq!(parsed.layer.tool_args["air"], vec!["--fast".to_string()]);
    }

    #[test]
    fn wrong_types_are_errors_not_warnings() {
        assert!(parse("[python]\ndependencies = \"yes\"\n").is_err());
        assert!(parse("[sql]\ndialect = 3\n").is_err());
        assert!(parse("[format]\nlanguages = \"r\"\n").is_err());
        assert!(parse("[tools]\nair = 3\n").is_err());
        assert!(parse("[tools.air]\nargs = [1, 2]\n").is_err());
        assert!(parse("[tools.air]\nversion = 1\n").is_err());
    }

    #[test]
    fn invalid_toml_syntax_is_an_error() {
        assert!(parse("[sql\ndialect = \"bigquery\"").is_err());
    }
}
