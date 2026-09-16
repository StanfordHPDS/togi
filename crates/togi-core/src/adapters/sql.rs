//! The SQL adapter: sqlfluff behind the [`Formatter`]/[`Linter`] traits.
//!
//! sqlfluff has no `format --check`, so check mode runs
//! `sqlfluff lint --format json` restricted to [`FORMAT_RULES`] — the same
//! rule subset the `format` subcommand force-applies — and reports files
//! with fixable violations as "would change". In-place formatting detects
//! changes by comparing file contents before and after the run instead of
//! parsing sqlfluff's human-oriented output.
//!
//! When sqlfluff would read no configuration of its own for any input
//! file, togi supplies its defaults: the configured `[sql] dialect` as `--dialect`,
//! and a generated config file (see [`DEFAULT_CONFIG`]) as `--config`,
//! which lints files of any size and leaves unquoted identifier case as
//! written. Both are gated on the same check because sqlfluff layers a
//! `--config` file over the config it discovers, so passing it alongside
//! a project's or user's own config would override it; when sqlfluff
//! finds any config, that config alone applies. See [`ConfigEnv`] for
//! the directories sqlfluff searches. `[tools.sqlfluff] args`
//! from togi.toml are appended to every invocation as the escape hatch.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context;
use serde::Deserialize;

use crate::adapters::{
    Diagnostic, FormatOutcome, Formatter, Linter, Position, Range, Severity, ToolCtx,
};
use crate::config::Config;
use crate::term::HintExt;

/// The sqlfluff adapter. One instance serves every run.
#[derive(Debug, Default)]
pub struct SqlFluffAdapter {
    /// Test override for where sqlfluff looks for config; production
    /// reads the real home directory, working directory, and environment
    /// (see [`ConfigEnv::current`]).
    config_env: Option<ConfigEnv>,
}

impl SqlFluffAdapter {
    pub fn new() -> SqlFluffAdapter {
        SqlFluffAdapter::default()
    }

    /// An adapter whose config lookup sees `env` instead of the real
    /// machine, so tests never depend on where they run.
    #[cfg(test)]
    pub(crate) fn with_config_env(env: ConfigEnv) -> SqlFluffAdapter {
        SqlFluffAdapter {
            config_env: Some(env),
        }
    }

    /// Where sqlfluff looks for config for this run.
    fn config_env(&self) -> ConfigEnv {
        match &self.config_env {
            Some(env) => env.clone(),
            None => ConfigEnv::current(),
        }
    }
}

/// The inputs that decide which directories sqlfluff reads config from.
///
/// sqlfluff reads, for each file: the user-level directories
/// (`user_dirs`: the home directory and its user config directory); every
/// directory strictly between the home directory and the file's
/// directory, starting below their common ancestor; and the working
/// directory's common ancestor with the file's directory down to and
/// including that directory. Its root config for the run additionally
/// reads the directories strictly between home and the working directory,
/// and the working directory itself. togi runs sqlfluff in its own
/// working directory, so both walks use the same `cwd`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfigEnv {
    /// The home directory, if known.
    pub(crate) home: Option<PathBuf>,
    /// The working directory sqlfluff runs in, if known.
    pub(crate) cwd: Option<PathBuf>,
    /// The user-level directories sqlfluff reads config from.
    pub(crate) user_dirs: Vec<PathBuf>,
}

impl ConfigEnv {
    /// The environment of this process.
    fn current() -> ConfigEnv {
        let base = directories::BaseDirs::new();
        let home = base.as_ref().map(|b| b.home_dir().to_path_buf());
        let user_dirs = user_config_dirs(
            UserConfigPlatform::current(),
            home.as_deref(),
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            base.as_ref().map(directories::BaseDirs::data_local_dir),
        );
        ConfigEnv {
            home,
            cwd: std::env::current_dir().ok(),
            user_dirs,
        }
    }
}

const TOOL: &str = "sqlfluff";

/// The rule subset sqlfluff's own `format` subcommand force-applies
/// (its command implementation hardwires this list). Check mode lints
/// against exactly this set so "would change" matches what `format`
/// would actually rewrite.
const FORMAT_RULES: &str = "capitalisation,layout,ambiguous.union,convention.not_equal,\
                            convention.coalesce,convention.select_trailing_comma,\
                            convention.is_null,jinja.padding,structure.distinct";

/// Violation codes sqlfluff emits for files it could not parse or
/// template: hard failures, not style findings a formatter could fix.
const PARSE_ERROR_CODES: [&str; 2] = ["PRS", "TMP"];

/// The shared what-to-do-next for sqlfluff usage/config failures.
const CONFIG_HINT: &str = "check `[sql] dialect` and `[tools.sqlfluff] args` in togi.toml \
                           (or the project's own sqlfluff config), then rerun";

/// sqlfluff settings togi applies when the project has no sqlfluff
/// configuration of its own.
const DEFAULT_CONFIG: &str = "\
[sqlfluff]
# Lint files of any size instead of skipping large ones.
large_file_skip_byte_limit = 0

[sqlfluff:rules:capitalisation.identifiers]
# Leave the case of unquoted identifiers as written.
unquoted_identifiers_policy = none
";

impl crate::adapters::Adapter for SqlFluffAdapter {
    fn name(&self) -> &'static str {
        TOOL
    }
}

impl Formatter for SqlFluffAdapter {
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome> {
        // No files means nothing to do; sqlfluff invoked without paths
        // would scan the whole working directory instead.
        if files.is_empty() {
            return Ok(FormatOutcome::default());
        }
        let env = self.config_env();
        if check {
            check_format(files, &env, ctx)
        } else {
            apply_format(files, &env, ctx)
        }
    }
}

impl Linter for SqlFluffAdapter {
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let env = self.config_env();
        if fix {
            let output = run(&["fix"], files, &env, ctx)?;
            // Exit 1 only means violations remain (unfixable findings or
            // parse errors); the follow-up lint below reports them.
            if exit_code(&output) > 1 {
                return Err(tool_error("sqlfluff fix", &output)).hint(CONFIG_HINT);
            }
        }
        let output = run(&["lint", "--format", "json"], files, &env, ctx)?;
        if exit_code(&output) > 1 {
            return Err(tool_error("sqlfluff lint", &output)).hint(CONFIG_HINT);
        }
        let reports = parse_reports(&String::from_utf8_lossy(&output.stdout))?;
        Ok(reports_to_diagnostics(&reports))
    }
}

/// Format in place, then report which files actually changed by
/// comparing their contents around the run (sqlfluff's stdout is
/// human-oriented and not worth parsing for this).
fn apply_format(
    files: &[PathBuf],
    env: &ConfigEnv,
    ctx: &ToolCtx,
) -> anyhow::Result<FormatOutcome> {
    let before: Vec<Vec<u8>> = files
        .iter()
        .map(|f| read_for_change_detection(f))
        .collect::<anyhow::Result<_>>()?;
    let output = run(&["format"], files, env, ctx)?;
    match exit_code(&output) {
        0 => {}
        // Exit 1 from `format` means templating/parse errors: sqlfluff
        // formatted what it could and skipped the broken files.
        1 => {
            return Err(tool_error("sqlfluff format", &output)).hint(
                "some SQL could not be parsed; run `togi lint` for the file \
                 and line, fix the syntax, then rerun `togi format`",
            );
        }
        _ => return Err(tool_error("sqlfluff format", &output)).hint(CONFIG_HINT),
    }
    let mut changed = Vec::new();
    for (file, old) in files.iter().zip(before) {
        if read_for_change_detection(file)? != old {
            changed.push(file.clone());
        }
    }
    Ok(FormatOutcome {
        processed: files.len(),
        changed,
    })
}

/// Check mode: `sqlfluff format` has no `--check`, so lint against the
/// same rule subset it would apply and report the files that would
/// change without touching anything.
fn check_format(
    files: &[PathBuf],
    env: &ConfigEnv,
    ctx: &ToolCtx,
) -> anyhow::Result<FormatOutcome> {
    let output = run(
        &["lint", "--format", "json", "--rules", FORMAT_RULES],
        files,
        env,
        ctx,
    )?;
    // Exit 1 is "violations found", which is exactly what we are asking.
    if exit_code(&output) > 1 {
        return Err(tool_error("sqlfluff lint", &output)).hint(CONFIG_HINT);
    }
    let reports = parse_reports(&String::from_utf8_lossy(&output.stdout))?;
    Ok(FormatOutcome {
        processed: files.len(),
        changed: would_change(&reports)?,
    })
}

