//! Project-selected togi runtimes and pre-Clap dispatch.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use togi_core::term::{self, HintExt};
use togi_core::tools::{Downloader, InstallContext, Platform, ToolCache};

const MARKER_VERSION: &str = "__TOGI_SELECTED_VERSION";
const MARKER_EXE: &str = "__TOGI_SELECTED_EXE";
const MARKER_TOKEN: &str = "__TOGI_SELECTED_TOKEN";
const TOKEN_PREFIX: &str = ".togi-dispatch-";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub enum Bootstrap {
    Run {
        args: Vec<OsString>,
        override_version: Option<String>,
    },
}

pub fn bootstrap(args: Vec<OsString>) -> anyhow::Result<Bootstrap> {
    let (args, override_version) = take_override(args)?;
    let current_exe = std::env::current_exe()
        .context("could not determine the path to the running togi executable")?;
    let immediate_child = validate_marker(
        std::env::var_os(MARKER_VERSION).as_deref(),
        std::env::var_os(MARKER_EXE).as_deref(),
        std::env::var_os(MARKER_TOKEN).as_deref(),
        &current_exe,
        override_version.is_some(),
    )?;
    if immediate_child {
        return Ok(Bootstrap::Run {
            args,
            override_version: None,
        });
    }
    if is_control_command(&args) {
        return Ok(Bootstrap::Run {
            args,
            override_version,
        });
    }

    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let selected = match override_version {
        Some(version) => Some(version),
        None => match nearest_lock(&cwd) {
            Some(path) => Some(read_lock(&path)?),
            None => None,
        },
    };
    let Some(version) = selected else {
        return Ok(Bootstrap::Run {
            args,
            override_version: None,
        });
    };
    if version == CURRENT {
        return Ok(Bootstrap::Run {
            args,
            override_version: None,
        });
    }

    let binary = ensure_runtime(&version, false)?;
    let canonical_binary = binary
        .canonicalize()
        .with_context(|| format!("could not resolve selected togi `{}`", binary.display()))?;
    let (token_name, token_path) = create_dispatch_token()?;
    let mut command = Command::new(&binary);
    command
        .args(args.iter().skip(1))
        .env(MARKER_VERSION, &version)
        .env(MARKER_EXE, &canonical_binary)
        .env(MARKER_TOKEN, &token_name);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        let _ = std::fs::remove_file(&token_path);
        Err(anyhow::Error::new(error))
            .with_context(|| format!("could not run selected togi {version}"))
            .hint("remove the selected runtime cache entry and retry")
    }
    #[cfg(not(unix))]
    {
        let result = command
            .status()
            .with_context(|| format!("could not run selected togi {version}"))
            .hint("remove the selected runtime cache entry and retry");
        let _ = std::fs::remove_file(&token_path);
        let status = result?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn validate_marker(
    marker_version: Option<&OsStr>,
    marker_exe: Option<&OsStr>,
    marker_token: Option<&OsStr>,
    current_exe: &Path,
    has_override: bool,
) -> anyhow::Result<bool> {
    let (Some(marker_version), Some(marker_exe), Some(marker_token)) =
        (marker_version, marker_exe, marker_token)
    else {
        return Ok(false);
    };
    let current = current_exe
        .canonicalize()
        .unwrap_or_else(|_| current_exe.to_path_buf());
    let marked = Path::new(marker_exe)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(marker_exe));
    if marked != current {
        return Ok(false);
    }
    let Some(token_path) = dispatch_token_path(marker_token) else {
        return Ok(false);
    };
    match std::fs::remove_file(&token_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(anyhow::Error::new(error))
                .with_context(|| format!("could not consume `{}`", token_path.display()))
                .hint("check the temporary directory's permissions and retry");
        }
    }
    if marker_version != OsStr::new(CURRENT) {
        return Err(crate::cli::usage_error(
            format!(
                "selected togi runtime marker `{}` does not match this executable ({CURRENT})",
                marker_version.to_string_lossy()
            ),
            "remove the selected runtime cache entry and retry",
        ));
    }
    Ok(!has_override)
}

