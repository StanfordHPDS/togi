//! Config discovery, parsing, and layering for `togi.toml`.
//!
//! Layering: **built-in defaults ← user config ← project config ← CLI
//! flags**. Each file parses into a [`Layer`] (only the keys it actually
//! sets); layers are applied to [`Config::default`] in order, so later
//! layers win key-by-key.
//!
//! This module returns data only — it never prints. Unknown-key warnings are
//! returned on [`Loaded::warnings`] for the caller to report through `term`.

mod discover;
pub(crate) mod raw;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::term::HintExt;

/// Fully resolved configuration; `Default` is the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub format: FileSelection,
    pub lint: FileSelection,
    pub sql: SqlConfig,
    pub tools: ToolsConfig,
}

/// `[format]` / `[lint]`: which languages to include and what to skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelection {
    pub languages: Vec<String>,
    /// gitignore-style globs, additive to `.gitignore`.
    pub exclude: Vec<String>,
}

/// `[sql]`: passed through to sqlfluff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlConfig {
    pub dialect: String,
}

/// `[tools]`: version pins plus per-tool passthrough args.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolsConfig {
    /// `air = "0.10.0"` style pins (also `[tools.air] version = "0.10.0"`).
    pub pins: BTreeMap<String, String>,
    /// `[tools.air] args = [...]` escape-hatch passthrough args.
    pub args: BTreeMap<String, Vec<String>>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            format: FileSelection {
                languages: strings(&["r", "python", "quarto", "sql", "markdown"]),
                exclude: Vec::new(),
            },
            lint: FileSelection {
                languages: strings(&["r", "python", "quarto", "sql"]),
                exclude: Vec::new(),
            },
            sql: SqlConfig {
                dialect: "bigquery".to_string(),
            },
            tools: ToolsConfig::default(),
        }
    }
}

/// One configuration layer: only the keys this source actually set.
///
/// A parsed config file becomes a `Layer`, and CLI flags that override
/// config keys are expressed as a `Layer` too, so all four layers merge
/// through the same [`Config::apply`] path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub format_languages: Option<Vec<String>>,
    pub format_exclude: Option<Vec<String>>,
    pub lint_languages: Option<Vec<String>>,
    pub lint_exclude: Option<Vec<String>>,
    pub sql_dialect: Option<String>,
    pub tool_pins: BTreeMap<String, String>,
    pub tool_args: BTreeMap<String, Vec<String>>,
}

impl Config {
    /// Apply a layer on top of `self`; every key the layer sets wins.
    pub fn apply(&mut self, layer: Layer) {
        if let Some(v) = layer.format_languages {
            self.format.languages = v;
        }
        if let Some(v) = layer.format_exclude {
            self.format.exclude = v;
        }
        if let Some(v) = layer.lint_languages {
            self.lint.languages = v;
        }
        if let Some(v) = layer.lint_exclude {
            self.lint.exclude = v;
        }
        if let Some(v) = layer.sql_dialect {
            self.sql.dialect = v;
        }
        self.tools.pins.extend(layer.tool_pins);
        self.tools.args.extend(layer.tool_args);
    }
}

/// Typed error for `--config` pointing at a file that does not exist: a
/// bad flag value, so `main` renders it as a usage error and exits 2.
#[derive(Debug, thiserror::Error)]
#[error("config file `{}` does not exist", path.display())]
pub struct MissingConfigFile {
    pub path: PathBuf,
}

impl MissingConfigFile {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        "check the path passed to --config, or drop the flag to discover \
         togi.toml automatically"
            .to_string()
    }
}

/// The result of [`load`]: the resolved config, which files contributed,
/// and any unknown-key warnings for the caller to print via `term::warn`.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// User config file, when it existed and was layered in.
    pub user_path: Option<PathBuf>,
    /// Project config file (`--config` or discovered `togi.toml`).
    pub project_path: Option<PathBuf>,
    /// Human-readable warnings (unknown keys); print through `term::warn`.
    pub warnings: Vec<String>,
}

/// Discover, parse, and layer configuration.
///
/// `explicit` is the global `--config <path>` flag: it replaces project-file
/// discovery and it is an error for it not to exist. `flags` carries any
/// CLI-flag overrides (the final layer).
pub fn load(cwd: &Path, explicit: Option<&Path>, flags: Layer) -> anyhow::Result<Loaded> {
    let mut config = Config::default();
    let mut warnings = Vec::new();

    let mut user_path = None;
    if let Some(path) = discover::user_config_path()
        && path.is_file()
    {
        config.apply(load_file(&path, &mut warnings)?);
        user_path = Some(path);
    }

    let project_path = match explicit {
        Some(path) => {
            if !path.is_file() {
                // Typed so `main` can exit 2: a bad flag value is a usage
                // error, not a runtime failure.
                return Err(anyhow::Error::new(MissingConfigFile {
                    path: path.to_path_buf(),
                }));
            }
            Some(path.to_path_buf())
        }
        None => discover::find_project_config(cwd),
    };
    if let Some(path) = &project_path {
        config.apply(load_file(path, &mut warnings)?);
    }

    config.apply(flags);

    Ok(Loaded {
        config,
        user_path,
        project_path,
        warnings,
    })
}