/// Resolve the managed sqlfluff and run one invocation over the whole
/// batch. `env` says where sqlfluff will look for config.
fn run(
    subcommand: &[&str],
    files: &[PathBuf],
    env: &ConfigEnv,
    ctx: &ToolCtx,
) -> anyhow::Result<Output> {
    let binary = ctx.tool_path(TOOL)?;
    // Held until the child exits so the generated config file survives
    // the run.
    let default_config = if default_config_needed(files, env) {
        Some(write_default_config()?)
    } else {
        None
    };
    let args = build_args(
        subcommand,
        files,
        ctx.config,
        default_config.as_ref().map(|f| f.path()),
    );
    crate::adapters::log_command(ctx, &binary, &args);
    let mut cmd = Command::new(&binary);
    cmd.args(&args);
    crate::adapters::process::retry_etxtbsy(|| cmd.output())
        .with_context(|| format!("could not run `{}`", binary.display()))
        .hint("run `togi tools clean` to reset the managed tool cache, then rerun")
}

/// The process exit code; a signal death counts as a hard error.
fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(2)
}

/// A failure error carrying whatever the tool said (stderr first, else
/// stdout) so the user sees sqlfluff's own report.
fn tool_error(what: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = match stderr.trim() {
        "" => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        s => s.to_string(),
    };
    anyhow::anyhow!("`{what}` failed (exit {}): {said}", exit_code(output))
}

/// Read one file for before/after change comparison.
fn read_for_change_detection(file: &Path) -> anyhow::Result<Vec<u8>> {
    fs::read(file)
        .with_context(|| {
            format!(
                "could not read `{}` to detect formatting changes",
                file.display()
            )
        })
        .hint("check that the file exists and is readable, or add it to `[format] exclude` in togi.toml")
}

/// The full sqlfluff command line for one invocation: subcommand, then
/// togi's defaults, then the `[tools.sqlfluff] args` escape hatch (after
/// the defaults, so it can override them), then the files.
///
/// `default_config` is the path of the generated [`DEFAULT_CONFIG`] file,
/// present only when [`default_config_needed`] says togi should supply
/// its defaults. Its presence gates both `--dialect` and `--config`, so
/// the two defaults are always applied or omitted together.
fn build_args(
    subcommand: &[&str],
    files: &[PathBuf],
    config: &Config,
    default_config: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = subcommand.iter().map(OsString::from).collect();
    args.push("--disable-progress-bar".into());
    if let Some(path) = default_config {
        args.push("--dialect".into());
        args.push(config.sql.dialect.as_str().into());
        args.push("--config".into());
        args.push(path.as_os_str().to_os_string());
    }
    if let Some(extra) = config.tools.args.get(TOOL) {
        args.extend(extra.iter().map(OsString::from));
    }
    args.extend(files.iter().map(|f| f.as_os_str().to_os_string()));
    args
}

/// Whether togi should supply its defaults, the dialect and
/// [`DEFAULT_CONFIG`]: only when sqlfluff would read no config of its own
/// for any of `files`, since sqlfluff layers `--config` over the config
/// it discovers and would override it.
fn default_config_needed(files: &[PathBuf], env: &ConfigEnv) -> bool {
    !env.user_dirs.iter().any(|d| dir_has_sqlfluff_config(d))
        && !project_has_sqlfluff_config(files, env)
}

/// The platform rules sqlfluff follows when locating its user config
/// directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserConfigPlatform {
    MacOs,
    Windows,
    /// Linux and every other unix.
    Unix,
}

impl UserConfigPlatform {
    fn current() -> UserConfigPlatform {
        if cfg!(target_os = "macos") {
            UserConfigPlatform::MacOs
        } else if cfg!(windows) {
            UserConfigPlatform::Windows
        } else {
            UserConfigPlatform::Unix
        }
    }
}

/// The user-level directories sqlfluff reads config from, with its
/// environment inputs made explicit: the home directory itself, then
/// sqlfluff's user config directory for `platform`. `local_data_dir` is
/// the Windows local application data directory (`%LOCALAPPDATA%`).
fn user_config_dirs(
    platform: UserConfigPlatform,
    home: Option<&Path>,
    xdg_config_home: Option<&OsStr>,
    local_data_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = home.map(Path::to_path_buf).into_iter().collect();
    // sqlfluff prefers `~/.config/sqlfluff` on every platform when it exists.
    let cross_platform = home.map(|h| h.join(".config").join("sqlfluff"));
    if let Some(path) = cross_platform.as_ref().filter(|p| p.exists()) {
        dirs.push(path.clone());
        return dirs;
    }
    let xdg = xdg_config_home
        .and_then(xdg_value)
        .map(|x| x.join("sqlfluff"));
    let appdir = match platform {
        // Whether or not it exists, a set `XDG_CONFIG_HOME` wins on macOS
        // too; Application Support is used only without it.
        UserConfigPlatform::MacOs => xdg.or_else(|| {
            home.map(|h| {
                h.join("Library")
                    .join("Application Support")
                    .join("sqlfluff")
            })
        }),
        UserConfigPlatform::Unix => xdg.or(cross_platform),
        UserConfigPlatform::Windows => local_data_dir.map(|d| d.join("sqlfluff").join("sqlfluff")),
    };
    dirs.extend(appdir);
    dirs
}

/// `XDG_CONFIG_HOME` as sqlfluff's directory lookup reads it: surrounding
/// whitespace removed, and a blank value treated as unset.
fn xdg_value(raw: &OsStr) -> Option<PathBuf> {
    match raw.to_str() {
        Some(text) => {
            let trimmed = text.trim_matches(is_python_whitespace);
            (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
        }
        None => Some(trim_non_unicode(raw)),
    }
}

/// Whether Python's `str.strip` removes `c`: Unicode whitespace plus the
/// ASCII information separators, which Rust does not count as whitespace.
fn is_python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// A value that is not valid Unicode, trimmed as Python sees it. Python
/// decodes each undecodable byte to a lone surrogate, which is never
/// whitespace, so only ASCII whitespace bytes are stripped from the ends.
#[cfg(unix)]
fn trim_non_unicode(raw: &OsStr) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let is_space = |b: &u8| b.is_ascii() && is_python_whitespace(char::from(*b));
    let bytes = raw.as_bytes();
    let start = bytes
        .iter()
        .position(|b| !is_space(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_space(b))
        .map_or(start, |i| i + 1);
    PathBuf::from(OsStr::from_bytes(bytes.get(start..end).unwrap_or_default()))
}

/// A value that is not valid Unicode, used as is: only unix platforms
/// read `XDG_CONFIG_HOME`.
#[cfg(not(unix))]
fn trim_non_unicode(raw: &OsStr) -> PathBuf {
    PathBuf::from(raw)
}

/// Write [`DEFAULT_CONFIG`] to a temp file that lives as long as the
/// returned handle.
fn write_default_config() -> anyhow::Result<tempfile::NamedTempFile> {
    let file = tempfile::Builder::new()
        .prefix("togi-sqlfluff-")
        .suffix(".cfg")
        .tempfile()
        .context("could not create a temporary sqlfluff config")
        .hint("check that the system temp directory is writable")?;
    fs::write(file.path(), DEFAULT_CONFIG)
        .context("could not write the temporary sqlfluff config")
        .hint("check that the system temp directory is writable")?;
    Ok(file)
}

/// Whether sqlfluff would find project-level config for any of `files`,
/// or for its root config in the working directory (see [`ConfigEnv`]).
fn project_has_sqlfluff_config(files: &[PathBuf], env: &ConfigEnv) -> bool {
    let home = env.home.as_deref();
    let cwd = env.cwd.as_deref();
    let root = cwd.map(|dir| dir_search_dirs(dir, home, cwd));
    let mut seen = HashSet::new();
    files
        .iter()
        .map(|file| file_search_dirs(file, home, cwd))
        .chain(root)
        .flatten()
        .any(|dir| seen.insert(dir.clone()) && dir_has_sqlfluff_config(&dir))
}

/// The directories sqlfluff searches for project config for `file`,
/// given the home and working directories. A relative `file` is resolved
/// against `cwd`.
fn file_search_dirs(file: &Path, home: Option<&Path>, cwd: Option<&Path>) -> Vec<PathBuf> {
    // sqlfluff normalizes each input path before searching for its config.
    let file = absolute(&normalize(file), cwd);
    let dir = file.parent().unwrap_or(&file);
    dir_search_dirs(dir, home, cwd)
}

/// The directories sqlfluff searches for project config for a target
/// directory `dir`: those strictly between `home` and `dir` (starting
/// below their common ancestor), then `cwd`'s common ancestor with `dir`
/// down to and including `dir`.
fn dir_search_dirs(dir: &Path, home: Option<&Path>, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        let chain = intermediate_dirs(dir, &absolute(home, cwd));
        // The two ends are covered elsewhere: home as a user-level
        // directory, the target by the working-directory walk.
        let inner = chain.len().saturating_sub(1);
        dirs.extend(chain.into_iter().take(inner).skip(1));
    }
    if let Some(cwd) = cwd {
        dirs.extend(intermediate_dirs(dir, cwd));
    }
    dirs
}

