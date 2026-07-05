//! `togi completions` — generate shell completions.

use clap::{Args, CommandFactory};
use clap_complete::Shell;

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: Shell,
}

pub fn run(args: CompletionsArgs) -> anyhow::Result<()> {
    let mut cmd = super::Cli::command();
    // Completion scripts are machine output meant for redirection, written
    // straight to stdout by design (not styled terminal output via `term`).
    clap_complete::generate(args.shell, &mut cmd, "togi", &mut std::io::stdout());
    Ok(())
}
