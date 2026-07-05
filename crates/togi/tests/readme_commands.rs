//! Every `togi ...` command shown in README.md must actually exist and parse.
//!
//! Commands are extracted from fenced code blocks and re-run with `--help`
//! appended: clap prints help and exits 0 only when the subcommand path and
//! every flag/value on the line parse against the real CLI, and exits 2
//! otherwise. In ```console blocks only `$ `-prompted lines count (the rest
//! is program output); in other fences every `togi` line counts. Lines that
//! should not be verified opt out with a trailing `# no-verify` comment.

use assert_cmd::Command;

/// Marker comment that exempts a code-block line from verification.
const NO_VERIFY: &str = "# no-verify";

fn readme() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");
    std::fs::read_to_string(path).expect("README.md should exist at the repository root")
}

/// Extract the `togi` invocations from every fenced code block.
///
/// Inside a ```console fence, a line is an invocation only when it carries
/// the `$ ` prompt (unprompted lines are program output). In any other
/// fence, a line counts when it starts with `togi ` (or is exactly `togi`),
/// with an optional `$ ` prompt stripped first. Trailing `# ...` comments
/// are dropped; lines carrying the no-verify marker are skipped entirely.
fn extract_commands(markdown: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut fence: Option<String> = None;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(info) = trimmed.strip_prefix("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(info.trim().to_string()),
            };
            continue;
        }
        let Some(info) = &fence else { continue };
        if trimmed.contains(NO_VERIFY) {
            continue;
        }
        let prompted = trimmed.strip_prefix("$ ");
        if info == "console" && prompted.is_none() {
            continue;
        }
        let candidate = prompted.unwrap_or(trimmed);
        if candidate != "togi" && !candidate.starts_with("togi ") {
            continue;
        }
        // Drop any trailing explanatory comment (`togi format   # in place`).
        let command = match candidate.find(" #") {
            Some(idx) => candidate[..idx].trim_end(),
            None => candidate,
        };
        commands.push(command.to_string());
    }
    commands
}

#[test]
fn readme_shows_a_healthy_number_of_togi_commands() {
    let commands = extract_commands(&readme());
    assert!(
        commands.len() >= 8,
        "expected the README to show at least 8 togi commands, \
         found {}: {commands:#?}",
        commands.len()
    );
}

#[test]
fn every_readme_togi_command_parses() {
    let commands = extract_commands(&readme());
    for command in &commands {
        assert!(
            !command.contains(['"', '\'']),
            "README command `{command}` uses shell quoting, which this test \
             does not interpret; rewrite it without quotes or mark the line \
             `{NO_VERIFY}`"
        );
        let args: Vec<&str> = command.split_whitespace().skip(1).collect();
        let assert = Command::cargo_bin("togi")
            .expect("togi binary should build")
            .args(&args)
            .arg("--help")
            .assert();
        let output = assert.get_output();
        assert!(
            output.status.success(),
            "README command `{command}` does not parse against the real CLI \
             (`togi {} --help` failed):\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn extraction_reads_prompts_comments_and_no_verify_markers() {
    let sample = "\
Run `togi format` to format (prose, not extracted).

```console
$ togi format --check
2 of 14 files would be reformatted
togi is not a command here, just output
$ togi lint --fix   # autofix what we can
$ togi version
togi 0.1.0
```

togi lint outside any fence is not extracted.

```sh
togi tools update
togi upgrade --dry-run   # no-verify
```
";
    assert_eq!(
        extract_commands(sample),
        vec![
            "togi format --check",
            "togi lint --fix",
            "togi version",
            "togi tools update",
        ]
    );
}