/// `path` with `.` segments removed and each `..` folded into the
/// segment before it, purely lexically as Python's `os.path.normpath`
/// does: no filesystem access, so symlinks are not resolved. A `..` at the
/// root is dropped; a leading `..` in a relative path is kept.
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir) => {}
                _ => parts.push(part),
            },
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        PathBuf::from(".")
    } else {
        parts.iter().collect()
    }
}

/// `path` made absolute against `cwd`, without normalizing it.
fn absolute(path: &Path, cwd: Option<&Path>) -> PathBuf {
    match cwd {
        Some(cwd) => cwd.join(path),
        None => path.to_path_buf(),
    }
}

/// The directories from the common ancestor of `inner` and `outer` down
/// to and including `inner`. With no common ancestor (different Windows
/// drives), just `outer` and then `inner`.
fn intermediate_dirs(inner: &Path, outer: &Path) -> Vec<PathBuf> {
    let parts: Vec<Component> = inner.components().collect();
    let common = parts
        .iter()
        .zip(outer.components())
        .take_while(|(a, b)| same_component(a, b))
        .count();
    if common == 0 {
        return vec![outer.to_path_buf(), inner.to_path_buf()];
    }
    (common..=parts.len())
        .map(|n| parts.iter().take(n).collect())
        .collect()
}

/// Whether two path components match the way Python's `commonpath`
/// compares them: exactly, except case-insensitively on Windows.
fn same_component(a: &Component, b: &Component) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Whether `dir` contains sqlfluff configuration: a `.sqlfluff` file, one
/// of the shared ini files with a sqlfluff section, or a `pyproject.toml`
/// sqlfluff would take config from (see [`pyproject_has_sqlfluff_config`]).
fn dir_has_sqlfluff_config(dir: &Path) -> bool {
    if dir.join(".sqlfluff").is_file() {
        return true;
    }
    let has_ini_section = ["setup.cfg", "tox.ini", "pep8.ini"].iter().any(|name| {
        fs::read_to_string(dir.join(name)).is_ok_and(|text| text.contains("[sqlfluff"))
    });
    has_ini_section || pyproject_has_sqlfluff_config(&dir.join("pyproject.toml"))
}

/// Whether sqlfluff would load `path` as a `pyproject.toml` carrying its
/// config: sqlfluff reads the `tool.sqlfluff` table, however the TOML
/// spells it. A file sqlfluff would fail on (unreadable, not UTF-8, not
/// valid TOML, or with a `tool` key that is not a table) also counts, so
/// togi never layers its defaults over it and sqlfluff reports the
/// problem itself. An empty `tool.sqlfluff` table counts too.
fn pyproject_has_sqlfluff_config(path: &Path) -> bool {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => return err.kind() != std::io::ErrorKind::NotFound,
    };
    match text.parse::<toml::Table>() {
        Ok(doc) => match doc.get("tool") {
            None => false,
            Some(toml::Value::Table(tool)) => tool.contains_key("sqlfluff"),
            Some(_) => true,
        },
        Err(_) => true,
    }
}

/// One file's entry in `sqlfluff lint --format json` output.
#[derive(Debug, Deserialize)]
struct FileReport {
    filepath: String,
    #[serde(default)]
    violations: Vec<Violation>,
}

/// One violation as sqlfluff reports it; every field is defaulted so a
/// sparse entry (e.g. a parse error, which carries no `fixes`) still
/// parses.
#[derive(Debug, Default, Deserialize)]
struct Violation {
    #[serde(default)]
    start_line_no: Option<u32>,
    #[serde(default)]
    start_line_pos: Option<u32>,
    #[serde(default)]
    end_line_no: Option<u32>,
    #[serde(default)]
    end_line_pos: Option<u32>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    warning: bool,
    #[serde(default)]
    fixes: Vec<serde::de::IgnoredAny>,
}

/// Parse `sqlfluff lint --format json` stdout.
fn parse_reports(stdout: &str) -> anyhow::Result<Vec<FileReport>> {
    serde_json::from_str(stdout.trim())
        .context("could not parse sqlfluff's JSON lint output")
        .hint(
            "this togi build may not match the installed sqlfluff; run \
             `togi tools update`, or pin a compatible `[tools] sqlfluff` \
             version in togi.toml",
        )
}

/// Every violation in `reports`, normalized into [`Diagnostic`]s.
fn reports_to_diagnostics(reports: &[FileReport]) -> Vec<Diagnostic> {
    reports
        .iter()
        .flat_map(|report| {
            report
                .violations
                .iter()
                .map(|violation| to_diagnostic(&report.filepath, violation))
        })
        .collect()
}

fn to_diagnostic(filepath: &str, violation: &Violation) -> Diagnostic {
    let position = |line: Option<u32>, col: Option<u32>| match (line, col) {
        (Some(line), Some(col)) => Some(Position { line, col }),
        _ => None,
    };
    let range = position(violation.start_line_no, violation.start_line_pos).map(|start| Range {
        start,
        end: position(violation.end_line_no, violation.end_line_pos),
    });
    Diagnostic {
        path: PathBuf::from(filepath),
        range,
        code: violation.code.clone(),
        severity: if violation.warning {
            Severity::Warning
        } else {
            Severity::Error
        },
        message: violation.description.clone(),
        fixable: !violation.fixes.is_empty(),
    }
}

/// Whether a violation is a templating/parse failure rather than a style
/// finding.
fn is_parse_error(violation: &Violation) -> bool {
    violation
        .code
        .as_deref()
        .is_some_and(|code| PARSE_ERROR_CODES.contains(&code))
}

