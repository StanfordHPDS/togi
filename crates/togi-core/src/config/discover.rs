//! Where config files live: walk up from CWD for `togi.toml`,
//! platform user-config dir for `config.toml`.

use std::path::{Path, PathBuf};

/// Find the project `togi.toml`: walk up from `start` checking each
/// directory, stopping after the git root (a directory containing `.git`,
/// which may be a file in worktrees) or at the filesystem root.
pub(crate) fn find_project_config(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join("togi.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            // Reached the git root without finding a config; a togi.toml in
            // some unrelated parent directory must not leak in.
            return None;
        }
    }
    None
}

/// The user-level config file (`config.toml` in the platform config dir for
/// `togi`, e.g. `~/.config/togi` on Linux). `None` when no home directory
/// can be determined.
pub(crate) fn user_config_path() -> Option<PathBuf> {
    user_config_dir().map(|dir| dir.join("config.toml"))
}

fn user_config_dir() -> Option<PathBuf> {
    // Internal override so tests can isolate the user-config layer without
    // faking the whole platform home directory. Not documented for users.
    if let Some(dir) = std::env::var_os("TOGI_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "togi").map(|dirs| dirs.config_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_togi_toml_in_the_start_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("togi.toml");
        fs::write(&config, "").expect("write togi.toml");
        assert_eq!(find_project_config(dir.path()), Some(config));
    }

    #[test]
    fn walks_up_to_find_togi_toml_in_a_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("togi.toml");
        fs::write(&config, "").expect("write togi.toml");
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("create nested dirs");
        assert_eq!(find_project_config(&nested), Some(config));
    }

    #[test]
    fn stops_at_the_git_root() {
        // <tmp>/togi.toml  <- must NOT be found
        // <tmp>/repo/.git  <- git root
        // <tmp>/repo/sub   <- start
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("togi.toml"), "").expect("write outer togi.toml");
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("create .git dir");
        let sub = repo.join("sub");
        fs::create_dir_all(&sub).expect("create sub dir");
        assert_eq!(find_project_config(&sub), None);
    }

    #[test]
    fn git_root_itself_may_hold_the_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("create .git dir");
        let config = repo.join("togi.toml");
        fs::write(&config, "").expect("write togi.toml");
        let sub = repo.join("sub");
        fs::create_dir_all(&sub).expect("create sub dir");
        assert_eq!(find_project_config(&sub), Some(config));
    }

    #[test]
    fn a_git_file_marks_the_root_too() {
        // Linked worktrees have a `.git` file, not a directory.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("togi.toml"), "").expect("write outer togi.toml");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("create repo dir");
        fs::write(repo.join(".git"), "gitdir: elsewhere").expect("write .git file");
        assert_eq!(find_project_config(&repo), None);
    }

    #[test]
    fn returns_none_when_nothing_is_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("create .git dir");
        assert_eq!(find_project_config(&repo), None);
    }
}
