//! Integration tests for the CLI skeleton.
//!
//! Covers: `--help` snapshots for every command, global flags, `togi
//! version`, and `togi completions`.

use assert_cmd::Command;
use predicates::prelude::*;

fn togi() -> Command {
    Command::cargo_bin("togi").expect("togi binary should build")
}

fn help_output(args: &[&str]) -> String {
    let assert = togi().args(args).assert().success();
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("help output should be UTF-8");
    // clap renders the usage line from `argv[0]`, which is `togi.exe` on
    // Windows and `togi` elsewhere; normalize the extension away so one set
    // of snapshots is canonical across platforms.
    stdout.replace("togi.exe", "togi")
}

/// Snapshot `togi <args...> --help` under the test's name.
macro_rules! help_snapshot {
    ($name:ident $(, $arg:literal)*) => {
        #[test]
        fn $name() {
            insta::assert_snapshot!(help_output(&[$($arg,)* "--help"]));
        }
    };
}

help_snapshot!(help_root);
help_snapshot!(help_format, "format");
help_snapshot!(help_lint, "lint");
help_snapshot!(help_tools, "tools");
help_snapshot!(help_tools_list, "tools", "list");
help_snapshot!(help_tools_update, "tools", "update");
help_snapshot!(help_tools_clean, "tools", "clean");
help_snapshot!(help_completions, "completions");
help_snapshot!(help_version, "version");
help_snapshot!(help_upgrade, "upgrade");

#[test]
fn root_help_advertises_the_fmt_alias() {
    // The alias is discoverable, not a hidden easter egg.
    let help = help_output(&["--help"]);
    assert!(
        help.contains("[aliases: fmt]"),
        "`togi --help` should name the `fmt` alias:\n{help}"
    );
}

#[test]
fn fmt_is_an_alias_for_format() {
    // The alias must parse to the same command; --help output proves the
    // route without touching any files.
    let fmt = help_output(&["fmt", "--help"]);
    let format = help_output(&["format", "--help"]);
    assert_eq!(fmt, format);
}

#[test]
fn version_command_prints_version() {
    togi()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_flag_prints_version() {
    togi()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn completions_generates_a_bash_script() {
    togi()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_togi"));
}

#[test]
fn completions_requires_a_shell_argument() {
    togi().arg("completions").assert().code(2);
}

#[test]
fn global_flags_parse_before_the_subcommand() {
    togi()
        .args([
            "--verbose",
            "--quiet",
            "--no-color",
            "--config",
            "togi.toml",
            "version",
        ])
        .assert()
        .success();
}

#[test]
fn global_flags_parse_after_the_subcommand() {
    // Global flags must also be accepted in subcommand position: `version`
    // runs offline and touches nothing, so the flags parsing after it is
    // what this exercises.
    togi()
        .args(["version", "-v", "-q", "--no-color", "--config", "togi.toml"])
        .assert()
        .success();
}

#[test]
fn no_arguments_shows_help_and_exits_2() {
    togi()
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage:"));
}

#[test]
fn unknown_command_exits_2() {
    togi().arg("frobnicate").assert().code(2);
}

#[test]
fn tools_without_subcommand_exits_2() {
    togi().arg("tools").assert().code(2);
}