/// Check-mode change detection over parsed lint output: files with
/// fixable violations under [`FORMAT_RULES`] would change; unparsable
/// files are an error, matching what running `format` on them would be.
fn would_change(reports: &[FileReport]) -> anyhow::Result<Vec<PathBuf>> {
    let unparsable: Vec<&str> = reports
        .iter()
        .filter(|report| report.violations.iter().any(is_parse_error))
        .map(|report| report.filepath.as_str())
        .collect();
    if !unparsable.is_empty() {
        return Err(anyhow::anyhow!(
            "sqlfluff could not parse: {}",
            unparsable.join(", ")
        ))
        .hint(
            "fix the SQL syntax in the listed files (`togi lint` shows the \
             failing lines), then rerun",
        );
    }
    Ok(reports
        .iter()
        .filter(|report| {
            report
                .violations
                .iter()
                .any(|violation| !violation.fixes.is_empty())
        })
        .map(|report| PathBuf::from(&report.filepath))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeToolPaths;
    use crate::adapters::{Formatter, Linter};

    /// A recorded sqlfluff output from
    /// `tests/fixtures/tool-output/sqlfluff/`.
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/sqlfluff")
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    // ---- recorded-output parsing -------------------------------------

    #[test]
    fn recorded_lint_violations_parse_into_normalized_diagnostics() {
        let reports = parse_reports(&fixture("lint-violations.json")).expect("parses");
        let diagnostics = reports_to_diagnostics(&reports);

        // clean.sql contributes nothing; events.sql has five findings.
        assert_eq!(diagnostics.len(), 5);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.path == Path::new("events.sql")),
            "{diagnostics:?}"
        );

        let first = &diagnostics[0];
        assert_eq!(first.code.as_deref(), Some("LT09"));
        assert_eq!(first.severity, Severity::Error);
        assert!(first.fixable, "LT09 comes with fixes: {first:?}");
        assert!(
            first.message.contains("Select targets"),
            "keeps sqlfluff's description: {first:?}"
        );
        let range = first.range.expect("LT09 has positions");
        assert_eq!(range.start, Position { line: 1, col: 1 });
        assert_eq!(range.end, Some(Position { line: 3, col: 18 }));
    }

    #[test]
    fn recorded_parse_errors_become_error_diagnostics() {
        let reports = parse_reports(&fixture("lint-parse-error.json")).expect("parses");
        let diagnostics = reports_to_diagnostics(&reports);

        assert_eq!(diagnostics.len(), 1);
        let prs = &diagnostics[0];
        assert_eq!(prs.path, PathBuf::from("broken.sql"));
        assert_eq!(prs.code.as_deref(), Some("PRS"));
        assert_eq!(prs.severity, Severity::Error);
        assert!(!prs.fixable, "parse errors carry no fixes: {prs:?}");
        let range = prs.range.expect("PRS still has a position");
        assert_eq!(range.start, Position { line: 1, col: 19 });
    }

    #[test]
    fn check_mode_flags_files_with_fixable_violations_as_would_change() {
        let reports = parse_reports(&fixture("lint-violations.json")).expect("parses");
        let changed = would_change(&reports).expect("no parse errors in this recording");
        assert_eq!(changed, vec![PathBuf::from("events.sql")]);
    }

    #[test]
    fn check_mode_errors_on_unparsable_files() {
        // The format-rules recording includes broken.sql, whose PRS
        // violation means `sqlfluff format` would fail on it too.
        let reports = parse_reports(&fixture("lint-format-rules.json")).expect("parses");
        let err = would_change(&reports).expect_err("parse errors must not pass silently");
        let rendered = crate::term::render_error(&err, false);
        assert!(
            rendered.contains("broken.sql"),
            "names the file: {rendered}"
        );
        assert!(rendered.contains("hint:"), "says what to do: {rendered}");
    }

    #[test]
    fn garbled_tool_output_is_an_error_with_a_hint_not_a_panic() {
        let err = parse_reports("not json at all").expect_err("garbage must not parse");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("sqlfluff"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    // ---- dialect flag presence/absence -------------------------------

    /// A fresh project directory in a tempdir.
    fn project() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// A config environment rooted at `root`: it is both the home and the
    /// working directory, with no user-level directories, so config
    /// discovery for files under `root` never leaves it.
    fn hermetic(root: &Path) -> ConfigEnv {
        ConfigEnv {
            home: Some(root.to_path_buf()),
            cwd: Some(root.to_path_buf()),
            user_dirs: Vec::new(),
        }
    }

    /// `hermetic(root)` with the given user-level directories.
    fn hermetic_with_user_dirs(root: &Path, user_dirs: Vec<PathBuf>) -> ConfigEnv {
        ConfigEnv {
            user_dirs,
            ..hermetic(root)
        }
    }

    /// `build_args` gated the way `run` gates it: a generated config path
    /// (any path will do here) only when togi's defaults apply for a
    /// project rooted at `root`.
    fn gated_args(
        subcommand: &[&str],
        files: &[PathBuf],
        config: &Config,
        root: &Path,
    ) -> Vec<String> {
        let placeholder = Path::new("togi-default.cfg");
        let default_config = default_config_needed(files, &hermetic(root)).then_some(placeholder);
        strings(&build_args(subcommand, files, config, default_config))
    }

    #[test]
    fn dialect_flag_present_without_project_sqlfluff_config() {
        let dir = project();
        let files = vec![dir.path().join("q.sql")];
        let args = gated_args(&["lint"], &files, &Config::default(), dir.path());
        let dialect_at = args.iter().position(|a| a == "--dialect");
        let at = dialect_at.expect("no project config, so togi supplies the dialect");
        assert_eq!(args[at + 1], "bigquery", "{args:?}");
    }

    #[test]
    fn configured_dialect_is_passed_through() {
        let dir = project();
        let files = vec![dir.path().join("q.sql")];
        let mut config = Config::default();
        config.sql.dialect = "duckdb".to_string();
        let args = gated_args(&["lint"], &files, &config, dir.path());
        let at = args.iter().position(|a| a == "--dialect").expect("flag");
        assert_eq!(args[at + 1], "duckdb", "{args:?}");
    }

    #[test]
    fn dialect_flag_absent_with_project_dot_sqlfluff() {
        let dir = project();
        fs::write(dir.path().join(".sqlfluff"), "[sqlfluff]\ndialect = ansi\n")
            .expect("write config");
        let files = vec![dir.path().join("q.sql")];
        let args = gated_args(&["lint"], &files, &Config::default(), dir.path());
        assert!(
            !args.contains(&"--dialect".to_string()),
            "the project's own config wins: {args:?}"
        );
    }

    #[test]
    fn shared_config_files_count_only_with_a_sqlfluff_section() {
        for (name, with_section, without_section) in [
            ("setup.cfg", "[sqlfluff]\ndialect = ansi\n", "[metadata]\n"),
            ("tox.ini", "[sqlfluff:rules]\nx = y\n", "[tox]\n"),
            (
                "pyproject.toml",
                "[tool.sqlfluff.core]\ndialect = \"ansi\"\n",
                "[tool.ruff]\n",
            ),
        ] {
            let dir = project();
            let files = vec![dir.path().join("q.sql")];

            fs::write(dir.path().join(name), without_section).expect("write");
            let args = gated_args(&["lint"], &files, &Config::default(), dir.path());
            assert!(
                args.contains(&"--dialect".to_string()),
                "{name} without a sqlfluff section is not sqlfluff config: {args:?}"
            );

            fs::write(dir.path().join(name), with_section).expect("write");
            let args = gated_args(&["lint"], &files, &Config::default(), dir.path());
            assert!(
                !args.contains(&"--dialect".to_string()),
                "{name} with a sqlfluff section is project config: {args:?}"
            );
        }
    }

    /// Whether a project whose `pyproject.toml` holds `contents` counts as
    /// having sqlfluff config.
    fn pyproject_counts(contents: &[u8]) -> bool {
        let dir = project();
        fs::write(dir.path().join("pyproject.toml"), contents).expect("write");
        let files = vec![dir.path().join("q.sql")];
        !default_config_needed(&files, &hermetic(dir.path()))
    }

    #[test]
    fn pyproject_counts_for_any_spelling_of_the_tool_sqlfluff_table() {
        for contents in [
            "[tool.sqlfluff]\n",
            "[tool.sqlfluff.core]\nexclude_rules = \"CP02\"\n",
            "[tool]\nsqlfluff = { core = { exclude_rules = \"CP02\" } }\n",
            "tool.sqlfluff.core.exclude_rules = \"CP02\"\n",
            "[tool.sqlfluff.rules.\"capitalisation.identifiers\"]\nx = 1\n",
        ] {
            assert!(pyproject_counts(contents.as_bytes()), "{contents}");
        }
    }

    #[test]
    fn pyproject_mentioning_sqlfluff_outside_the_table_does_not_count() {
        for contents in [
            "",
            "[project]\nname = \"x\"\n",
            "# [tool.sqlfluff]\n[tool.ruff]\n",
            "[tool.ruff]\nnote = \"\"\"\n[tool.sqlfluff]\n\"\"\"\n",
            "[tool.sqlfluffy]\nx = 1\n",
            "[project.tool.sqlfluff]\nx = 1\n",
        ] {
            assert!(!pyproject_counts(contents.as_bytes()), "{contents}");
        }
    }

    #[test]
    fn pyproject_sqlfluff_cannot_load_counts_so_sqlfluff_reports_it() {
        for contents in [
            &b"[tool.sqlfluff\n"[..],
            b"# \xff\n[tool.ruff]\n",
            b"tool = 1\n",
        ] {
            assert!(
                pyproject_counts(contents),
                "{}",
                String::from_utf8_lossy(contents)
            );
        }
    }

    #[test]
    fn a_pyproject_toml_directory_counts_but_a_missing_file_does_not() {
        let dir = project();
        let files = vec![dir.path().join("q.sql")];
        assert!(default_config_needed(&files, &hermetic(dir.path())));
        fs::create_dir(dir.path().join("pyproject.toml")).expect("mkdir");
        assert!(!default_config_needed(&files, &hermetic(dir.path())));
    }

    #[test]
    fn config_in_a_parent_directory_within_the_project_counts() {
        let dir = project();
        fs::write(dir.path().join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let nested = dir.path().join("analysis/queries");
        fs::create_dir_all(&nested).expect("mkdirs");
        let files = vec![nested.join("q.sql")];
        let args = gated_args(&["lint"], &files, &Config::default(), dir.path());
        assert!(!args.contains(&"--dialect".to_string()), "{args:?}");
    }

    #[test]
    fn repository_boundaries_do_not_stop_the_walk() {
        // sqlfluff does not stop at `.git`: config between the repository
        // and the home directory still applies.
        let home = project();
        let work = home.path().join("work");
        let repo = work.join("repo");
        fs::create_dir_all(repo.join(".git")).expect("mkdirs");
        fs::write(work.join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let files = vec![repo.join("q.sql")];
        let env = ConfigEnv {
            home: Some(home.path().to_path_buf()),
            cwd: Some(repo.clone()),
            user_dirs: Vec::new(),
        };
        assert!(project_has_sqlfluff_config(&files, &env));
        assert!(!default_config_needed(&files, &env));
    }

    #[test]
    fn config_above_home_counts_only_on_the_working_directory_chain() {
        let outer = project();
        fs::write(outer.path().join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let home = outer.path().join("home");
        let repo = home.join("repo");
        fs::create_dir_all(&repo).expect("mkdirs");
        let files = vec![repo.join("q.sql")];

        let from_repo = ConfigEnv {
            home: Some(home.clone()),
            cwd: Some(repo.clone()),
            user_dirs: Vec::new(),
        };
        assert!(
            default_config_needed(&files, &from_repo),
            "config above home is not read"
        );

        let from_outer = ConfigEnv {
            cwd: Some(outer.path().to_path_buf()),
            ..from_repo
        };
        assert!(
            !default_config_needed(&files, &from_outer),
            "the working directory's walk down to the file reads it"
        );
    }

    #[test]
    fn config_in_the_working_directory_counts_for_files_elsewhere() {
        // sqlfluff's root config reads the working directory even when
        // the files live in a sibling directory.
        let home = project();
        let cwd = home.path().join("run");
        let elsewhere = home.path().join("data");
        fs::create_dir_all(&cwd).expect("mkdirs");
        fs::create_dir_all(&elsewhere).expect("mkdirs");
        fs::write(cwd.join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let files = vec![elsewhere.join("q.sql")];
        let env = ConfigEnv {
            home: Some(home.path().to_path_buf()),
            cwd: Some(cwd),
            user_dirs: Vec::new(),
        };
        assert!(!default_config_needed(&files, &env));
    }

    // ---- search directory sets ----------------------------------------

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn search_dirs_for_a_file_under_home_run_between_home_and_the_file() {
        let dirs = file_search_dirs(
            Path::new("/home/u/a/b/c/q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/a/b/c")),
        );
        assert_eq!(
            dirs,
            paths(&["/home/u/a", "/home/u/a/b", "/home/u/a/b/c"]),
            "home and the file's directory are excluded from the home walk"
        );
    }

    #[test]
    fn search_dirs_for_a_file_outside_home_start_below_the_common_ancestor() {
        let dirs = file_search_dirs(
            Path::new("/srv/p/q/x.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/srv/p/q")),
        );
        assert_eq!(dirs, paths(&["/srv", "/srv/p", "/srv/p/q"]));
    }

    #[test]
    fn search_dirs_follow_the_working_directory_from_its_common_ancestor() {
        let dirs = file_search_dirs(
            Path::new("/home/u/a/q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/x/y")),
        );
        assert_eq!(dirs, paths(&["/home/u", "/home/u/a"]));

        let above_home = file_search_dirs(
            Path::new("/home/u/a/q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/")),
        );
        assert_eq!(above_home, paths(&["/", "/home", "/home/u", "/home/u/a"]));
    }

    #[test]
    fn search_dirs_for_a_file_in_home_or_above_it_skip_the_home_walk() {
        let in_home = file_search_dirs(
            Path::new("/home/u/q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(in_home, paths(&["/home/u"]));

        let above = file_search_dirs(
            Path::new("/home/q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home")),
        );
        assert_eq!(above, paths(&["/home"]));
    }

    #[test]
    fn relative_files_resolve_against_the_working_directory() {
        let dirs = file_search_dirs(
            Path::new("sub/q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/p")),
        );
        assert_eq!(
            dirs,
            paths(&["/home/u/p", "/home/u/p", "/home/u/p/sub"]),
            "{dirs:?}"
        );
    }

    #[test]
    fn files_are_normalized_lexically_before_the_search() {
        let dirs = file_search_dirs(
            Path::new("sub/../z.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/p")),
        );
        assert_eq!(dirs, paths(&["/home/u/p"]), "{dirs:?}");

        let dirs = file_search_dirs(
            Path::new("/home/u/p/./a/b/../q.sql"),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/p")),
        );
        assert_eq!(
            dirs,
            paths(&["/home/u/p", "/home/u/p", "/home/u/p/a"]),
            "{dirs:?}"
        );
    }

    #[test]
    fn normalize_matches_python_normpath() {
        for (input, expected) in [
            ("sub/../z.sql", "z.sql"),
            ("./a/./b", "a/b"),
            ("a/..", "."),
            (".", "."),
            ("../a/../../b", "../../b"),
            ("/../a/..", "/"),
            ("/x/../../y", "/y"),
            ("a/b/../../..", ".."),
        ] {
            assert_eq!(
                normalize(Path::new(input)),
                PathBuf::from(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn a_dot_sqlfluff_reached_only_through_dot_dot_does_not_count() {
        let dir = project();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).expect("mkdirs");
        fs::write(sub.join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let files = vec![PathBuf::from("sub/../z.sql")];
        assert!(default_config_needed(&files, &hermetic(dir.path())));
        let files = vec![PathBuf::from("sub/z.sql")];
        assert!(!default_config_needed(&files, &hermetic(dir.path())));
    }

    #[test]
    fn without_a_common_ancestor_only_the_two_ends_are_yielded() {
        // Only reachable in production across Windows drives; a relative
        // inner path has no common component with an absolute one.
        assert_eq!(
            intermediate_dirs(Path::new("a/b"), Path::new("/x/y")),
            paths(&["/x/y", "a/b"])
        );
        // So the home walk contributes nothing.
        assert_eq!(
            dir_search_dirs(Path::new("a/b"), Some(Path::new("/x/y")), None),
            Vec::<PathBuf>::new()
        );
    }

    #[cfg(windows)]
    #[test]
    fn different_windows_drives_share_no_ancestor() {
        let dirs = file_search_dirs(
            Path::new(r"D:\data\q.sql"),
            Some(Path::new(r"C:\Users\u")),
            Some(Path::new(r"C:\Users\u\p")),
        );
        assert_eq!(dirs, paths(&[r"C:\Users\u\p", r"D:\data"]));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_and_directory_case_is_ignored() {
        let dirs = file_search_dirs(
            Path::new(r"c:\users\U\a\b\q.sql"),
            Some(Path::new(r"C:\Users\u")),
            None,
        );
        assert_eq!(dirs, paths(&[r"c:\users\U\a"]));
    }

    #[test]
    fn tools_sqlfluff_args_are_appended_between_defaults_and_files() {
        let dir = project();
        let file = dir.path().join("q.sql");
        let files = vec![file.clone()];
        let mut config = Config::default();
        config.tools.args.insert(
            TOOL.to_string(),
            vec!["--templater".to_string(), "raw".to_string()],
        );

        let args = gated_args(&["format"], &files, &config, dir.path());
        let templater_at = args.iter().position(|a| a == "--templater").expect("flag");
        let dialect_at = args.iter().position(|a| a == "--dialect").expect("flag");
        let file_at = args
            .iter()
            .position(|a| *a == file.to_string_lossy())
            .expect("file");
        assert!(
            dialect_at < templater_at && templater_at < file_at,
            "defaults, then escape hatch (so it can override), then files: {args:?}"
        );
        assert_eq!(args[0], "format", "{args:?}");
        assert!(
            args.contains(&"--disable-progress-bar".to_string()),
            "{args:?}"
        );
    }

    // ---- generated default config ------------------------------------

    /// The `key = value` lines of `text` under the INI section `section`.
    fn section_lines<'a>(text: &'a str, section: &str) -> Vec<&'a str> {
        let header = format!("[{section}]");
        let mut current: Option<&str> = None;
        let mut lines = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                current = Some(line);
            } else if current == Some(header.as_str()) && !line.is_empty() {
                lines.push(line);
            }
        }
        lines
    }

    #[test]
    fn default_config_sets_the_intended_options_in_their_sections() {
        let core = section_lines(DEFAULT_CONFIG, "sqlfluff");
        assert!(
            core.contains(&"large_file_skip_byte_limit = 0"),
            "large files are linted, not skipped: {DEFAULT_CONFIG:?}"
        );
        let identifiers =
            section_lines(DEFAULT_CONFIG, "sqlfluff:rules:capitalisation.identifiers");
        assert!(
            identifiers.contains(&"unquoted_identifiers_policy = none"),
            "identifier capitalisation is left alone: {DEFAULT_CONFIG:?}"
        );
        assert!(
            !core.contains(&"unquoted_identifiers_policy = none"),
            "the policy belongs to the rule section, not the core one: {DEFAULT_CONFIG:?}"
        );
    }

    #[test]
    fn default_config_path_is_passed_after_dialect_and_before_escape_hatch_and_files() {
        let dir = project();
        let file = dir.path().join("q.sql");
        let files = vec![file.clone()];
        let mut config = Config::default();
        config.tools.args.insert(
            TOOL.to_string(),
            vec!["--templater".to_string(), "raw".to_string()],
        );
        let generated = dir.path().join("generated.cfg");

        let args = strings(&build_args(
            &["lint"],
            &files,
            &config,
            Some(generated.as_path()),
        ));
        let config_at = args
            .iter()
            .position(|a| a == "--config")
            .unwrap_or_else(|| panic!("a generated config is passed: {args:?}"));
        assert_eq!(args[config_at + 1], generated.to_string_lossy(), "{args:?}");
        let dialect_at = args.iter().position(|a| a == "--dialect").expect("flag");
        let templater_at = args.iter().position(|a| a == "--templater").expect("flag");
        let file_at = args
            .iter()
            .position(|a| *a == file.to_string_lossy())
            .expect("file");
        assert!(
            dialect_at < config_at && config_at < templater_at && templater_at < file_at,
            "dialect, then generated config, then escape hatch, then files: {args:?}"
        );
    }

    #[test]
    fn neither_default_is_passed_without_a_generated_config() {
        let dir = project();
        let files = vec![dir.path().join("q.sql")];
        let args = strings(&build_args(&["lint"], &files, &Config::default(), None));
        assert!(!args.contains(&"--config".to_string()), "{args:?}");
        assert!(!args.contains(&"--dialect".to_string()), "{args:?}");
    }

    #[test]
    fn written_default_config_holds_exactly_the_default_settings() {
        let file = write_default_config().expect("temp config");
        let written = fs::read_to_string(file.path()).expect("read temp config");
        assert_eq!(written, DEFAULT_CONFIG);
    }

    #[test]
    fn default_config_is_needed_only_without_project_sqlfluff_config() {
        let bare = project();
        let with_dot_sqlfluff = project();
        fs::write(with_dot_sqlfluff.path().join(".sqlfluff"), "[sqlfluff]\n")
            .expect("write config");
        let with_pyproject = project();
        fs::write(
            with_pyproject.path().join("pyproject.toml"),
            "[tool.sqlfluff.core]\ndialect = \"ansi\"\n",
        )
        .expect("write config");

        for (dir, needed) in [
            (&bare, true),
            (&with_dot_sqlfluff, false),
            (&with_pyproject, false),
        ] {
            let files = vec![dir.path().join("q.sql")];
            assert_eq!(
                default_config_needed(&files, &hermetic(dir.path())),
                needed,
                "generated config needed in {}",
                dir.path().display()
            );
            let args = gated_args(&["lint"], &files, &Config::default(), dir.path());
            assert_eq!(
                args.contains(&"--dialect".to_string()),
                needed,
                "dialect passed in {}: {args:?}",
                dir.path().display()
            );
            assert_eq!(
                args.contains(&"--config".to_string()),
                needed,
                "generated config passed in {}: {args:?}",
                dir.path().display()
            );
        }
    }

    // ---- user-level sqlfluff config -----------------------------------

    const ALL_PLATFORMS: [UserConfigPlatform; 3] = [
        UserConfigPlatform::MacOs,
        UserConfigPlatform::Windows,
        UserConfigPlatform::Unix,
    ];

    #[test]
    fn home_directory_is_always_a_user_config_candidate() {
        let home = tempfile::tempdir().expect("tempdir");
        let local = tempfile::tempdir().expect("tempdir");
        for platform in ALL_PLATFORMS {
            for xdg in [None, Some(OsStr::new("")), Some(OsStr::new("/xdg"))] {
                let dirs = user_config_dirs(platform, Some(home.path()), xdg, Some(local.path()));
                assert!(
                    dirs.contains(&home.path().to_path_buf()),
                    "{platform:?} with XDG {xdg:?}: {dirs:?}"
                );
            }
        }
    }

    #[test]
    fn existing_dot_config_sqlfluff_wins_on_every_platform() {
        let home = tempfile::tempdir().expect("tempdir");
        let cross_platform = home.path().join(".config/sqlfluff");
        fs::create_dir_all(&cross_platform).expect("mkdirs");
        let xdg = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(xdg.path().join("sqlfluff")).expect("mkdirs");
        let local = tempfile::tempdir().expect("tempdir");
        for platform in ALL_PLATFORMS {
            let dirs = user_config_dirs(
                platform,
                Some(home.path()),
                Some(xdg.path().as_os_str()),
                Some(local.path()),
            );
            assert_eq!(
                dirs,
                vec![home.path().to_path_buf(), cross_platform.clone()],
                "{platform:?}"
            );
        }
    }

    #[test]
    fn macos_uses_an_existing_xdg_sqlfluff_dir() {
        let home = tempfile::tempdir().expect("tempdir");
        let xdg = tempfile::tempdir().expect("tempdir");
        let xdg_sqlfluff = xdg.path().join("sqlfluff");
        fs::create_dir_all(&xdg_sqlfluff).expect("mkdirs");
        let dirs = user_config_dirs(
            UserConfigPlatform::MacOs,
            Some(home.path()),
            Some(xdg.path().as_os_str()),
            None,
        );
        assert_eq!(dirs, vec![home.path().to_path_buf(), xdg_sqlfluff]);
    }

    #[test]
    fn macos_uses_xdg_config_home_when_set_even_if_missing() {
        let home = tempfile::tempdir().expect("tempdir");
        let missing_xdg = home.path().join("missing-xdg");
        let dirs = user_config_dirs(
            UserConfigPlatform::MacOs,
            Some(home.path()),
            Some(missing_xdg.as_os_str()),
            None,
        );
        assert_eq!(
            dirs,
            vec![home.path().to_path_buf(), missing_xdg.join("sqlfluff")]
        );
    }

    #[test]
    fn macos_falls_back_to_application_support_without_xdg() {
        let home = tempfile::tempdir().expect("tempdir");
        let library = home.path().join("Library/Application Support/sqlfluff");
        for xdg in [None, Some(OsStr::new("")), Some(OsStr::new(" \t\n"))] {
            let dirs = user_config_dirs(UserConfigPlatform::MacOs, Some(home.path()), xdg, None);
            assert_eq!(
                dirs,
                vec![home.path().to_path_buf(), library.clone()],
                "XDG {xdg:?}"
            );
        }
    }

    #[test]
    fn xdg_config_home_is_trimmed_and_blank_means_unset() {
        assert_eq!(xdg_value(OsStr::new("")), None);
        assert_eq!(xdg_value(OsStr::new("  \t\r\n")), None);
        assert_eq!(xdg_value(OsStr::new("\u{1f}\u{a0}")), None);
        assert_eq!(
            xdg_value(OsStr::new("  /xdg\n")),
            Some(PathBuf::from("/xdg"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_xdg_config_home_is_trimmed_of_ascii_whitespace_only() {
        use std::os::unix::ffi::OsStrExt;
        let expected = Some(PathBuf::from(OsStr::from_bytes(b"/xdg\xff")));
        assert_eq!(xdg_value(OsStr::from_bytes(b" /xdg\xff ")), expected);
        assert_eq!(
            xdg_value(OsStr::from_bytes(b"\x1c\x1d\t/xdg\xff\x1e\x1f\x0b\x0c\r\n")),
            expected
        );
        // Non-ASCII bytes are never whitespace here, even 0xa0.
        let raw = OsStr::from_bytes(b"\xa0/x\xff\xa0");
        assert_eq!(xdg_value(raw), Some(PathBuf::from(raw)));
    }

    #[test]
    fn unix_uses_xdg_config_home_when_set_even_if_missing() {
        let home = tempfile::tempdir().expect("tempdir");
        let dirs = user_config_dirs(
            UserConfigPlatform::Unix,
            Some(home.path()),
            Some(OsStr::new(" /xdg ")),
            None,
        );
        assert_eq!(
            dirs,
            vec![home.path().to_path_buf(), PathBuf::from("/xdg/sqlfluff")]
        );
    }

    #[test]
    fn unix_treats_blank_or_unset_xdg_as_dot_config() {
        let home = tempfile::tempdir().expect("tempdir");
        let expected = vec![
            home.path().to_path_buf(),
            home.path().join(".config").join("sqlfluff"),
        ];
        for xdg in [None, Some(OsStr::new("")), Some(OsStr::new("   "))] {
            let dirs = user_config_dirs(UserConfigPlatform::Unix, Some(home.path()), xdg, None);
            assert_eq!(dirs, expected, "XDG {xdg:?}");
        }
    }

    #[test]
    fn windows_uses_the_local_app_data_sqlfluff_dir() {
        let home = tempfile::tempdir().expect("tempdir");
        let local = tempfile::tempdir().expect("tempdir");
        let dirs = user_config_dirs(
            UserConfigPlatform::Windows,
            Some(home.path()),
            Some(OsStr::new("/xdg")),
            Some(local.path()),
        );
        assert_eq!(
            dirs,
            vec![
                home.path().to_path_buf(),
                local.path().join("sqlfluff").join("sqlfluff"),
            ]
        );
    }

    #[test]
    fn default_config_is_needed_without_project_or_user_config() {
        let dir = project();
        let home = tempfile::tempdir().expect("tempdir");
        let files = vec![dir.path().join("q.sql")];
        let user_dirs = vec![
            home.path().to_path_buf(),
            home.path().join(".config/sqlfluff"),
        ];
        assert!(default_config_needed(
            &files,
            &hermetic_with_user_dirs(dir.path(), user_dirs)
        ));
    }

    #[test]
    fn default_config_is_not_needed_with_a_dot_sqlfluff_in_the_user_home() {
        let dir = project();
        let home = tempfile::tempdir().expect("tempdir");
        fs::write(home.path().join(".sqlfluff"), "[sqlfluff]\n").expect("write config");
        let files = vec![dir.path().join("q.sql")];
        let user_dirs = vec![
            home.path().to_path_buf(),
            home.path().join(".config/sqlfluff"),
        ];
        assert!(
            !default_config_needed(&files, &hermetic_with_user_dirs(dir.path(), user_dirs)),
            "the user's own sqlfluff config wins"
        );
    }

    #[test]
    fn default_config_is_not_needed_with_a_sqlfluff_pyproject_in_the_user_appdir() {
        let dir = project();
        let home = tempfile::tempdir().expect("tempdir");
        let appdir = home.path().join(".config/sqlfluff");
        fs::create_dir_all(&appdir).expect("mkdirs");
        fs::write(
            appdir.join("pyproject.toml"),
            "[tool.sqlfluff.core]\ndialect = \"ansi\"\n",
        )
        .expect("write config");
        let files = vec![dir.path().join("q.sql")];
        let user_dirs = vec![home.path().to_path_buf(), appdir];
        assert!(
            !default_config_needed(&files, &hermetic_with_user_dirs(dir.path(), user_dirs)),
            "the user's own sqlfluff config wins"
        );
    }

    #[test]
    fn user_setup_cfg_without_a_sqlfluff_section_does_not_count() {
        let dir = project();
        let home = tempfile::tempdir().expect("tempdir");
        fs::write(home.path().join("setup.cfg"), "[metadata]\nname = x\n").expect("write");
        let files = vec![dir.path().join("q.sql")];
        let user_dirs = vec![home.path().to_path_buf()];
        assert!(default_config_needed(
            &files,
            &hermetic_with_user_dirs(dir.path(), user_dirs)
        ));
    }

    // ---- adapter behavior against a scripted tool --------------------

    /// Write an executable `sqlfluff` shell script whose body is `body`.
    #[cfg(unix)]
    fn fake_sqlfluff(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("sqlfluff");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake sqlfluff");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[cfg(unix)]
    fn ctx_with_script<'a>(
        script: &Path,
        provider: &'a mut Option<FakeToolPaths>,
        config: &'a Config,
    ) -> ToolCtx<'a> {
        *provider = Some(FakeToolPaths::with_tool(TOOL, &script.to_string_lossy()));
        ToolCtx::new(provider.as_ref().expect("just set"), config, false)
    }

    /// A fake sqlfluff that, while it runs, records its arguments, whether
    /// the path after `--config` exists, and that file's contents, then
    /// reports no findings.
    #[cfg(unix)]
    fn recording_sqlfluff(dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let args_record = dir.join("args.txt");
        let exists_record = dir.join("config-exists.txt");
        let contents_record = dir.join("config-contents.txt");
        let script = fake_sqlfluff(
            dir,
            &format!(
                r#"printf '%s\n' "$@" > "{args}"
prev=""
for arg in "$@"; do
  if [ "$prev" = "--config" ]; then
    if [ -f "$arg" ]; then
      echo yes > "{exists}"
      cat "$arg" > "{contents}"
    else
      echo no > "{exists}"
    fi
  fi
  prev="$arg"
done
echo '[]'
exit 0"#,
                args = args_record.display(),
                exists = exists_record.display(),
                contents = contents_record.display()
            ),
        );
        (script, args_record, exists_record, contents_record)
    }

    /// An adapter whose config lookup stays inside `root` and whose user
    /// level sees only a fresh empty home directory, returned alongside it
    /// so it outlives the run.
    #[cfg(unix)]
    fn adapter_with_empty_user_home(root: &Path) -> (tempfile::TempDir, SqlFluffAdapter) {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = SqlFluffAdapter::with_config_env(hermetic_with_user_dirs(
            root,
            vec![
                home.path().to_path_buf(),
                home.path().join(".config/sqlfluff"),
            ],
        ));
        (home, adapter)
    }

    /// An adapter whose config lookup stays inside `root`.
    #[cfg(unix)]
    fn hermetic_adapter(root: &Path) -> SqlFluffAdapter {
        SqlFluffAdapter::with_config_env(hermetic(root))
    }

    #[cfg(unix)]
    #[test]
    fn runs_pass_no_togi_defaults_with_user_level_sqlfluff_config() {
        let dir = project();
        let sql = dir.path().join("q.sql");
        fs::write(&sql, "select 1\n").expect("write");
        let (script, args_record, exists_record, _) = recording_sqlfluff(dir.path());
        let home = tempfile::tempdir().expect("tempdir");
        fs::write(
            home.path().join(".sqlfluff"),
            "[sqlfluff]\ndialect = ansi\n",
        )
        .expect("write config");
        let adapter = SqlFluffAdapter::with_config_env(hermetic_with_user_dirs(
            dir.path(),
            vec![
                home.path().to_path_buf(),
                home.path().join(".config/sqlfluff"),
            ],
        ));

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        adapter
            .lint(std::slice::from_ref(&sql), false, &ctx)
            .expect("lint");

        let args = fs::read_to_string(&args_record).expect("script ran");
        let args: Vec<&str> = args.lines().collect();
        assert!(!args.contains(&"--config"), "{args:?}");
        assert!(!args.contains(&"--dialect"), "{args:?}");
        assert!(!exists_record.exists(), "no config path was passed");
    }

    #[cfg(unix)]
    #[test]
    fn runs_pass_a_live_generated_config_without_project_sqlfluff_config() {
        let dir = project();
        let sql = dir.path().join("q.sql");
        fs::write(&sql, "select 1\n").expect("write");
        let (script, args_record, exists_record, contents_record) = recording_sqlfluff(dir.path());

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let (_home, adapter) = adapter_with_empty_user_home(dir.path());
        let diagnostics = adapter
            .lint(std::slice::from_ref(&sql), false, &ctx)
            .expect("lint");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let args = fs::read_to_string(&args_record).expect("script ran");
        let args: Vec<&str> = args.lines().collect();
        assert!(args.contains(&"--config"), "{args:?}");
        assert!(args.contains(&"--dialect"), "{args:?}");
        let exists = fs::read_to_string(&exists_record).expect("config flag seen");
        assert_eq!(
            exists.trim(),
            "yes",
            "the generated config exists during the run"
        );
        let contents = fs::read_to_string(&contents_record).expect("config copied");
        assert_eq!(contents, DEFAULT_CONFIG);
        assert!(contents.contains("unquoted_identifiers_policy = none"));
        assert!(contents.contains("large_file_skip_byte_limit = 0"));
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_runs_pass_a_live_generated_config_too() {
        let dir = project();
        let sql = dir.path().join("q.sql");
        fs::write(&sql, "select 1\n").expect("write");
        let (script, _, exists_record, contents_record) = recording_sqlfluff(dir.path());

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let (_home, adapter) = adapter_with_empty_user_home(dir.path());
        let outcome = adapter
            .format(std::slice::from_ref(&sql), true, &ctx)
            .expect("format --check");
        assert!(outcome.changed.is_empty(), "{:?}", outcome.changed);

        let exists = fs::read_to_string(&exists_record).expect("config flag seen");
        assert_eq!(
            exists.trim(),
            "yes",
            "the generated config exists during the run"
        );
        let contents = fs::read_to_string(&contents_record).expect("config copied");
        assert_eq!(contents, DEFAULT_CONFIG);
    }

    #[cfg(unix)]
    #[test]
    fn runs_pass_no_togi_defaults_with_project_sqlfluff_config() {
        let dir = project();
        fs::write(dir.path().join(".sqlfluff"), "[sqlfluff]\ndialect = ansi\n")
            .expect("write config");
        let sql = dir.path().join("q.sql");
        fs::write(&sql, "select 1\n").expect("write");
        let (script, args_record, exists_record, _) = recording_sqlfluff(dir.path());

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let (_home, adapter) = adapter_with_empty_user_home(dir.path());
        adapter
            .lint(std::slice::from_ref(&sql), false, &ctx)
            .expect("lint");

        let args = fs::read_to_string(&args_record).expect("script ran");
        let args: Vec<&str> = args.lines().collect();
        assert!(!args.contains(&"--config"), "{args:?}");
        assert!(!args.contains(&"--dialect"), "{args:?}");
        assert!(!exists_record.exists(), "no config path was passed");
    }

    #[cfg(unix)]
    #[test]
    fn format_reports_only_content_changed_files() {
        let dir = project();
        let messy = dir.path().join("messy.sql");
        let clean = dir.path().join("clean.sql");
        fs::write(&messy, "select  1\n").expect("write");
        fs::write(&clean, "select 1\n").expect("write");
        // The script rewrites messy.sql only, like a real formatter would.
        let script = fake_sqlfluff(
            dir.path(),
            r#"for arg in "$@"; do
  case "$arg" in
    *messy.sql) printf 'select 1\n' > "$arg" ;;
  esac
done"#,
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let outcome = hermetic_adapter(dir.path())
            .format(&[messy.clone(), clean.clone()], false, &ctx)
            .expect("format");

        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.changed, vec![messy]);
    }

    #[cfg(unix)]
    #[test]
    fn format_check_lints_with_the_format_rule_subset() {
        let dir = project();
        let record = dir.path().join("record.txt");
        let json = dir.path().join("out.json");
        fs::write(&json, fixture("lint-violations.json")).expect("write json");
        let sql = dir.path().join("events.sql");
        fs::write(&sql, "select 1\n").expect("write");
        // Real `lint` exits 1 when it finds violations; check mode must
        // tolerate that.
        let script = fake_sqlfluff(
            dir.path(),
            &format!(
                r#"printf '%s ' "$@" > "{record}"
cat "{json}"
exit 1"#,
                record = record.display(),
                json = json.display()
            ),
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let outcome = hermetic_adapter(dir.path())
            .format(&[sql], true, &ctx)
            .expect("format --check");

        assert_eq!(outcome.processed, 1);
        assert_eq!(outcome.changed, vec![PathBuf::from("events.sql")]);
        let recorded = fs::read_to_string(&record).expect("script ran");
        assert!(recorded.starts_with("lint "), "{recorded}");
        assert!(recorded.contains("--format json"), "{recorded}");
        assert!(
            recorded.contains(&format!("--rules {FORMAT_RULES}")),
            "check mode mirrors the format rule subset: {recorded}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn format_surfaces_templating_and_parse_failures() {
        let dir = project();
        let sql = dir.path().join("broken.sql");
        fs::write(&sql, "SELECT FROM WHERE (\n").expect("write");
        // Real `format` exits 1 (with a stderr note) on parse errors.
        let script = fake_sqlfluff(
            dir.path(),
            r#"echo "  [1 templating/parsing errors found]" >&2
exit 1"#,
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let err = hermetic_adapter(dir.path())
            .format(&[sql], false, &ctx)
            .expect_err("parse errors must fail the format run");
        let rendered = crate::term::render_error(&err, false);
        assert!(
            rendered.contains("templating/parsing errors"),
            "keeps sqlfluff's own report: {rendered}"
        );
        assert!(
            rendered.contains("togi lint"),
            "says what to do: {rendered}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_fix_runs_fix_then_reports_remaining_findings() {
        let dir = project();
        let record = dir.path().join("record.txt");
        let json = dir.path().join("out.json");
        fs::write(&json, fixture("lint-violations.json")).expect("write json");
        let sql = dir.path().join("events.sql");
        fs::write(&sql, "select 1\n").expect("write");
        // `fix` exits 1 when unfixable violations remain; that is not an
        // error, the follow-up lint reports them.
        let script = fake_sqlfluff(
            dir.path(),
            &format!(
                r#"printf '%s\n' "$1" >> "{record}"
if [ "$1" = "lint" ]; then
  cat "{json}"
fi
exit 1"#,
                record = record.display(),
                json = json.display()
            ),
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let diagnostics = hermetic_adapter(dir.path())
            .lint(&[sql], true, &ctx)
            .expect("lint --fix");

        assert_eq!(diagnostics.len(), 5);
        let recorded = fs::read_to_string(&record).expect("script ran");
        assert_eq!(recorded, "fix\nlint\n", "fix first, then lint");
    }

    #[cfg(unix)]
    #[test]
    fn lint_tolerates_exit_one_and_parses_diagnostics() {
        let dir = project();
        let json = dir.path().join("out.json");
        fs::write(&json, fixture("lint-parse-error.json")).expect("write json");
        let sql = dir.path().join("broken.sql");
        fs::write(&sql, "SELECT FROM WHERE (\n").expect("write");
        let script = fake_sqlfluff(dir.path(), &format!("cat \"{}\"\nexit 1", json.display()));

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let diagnostics = hermetic_adapter(dir.path())
            .lint(&[sql], false, &ctx)
            .expect("lint");

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code.as_deref(), Some("PRS"));
    }

    #[cfg(unix)]
    #[test]
    fn usage_errors_name_the_config_escape_hatches() {
        let dir = project();
        let sql = dir.path().join("q.sql");
        fs::write(&sql, "select 1\n").expect("write");
        // Exit 2 is sqlfluff's usage/config error (e.g. unknown dialect).
        let script = fake_sqlfluff(
            dir.path(),
            r#"echo "User Error: Unknown dialect 'bigquery2'" >&2
exit 2"#,
        );

        let mut provider = None;
        let config = Config::default();
        let ctx = ctx_with_script(&script, &mut provider, &config);
        let err = hermetic_adapter(dir.path())
            .lint(&[sql], false, &ctx)
            .expect_err("usage errors must fail the run");
        let rendered = crate::term::render_error(&err, false);
        assert!(rendered.contains("Unknown dialect"), "{rendered}");
        assert!(rendered.contains("togi.toml"), "{rendered}");
    }

    // ---- batch hygiene ------------------------------------------------

    #[test]
    fn empty_batches_never_invoke_the_tool() {
        // sqlfluff with no paths would lint the whole working directory;
        // an empty batch must not even resolve the binary.
        let provider = FakeToolPaths::default();
        let config = Config::default();
        let ctx = ToolCtx::new(&provider, &config, false);

        let outcome = SqlFluffAdapter::new()
            .format(&[], false, &ctx)
            .expect("format");
        assert_eq!(outcome, FormatOutcome::default());
        let outcome = SqlFluffAdapter::new()
            .format(&[], true, &ctx)
            .expect("check");
        assert_eq!(outcome, FormatOutcome::default());
        let diagnostics = SqlFluffAdapter::new().lint(&[], true, &ctx).expect("lint");
        assert!(diagnostics.is_empty());
        assert!(provider.requests().is_empty(), "no tool resolution");
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;
    use crate::adapters::{Formatter, Linter, ToolPaths};
    use crate::tools::{InstallContext, Platform, ToolCache, ToolSpec, UvToolInstaller, versions};

    /// A provider that hands out one pre-installed binary.
    struct FixedToolPaths {
        binary: PathBuf,
    }

    impl ToolPaths for FixedToolPaths {
        fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
            assert_eq!(tool, TOOL);
            Ok(self.binary.clone())
        }
    }

    /// Installs real sqlfluff (bootstrapping uv) into a temp cache, then
    /// drives the adapter end to end: lint, format --check, format,
    /// lint --fix.
    /// Run with: `cargo test --features online-tests -- --ignored`
    #[test]
    #[ignore = "downloads real uv and sqlfluff from the network"]
    fn real_sqlfluff_formats_lints_and_fixes_bigquery_sql() {
        let tools_dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(tools_dir.path());
        let platform = Platform::current().expect("supported platform");
        let spec = ToolSpec::builtin(TOOL).expect("sqlfluff is built in");
        let install_ctx = InstallContext {
            label: "SQL linter",
            command: "togi lint",
            verbose: true,
        };
        let binary = UvToolInstaller::new(cache, platform, versions::UV.to_string())
            .ensure_installed(&spec, spec.default_version, &install_ctx)
            .expect("bootstrap uv and install sqlfluff");

        let project = tempfile::tempdir().expect("tempdir");
        let messy = project.path().join("messy.sql");
        fs::write(
            &messy,
            "select event_id,\n    user_id ,\n  event_timestamp\n\
             from `analytics.events`\nWHERE event_date != '2024-01-01'\n",
        )
        .expect("write messy.sql");
        let star = project.path().join("star.sql");
        fs::write(&star, "select * from `analytics.events`\n").expect("write star.sql");
        let files = vec![messy.clone(), star.clone()];

        let provider = FixedToolPaths { binary };
        let mut config = Config::default();
        // sqlfluff itself would still read config from the real home and
        // working directory; this flag limits it to the dialect and the
        // generated `--config` togi passes.
        config
            .tools
            .args
            .insert(TOOL.to_string(), vec!["--ignore-local-config".to_string()]);
        let ctx = ToolCtx::new(&provider, &config, true);
        // Neither the developer's home nor anything above the project
        // may configure sqlfluff for this run.
        let adapter = SqlFluffAdapter::with_config_env(ConfigEnv {
            home: Some(project.path().to_path_buf()),
            cwd: Some(project.path().to_path_buf()),
            user_dirs: Vec::new(),
        });

        // Lint sees real violations under the default bigquery dialect.
        let diagnostics = adapter.lint(&files, false, &ctx).expect("lint");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.path == messy && d.code.as_deref() == Some("LT02")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.path == star && d.code.as_deref() == Some("AM04")),
            "{diagnostics:?}"
        );

        // Check mode: only the messy file would change.
        let outcome = adapter.format(&files, true, &ctx).expect("format --check");
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.changed, vec![messy.clone()]);

        // Formatting rewrites it in place...
        let outcome = adapter.format(&files, false, &ctx).expect("format");
        assert_eq!(outcome.changed, vec![messy.clone()]);
        // ...after which check mode is clean.
        let outcome = adapter.format(&files, true, &ctx).expect("re-check");
        assert!(outcome.changed.is_empty(), "{outcome:?}");

        // lint --fix applies what it can and reports the unfixable rest
        // (star.sql's `select *`).
        let remaining = adapter.lint(&files, true, &ctx).expect("lint --fix");
        assert!(
            remaining
                .iter()
                .any(|d| d.path == star && d.code.as_deref() == Some("AM04")),
            "{remaining:?}"
        );
        assert!(remaining.iter().all(|d| !d.fixable), "{remaining:?}");
    }
}
