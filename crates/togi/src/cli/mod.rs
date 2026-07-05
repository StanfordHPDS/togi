//! Clap derive command tree: one file per top-level command.

mod completions;
mod fmt_lint;
mod format;
mod lint;
mod tools;
mod upgrade;
mod version;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use togi_core::term;

/// Polyglot formatter and linter for data science projects: R, Python,
/// Quarto/Markdown, and SQL behind one stable interface.
///
/// togi routes every file to the right underlying tool and manages its own
/// copies of those tools, downloading them into a private cache on first
/// use. No configuration is required; a project togi.toml overrides only
/// what it sets.
#[derive(Debug, Parser)]
#[command(name = "togi", version, arg_required_else_help = true)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every command.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Show more detail (underlying commands, tool names)
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Suppress all output except errors
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Use this config file instead of discovering togi.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Format project files in place (R, Python, Quarto, SQL, Markdown)
    ///
    /// Discovers files from the given paths (or the whole project when none
    /// are given), routes each to its formatter behind one interface, and
    /// rewrites them in place. Pass --check to report what would change
    /// without touching anything (exit 1 when formatting is needed).
    /// Respects .gitignore, the [format] config, and any per-tool config the
    /// project already has (air.toml, ruff.toml, .sqlfluff).
    #[command(visible_alias = "fmt")]
    Format(format::FormatArgs),
    /// Report lint violations across the project
    ///
    /// Runs each language's linter and prints normalized diagnostics
    /// (file:line:col, rule, message), exiting 1 when any remain. --fix
    /// applies safe autofixes first, then reports what is left; --format json
    /// emits a stable, machine-readable schema.
    Lint(lint::LintArgs),
    /// Manage togi-installed formatter/linter tools (advanced)
    ///
    /// Inspects and maintains the private tool cache togi downloads on first
    /// use. `list` shows installed and default versions; `update` refreshes
    /// to release defaults or config pins; `clean` drops the cache. Most
    /// users never need this.
    Tools(tools::ToolsArgs),
    /// Generate shell completions
    ///
    /// Prints a completion script for the given shell to stdout; redirect it
    /// into the location your shell loads completions from.
    Completions(completions::CompletionsArgs),
    /// Print the togi version and the tool versions baked into this release
    ///
    /// The first line matches `togi --version`; the rest name the default
    /// version of each managed tool this release installs.
    Version,
    /// Upgrade togi to the latest release
    ///
    /// Downloads the latest release for your platform and replaces the
    /// running binary in place. Does nothing if you already have the latest
    /// version.
    Upgrade,
}

/// Dispatch a parsed CLI invocation to its command module.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    apply_global_args(&cli.global);
    let global = cli.global;
    match cli.command {
        Command::Format(args) => format::run(args, &global),
        Command::Lint(args) => lint::run(args, &global),
        Command::Tools(args) => tools::run(args, &global),
        Command::Completions(args) => completions::run(args),
        Command::Version => version::run(),
        Command::Upgrade => upgrade::run(&global),
    }
}

/// Push the global flags into `term`'s process-wide state before dispatch:
/// `--quiet` gates informational stdout output, `--no-color` forces color
/// off, and `--verbose` enables the `running …` invocation log. `--config`
/// is consumed by the commands that need it.
fn apply_global_args(global: &GlobalArgs) {
    term::set_quiet(global.quiet);
    term::set_verbose(global.verbose);
    term::set_color_choice(color_choice_for(global.no_color));
}

/// Pure flag → color-choice mapping, factored out so it is unit-testable.
fn color_choice_for(no_color: bool) -> term::ColorChoice {
    if no_color {
        term::ColorChoice::Never
    } else {
        term::ColorChoice::Auto
    }
}

/// Typed error for usage mistakes clap cannot catch (e.g. a path argument
/// that does not exist). `main` renders it with its hint and exits 2,
/// matching clap's own usage-error exit code.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct UsageError {
    message: String,
    hint: String,
}

impl UsageError {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        self.hint.clone()
    }
}

/// Convenience constructor for command-level usage errors.
pub(crate) fn usage_error(message: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError {
        message: message.into(),
        hint: hint.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use togi_core::term::ColorChoice;

    #[test]
    fn no_color_flag_maps_to_never() {
        assert_eq!(color_choice_for(true), ColorChoice::Never);
    }

    #[test]
    fn without_no_color_the_choice_stays_auto() {
        assert_eq!(color_choice_for(false), ColorChoice::Auto);
    }
}