/// Read and parse one config file into a layer, converting its unknown keys
/// into warnings that name the file.
fn load_file(path: &Path, warnings: &mut Vec<String>) -> anyhow::Result<Layer> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read config file `{}`", path.display()))
        .hint("check the file's permissions, or remove it if it should not exist")?;
    let parsed = raw::parse(&text)
        .with_context(|| format!("could not parse `{}`", path.display()))
        .hint(
            "fix the TOML shown above; the supported keys are [format], [lint], \
             [sql], and [tools]",
        )?;
    for key in parsed.unknown_keys {
        warnings.push(format!(
            "ignoring unknown key `{key}` in {}",
            path.display()
        ));
    }
    Ok(parsed.layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_language_with_bigquery_sql() {
        let config = Config::default();
        assert_eq!(
            config.format.languages,
            strings(&["r", "python", "quarto", "sql", "markdown"])
        );
        assert!(config.format.exclude.is_empty());
        assert_eq!(
            config.lint.languages,
            strings(&["r", "python", "quarto", "sql"])
        );
        assert!(config.lint.exclude.is_empty());
        assert_eq!(config.sql.dialect, "bigquery");
        assert!(config.tools.pins.is_empty());
        assert!(config.tools.args.is_empty());
    }

    #[test]
    fn layering_defaults_then_user_then_project_then_flags() {
        // default < user < project < flag. Each layer overrides only the
        // keys it sets; everything else shines through.
        let user = Layer {
            sql_dialect: Some("duckdb".to_string()),
            format_exclude: Some(strings(&["renv/**"])),
            ..Layer::default()
        };
        let project = Layer {
            sql_dialect: Some("postgres".to_string()),
            ..Layer::default()
        };
        let flags = Layer {
            sql_dialect: Some("sqlite".to_string()),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        config.apply(project);
        config.apply(flags);

        // flag beat project beat user for sql.dialect
        assert_eq!(config.sql.dialect, "sqlite");
        // user's value survives where nothing above set the key
        assert_eq!(config.format.exclude, strings(&["renv/**"]));
        // untouched keys keep built-in defaults
        assert_eq!(
            config.lint.languages,
            strings(&["r", "python", "quarto", "sql"])
        );
    }

    #[test]
    fn user_layer_alone_overrides_defaults() {
        let mut config = Config::default();
        config.apply(Layer {
            format_languages: Some(strings(&["r"])),
            format_exclude: Some(strings(&["renv/**"])),
            ..Layer::default()
        });
        assert_eq!(config.format.languages, strings(&["r"]));
        assert_eq!(config.format.exclude, strings(&["renv/**"]));
        // lint untouched
        assert_eq!(
            config.lint.languages,
            strings(&["r", "python", "quarto", "sql"])
        );
    }

    #[test]
    fn a_layer_that_sets_nothing_changes_nothing() {
        let mut config = Config::default();
        config.apply(Layer {
            sql_dialect: Some("duckdb".to_string()),
            ..Layer::default()
        });
        config.apply(Layer::default());
        assert_eq!(config.sql.dialect, "duckdb");
    }

    #[test]
    fn tool_pins_and_args_merge_per_tool_across_layers() {
        let user = Layer {
            tool_pins: BTreeMap::from([
                ("air".to_string(), "0.9.0".to_string()),
                ("ruff".to_string(), "0.14.0".to_string()),
            ]),
            tool_args: BTreeMap::from([("air".to_string(), strings(&["--old"]))]),
            ..Layer::default()
        };
        let project = Layer {
            tool_pins: BTreeMap::from([("air".to_string(), "0.10.0".to_string())]),
            tool_args: BTreeMap::from([("air".to_string(), strings(&["--new"]))]),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        config.apply(project);

        // project pin wins for air; user pin for ruff survives
        assert_eq!(config.tools.pins["air"], "0.10.0");
        assert_eq!(config.tools.pins["ruff"], "0.14.0");
        // args replace wholesale per tool, they do not concatenate
        assert_eq!(config.tools.args["air"], strings(&["--new"]));
    }

    #[test]
    fn load_reports_missing_explicit_config_as_a_typed_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let err = load(dir.path(), Some(&missing), Layer::default())
            .expect_err("missing --config file must be an error");
        assert!(
            err.to_string().contains("nope.toml"),
            "names the file: {err}"
        );
        let typed = err
            .downcast_ref::<MissingConfigFile>()
            .expect("typed so main can exit 2 (usage error)");
        assert!(typed.hint().contains("--config"), "hint: {}", typed.hint());
    }
}