fn create_dispatch_token() -> anyhow::Result<(OsString, PathBuf)> {
    let temp_dir = std::env::temp_dir();
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    sweep_dispatch_tokens(&temp_dir, cutoff);
    let token = tempfile::Builder::new()
        .prefix(TOKEN_PREFIX)
        .tempfile_in(&temp_dir)
        .context("could not create a togi dispatch token")
        .hint("check the temporary directory's permissions and retry")?;
    let (_file, path) = token
        .keep()
        .map_err(|error| anyhow::Error::new(error.error))
        .context("could not preserve the togi dispatch token")
        .hint("check the temporary directory's permissions and retry")?;
    let name = path
        .file_name()
        .expect("temporary token has a filename")
        .to_os_string();
    Ok((name, path))
}

fn sweep_dispatch_tokens(dir: &Path, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            continue;
        };
        if !text.starts_with(TOKEN_PREFIX) || text.len() == TOKEN_PREFIX.len() {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn dispatch_token_path(name: &OsStr) -> Option<PathBuf> {
    let path = Path::new(name);
    let text = name.to_str()?;
    if !text.starts_with(TOKEN_PREFIX)
        || text.len() == TOKEN_PREFIX.len()
        || path.file_name() != Some(name)
        || path.components().count() != 1
    {
        return None;
    }
    Some(std::env::temp_dir().join(name))
}

pub fn pin(
    requested: Option<&str>,
    override_version: Option<&str>,
    verbose: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let existing = nearest_lock(&cwd);
    let requested = requested
        .map(|raw| parse_version(raw, "version passed to `togi pin`"))
        .transpose()?;
    let override_version = override_version
        .map(|raw| parse_version(raw, "`--with-version`"))
        .transpose()?;
    if let (Some(requested), Some(override_version)) = (&requested, &override_version)
        && requested != override_version
    {
        return Err(crate::cli::usage_error(
            format!(
                "conflicting togi versions `{requested}` and `{override_version}` were requested"
            ),
            "pass the version either as `togi pin VERSION` or with `--with-version`, not both",
        ));
    }
    let selected = requested.or(override_version);
    let version = match selected {
        Some(raw) => {
            let version = raw;
            if version != CURRENT {
                ensure_runtime(&version, verbose)?;
            }
            version
        }
        None => match existing.as_deref() {
            Some(path) => read_lock(path)?,
            None => CURRENT.to_string(),
        },
    };
    let root = existing
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project_root(&cwd));
    write_lock(&root.join(".togi-version"), &version)?;
    term::success(&format!("pinned togi {version}"));
    Ok(())
}

pub fn unpin() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    if let Some(path) = nearest_lock(&cwd) {
        std::fs::remove_file(&path)
            .with_context(|| format!("could not remove `{}`", path.display()))
            .hint("check the file's permissions and retry")?;
    }
    Ok(())
}

fn take_override(args: Vec<OsString>) -> anyhow::Result<(Vec<OsString>, Option<String>)> {
    let mut kept = Vec::with_capacity(args.len());
    let mut selected = None;
    let mut iter = args.into_iter();
    if let Some(program) = iter.next() {
        kept.push(program);
    }
    let mut literal = false;
    while let Some(arg) = iter.next() {
        if literal {
            kept.push(arg);
            continue;
        }
        if arg == "--" {
            literal = true;
            kept.push(arg);
            continue;
        }
        let value = if arg == "--with-version" {
            Some(iter.next().ok_or_else(|| {
                crate::cli::usage_error(
                    "`--with-version` requires a version",
                    "pass an exact version such as `--with-version 0.1.1`",
                )
            })?)
        } else {
            arg.to_str()
                .and_then(|s| s.strip_prefix("--with-version="))
                .map(OsString::from)
        };
        if let Some(value) = value {
            if selected.is_some() {
                return Err(crate::cli::usage_error(
                    "`--with-version` may only be passed once",
                    "remove the duplicate version selector and retry",
                ));
            }
            let value = value.to_str().ok_or_else(|| {
                crate::cli::usage_error(
                    "`--with-version` is not valid UTF-8",
                    "pass an exact numeric version such as `0.1.1`",
                )
            })?;
            selected = Some(parse_version(value, "`--with-version`")?);
        } else {
            kept.push(arg);
        }
    }
    Ok((kept, selected))
}

