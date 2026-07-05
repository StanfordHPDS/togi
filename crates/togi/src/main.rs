//! `togi` — polyglot formatter/linter for data science projects.
//!
//! Thin entry point: parse the CLI, dispatch, render top-level errors.

mod cli;

use std::process::ExitCode;

use clap::Parser;

use togi_core::config;
use togi_core::term::{self, HintExt};

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => render_error(err),
    }
}

/// Render a top-level error through `term` and pick the exit code:
/// 1 = failure, 2 = usage error.
fn render_error(err: anyhow::Error) -> ExitCode {
    let usage_hint = err
        .downcast_ref::<cli::UsageError>()
        .map(|u| u.hint())
        .or_else(|| {
            // A bad `--config` value is a usage error like any other bad
            // flag value.
            err.downcast_ref::<config::MissingConfigFile>()
                .map(|e| e.hint())
        });
    match usage_hint {
        Some(hint) => {
            // Usage errors carry their hint on the type; attach it so
            // `term::error` renders the standard `hint:` line.
            let hinted = Err::<(), _>(err).hint(hint).expect_err("just wrapped");
            term::error(&hinted);
            ExitCode::from(2)
        }
        None => {
            term::error(&err);
            ExitCode::FAILURE
        }
    }
}
