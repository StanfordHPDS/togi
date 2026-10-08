//! `togi pin` and `togi unpin` project version selection.

use clap::Args;

#[derive(Debug, Args)]
pub struct PinArgs {
    /// Exact released version to select (default: the effective version)
    pub version: Option<String>,
}

pub fn run(args: PinArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    crate::project_version::pin(
        args.version.as_deref(),
        global.with_version.as_deref(),
        global.verbose,
    )
}

pub fn unpin() -> anyhow::Result<()> {
    crate::project_version::unpin()
}