fn parse_version(raw: &str, source: &str) -> anyhow::Result<String> {
    let value = raw.strip_prefix('v').unwrap_or(raw);
    let mut parts = value.split('.');
    let valid = !value.is_empty()
        && parts.by_ref().take(3).all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        })
        && parts.next().is_none()
        && value.split('.').count() == 3;
    if !valid {
        return Err(crate::cli::usage_error(
            format!("invalid togi version `{raw}` in {source}"),
            "use an exact stable version with three numeric components, such as `0.1.1`",
        ));
    }
    Ok(value.to_string())
}

fn read_lock(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read `{}`", path.display()))
        .hint("check the file's permissions and retry")?;
    let raw = text.strip_suffix('\n').unwrap_or(&text);
    if raw.contains(['\n', '\r']) || (text.ends_with('\n') && raw.ends_with('\r')) {
        return Err(crate::cli::usage_error(
            format!("invalid togi version in `{}`", path.display()),
            "write one exact version such as `0.1.1` followed by a newline",
        ));
    }
    parse_version(raw, &format!("`{}`", path.display()))
}

fn nearest_lock(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let lock = dir.join(".togi-version");
        if lock.is_file() {
            return Some(lock);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

fn project_root(cwd: &Path) -> PathBuf {
    for dir in cwd.ancestors() {
        if dir.join("togi.toml").is_file() {
            return dir.to_path_buf();
        }
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

fn write_lock(path: &Path, version: &str) -> anyhow::Result<()> {
    let parent = path.parent().expect("version lock has a parent");
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| {
            format!(
                "could not create a temporary file in `{}`",
                parent.display()
            )
        })
        .hint("check the project's permissions and retry")?;
    use std::io::Write;
    writeln!(file, "{version}").context("could not write the project version lock")?;
    file.persist(path)
        .map_err(|error| anyhow::Error::new(error.error))
        .with_context(|| format!("could not write `{}`", path.display()))
        .hint("check the project's permissions and retry")?;
    Ok(())
}

fn ensure_runtime(version: &str, verbose: bool) -> anyhow::Result<PathBuf> {
    let cache = ToolCache::versions_from_env()?;
    let platform = Platform::current()?;
    let spec = crate::cli::upgrade::self_spec();
    let downloader = Downloader::new(cache, platform);
    let ctx = InstallContext {
        label: "togi runtime",
        command: "togi pin",
        verbose,
    };
    let binary = downloader
        .ensure_verified_installed(&spec, version, &ctx)
        .with_context(|| format!("could not install requested togi {version}"))
        .hint("check that the release exists and that the network is available, then retry")?;
    Ok(binary)
}

fn is_control_command(args: &[OsString]) -> bool {
    let mut skip_value = false;
    for arg in args.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg == "--" {
            return false;
        }
        if arg == "--config" {
            skip_value = true;
            continue;
        }
        if arg.to_string_lossy().starts_with('-') {
            continue;
        }
        return matches!(arg.to_str(), Some("pin" | "unpin" | "upgrade"));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_versions_accept_only_stable_triplets() {
        assert_eq!(parse_version("1.2.3", "test").unwrap(), "1.2.3");
        assert_eq!(parse_version("v1.2.3", "test").unwrap(), "1.2.3");
        for value in [
            "",
            "v",
            "latest",
            "1.2",
            "1.2.3.4",
            "1.2.3-rc.1",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            " 1.2.3",
            "1.2.3 ",
        ] {
            assert!(parse_version(value, "test").is_err(), "{value}");
        }
    }

    #[test]
    fn selector_is_removed_only_before_double_dash() {
        let args = [
            "togi",
            "lint",
            "--with-version=1.2.3",
            "--",
            "--with-version",
            "x",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let (kept, selected) = take_override(args).unwrap();
        assert_eq!(selected.as_deref(), Some("1.2.3"));
        assert_eq!(kept, ["togi", "lint", "--", "--with-version", "x"]);
    }

    #[test]
    fn project_root_prefers_config_then_git_then_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let nested = repo.join("a/b");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(project_root(&nested), repo);
        std::fs::write(repo.join("a/togi.toml"), "").unwrap();
        assert_eq!(project_root(&nested), repo.join("a"));
    }

    #[test]
    fn identifies_only_control_commands() {
        for command in ["pin", "unpin", "upgrade"] {
            assert!(is_control_command(&["togi".into(), command.into()]));
        }
        assert!(!is_control_command(&[
            "togi".into(),
            "lint".into(),
            "upgrade".into()
        ]));
    }

    #[test]
    fn marker_mismatch_is_an_error_without_redispatch() {
        let temp = tempfile::tempdir().unwrap();
        let direct = temp.path().join("direct");
        let other = temp.path().join("other");
        std::fs::write(&direct, "").unwrap();
        std::fs::write(&other, "").unwrap();
        assert!(!validate_marker(None, None, None, &direct, false).unwrap());

        let (token, _) = create_dispatch_token().unwrap();
        assert!(
            validate_marker(
                Some(OsStr::new(CURRENT)),
                Some(direct.as_os_str()),
                Some(&token),
                &direct,
                false,
            )
            .unwrap()
        );

        let (token, token_path) = create_dispatch_token().unwrap();
        assert!(
            !validate_marker(
                Some(OsStr::new(CURRENT)),
                Some(direct.as_os_str()),
                Some(&token),
                &direct,
                true,
            )
            .unwrap()
        );
        assert!(
            !token_path.exists(),
            "a fresh override consumes the one-use token"
        );

        let (token, token_path) = create_dispatch_token().unwrap();
        assert!(
            !validate_marker(
                Some(OsStr::new("9.9.9")),
                Some(other.as_os_str()),
                Some(&token),
                &direct,
                false,
            )
            .unwrap()
        );
        assert!(
            token_path.exists(),
            "a different executable cannot consume the token"
        );
        std::fs::remove_file(token_path).unwrap();

        let (token, _) = create_dispatch_token().unwrap();
        let error = validate_marker(
            Some(OsStr::new("9.9.9")),
            Some(direct.as_os_str()),
            Some(&token),
            &direct,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn token_sweep_removes_stale_matching_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join(".togi-dispatch-stale");
        std::fs::write(&stale, "").unwrap();
        let modified = std::fs::metadata(&stale).unwrap().modified().unwrap();

        sweep_dispatch_tokens(temp.path(), modified + Duration::from_secs(1));

        assert!(!stale.exists());
    }

    #[test]
    fn token_sweep_retains_recent_and_future_dated_files() {
        let temp = tempfile::tempdir().unwrap();
        let recent = temp.path().join(".togi-dispatch-recent");
        let future = temp.path().join(".togi-dispatch-future");
        std::fs::write(&recent, "").unwrap();
        std::fs::write(&future, "").unwrap();
        let modified = std::fs::metadata(&recent).unwrap().modified().unwrap();

        sweep_dispatch_tokens(
            temp.path(),
            modified.checked_sub(Duration::from_secs(1)).unwrap(),
        );
        sweep_dispatch_tokens(temp.path(), SystemTime::UNIX_EPOCH);

        assert!(recent.exists());
        assert!(future.exists());
    }

    #[test]
    fn token_sweep_retains_unrelated_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        let unrelated = temp.path().join("unrelated");
        let directory = temp.path().join(".togi-dispatch-directory");
        std::fs::write(&unrelated, "").unwrap();
        std::fs::create_dir(&directory).unwrap();

        sweep_dispatch_tokens(temp.path(), SystemTime::now() + Duration::from_secs(1));

        assert!(unrelated.exists());
        assert!(directory.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn token_sweep_does_not_follow_or_remove_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join(".togi-dispatch-link");
        std::fs::write(&target, "").unwrap();
        symlink(&target, &link).unwrap();

        sweep_dispatch_tokens(temp.path(), SystemTime::now() + Duration::from_secs(1));

        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(target.exists());
    }
}
