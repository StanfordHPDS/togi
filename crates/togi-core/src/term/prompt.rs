//! Prompt wrappers around `inquire`.
//!
//! Every interactive flow in togi must have a flag-driven non-interactive
//! path. These wrappers enforce the output half of that contract:
//! when the process is non-interactive — the global flag is set (wired to
//! `--yes`/CI detection later) or stdin is not a TTY — prompting fails with
//! an actionable error instead of hanging or panicking.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use super::HintExt;

static NON_INTERACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark the whole process as non-interactive (wired to `--yes` flags).
/// All prompt wrappers will then refuse to prompt.
pub fn set_non_interactive(non_interactive: bool) {
    NON_INTERACTIVE.store(non_interactive, Ordering::Relaxed);
}

/// Pure decision, factored out of the global flag and TTY probing so it is
/// unit-testable.
fn is_non_interactive(flag: bool, stdin_is_tty: bool) -> bool {
    flag || !stdin_is_tty
}

fn ensure_interactive(prompt: &str) -> anyhow::Result<()> {
    if is_non_interactive(
        NON_INTERACTIVE.load(Ordering::Relaxed),
        std::io::stdin().is_terminal(),
    ) {
        Err(interactivity_error(prompt))
    } else {
        Ok(())
    }
}

/// The error every prompt wrapper returns when it cannot prompt.
fn interactivity_error(prompt: &str) -> anyhow::Error {
    Err::<(), _>(anyhow::anyhow!(
        "cannot ask \"{prompt}\": togi is running non-interactively"
    ))
    .hint("re-run from an interactive terminal, or pass the flag that answers this prompt (see --help)")
    .unwrap_err()
}

/// Ask for a line of text; `default` is used when the user just hits enter.
pub fn text(prompt: &str, default: &str) -> anyhow::Result<String> {
    ensure_interactive(prompt)?;
    inquire::Text::new(prompt)
        .with_default(default)
        .prompt()
        .with_context(|| format!("could not read an answer to \"{prompt}\""))
}

/// Ask a yes/no question. `default` is used as the highlighted answer.
pub fn confirm(prompt: &str, default: bool) -> anyhow::Result<bool> {
    ensure_interactive(prompt)?;
    inquire::Confirm::new(prompt)
        .with_default(default)
        .prompt()
        .with_context(|| format!("could not read an answer to \"{prompt}\""))
}

/// Ask the user to pick exactly one of `options`.
pub fn select<T: Display>(prompt: &str, options: Vec<T>) -> anyhow::Result<T> {
    ensure_interactive(prompt)?;
    inquire::Select::new(prompt, options)
        .prompt()
        .with_context(|| format!("could not read an answer to \"{prompt}\""))
}

/// Ask the user to pick any number of `options`.
pub fn multiselect<T: Display>(prompt: &str, options: Vec<T>) -> anyhow::Result<Vec<T>> {
    ensure_interactive(prompt)?;
    inquire::MultiSelect::new(prompt, options)
        .prompt()
        .with_context(|| format!("could not read an answer to \"{prompt}\""))
}

/// Ask the user to pick any number of `options`, with every option
/// pre-selected — an opt-out checklist (deselect what you don't want).
pub fn multiselect_all<T: Display>(prompt: &str, options: Vec<T>) -> anyhow::Result<Vec<T>> {
    ensure_interactive(prompt)?;
    inquire::MultiSelect::new(prompt, options)
        .with_all_selected_by_default()
        .prompt()
        .with_context(|| format!("could not read an answer to \"{prompt}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::render_error;

    #[test]
    fn interactive_when_stdin_is_a_tty_and_flag_unset() {
        assert!(!is_non_interactive(false, true));
    }

    #[test]
    fn non_interactive_when_flag_is_set_even_on_a_tty() {
        assert!(is_non_interactive(true, true));
    }

    #[test]
    fn non_interactive_when_stdin_is_not_a_tty() {
        assert!(is_non_interactive(false, false));
    }

    #[test]
    fn interactivity_error_names_the_prompt_and_suggests_a_fix() {
        let err = interactivity_error("Initialize a git repository?");
        let out = render_error(&err, false);
        assert!(
            out.contains("Initialize a git repository?"),
            "out was: {out}"
        );
        assert!(out.contains("hint:"), "out was: {out}");
    }
}
