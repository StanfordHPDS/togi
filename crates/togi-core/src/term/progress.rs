//! Progress bar wrapper around `indicatif`.
//!
//! Bars draw to stderr so they never pollute piped stdout. When stderr is
//! not a TTY a bar cannot render, so the label is printed once as a plain
//! notice line instead — long work is never silent.

use std::io::IsTerminal;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Create a progress bar of `len` steps labeled with `msg`.
///
/// On a TTY the bar (label included) is on screen immediately, before any
/// progress is reported. On a non-TTY stderr the bar is hidden and `msg`
/// is printed once as a plain line (suppressed by `--quiet`), so logs and
/// CI runs still say what is happening.
pub fn progress_bar(len: u64, msg: impl Into<String>) -> ProgressBar {
    let msg = msg.into();
    if std::io::stderr().is_terminal() {
        new_progress_bar(len, msg, ProgressDrawTarget::stderr())
    } else {
        if let Some(notice) = non_tty_notice(&msg, super::is_quiet()) {
            eprintln!("{notice}");
        }
        new_progress_bar(len, msg, ProgressDrawTarget::hidden())
    }
}

/// Target-independent core, factored out so unit tests are deterministic.
/// The immediate `tick` puts the label on screen at creation: a bar that
/// only rendered on its first `inc` would show nothing at all for work
/// that reports no increments (e.g. waiting on a subprocess).
fn new_progress_bar(len: u64, msg: String, target: ProgressDrawTarget) -> ProgressBar {
    let style = ProgressStyle::with_template("{msg} [{bar:30}] {pos}/{len}")
        // Static template; a failure here is a programming error, not user input.
        .expect("progress bar template is valid")
        .progress_chars("=> ");
    let bar = ProgressBar::with_draw_target(Some(len), target)
        .with_style(style)
        .with_message(msg);
    bar.tick();
    bar
}

/// The one-line stand-in for a bar on non-TTY stderr; `None` under
/// `--quiet`.
fn non_tty_notice(msg: &str, quiet: bool) -> Option<String> {
    (!quiet).then(|| msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::InMemoryTerm;

    #[test]
    fn non_tty_progress_bar_is_hidden() {
        let pb = new_progress_bar(10, "downloading ruff".into(), ProgressDrawTarget::hidden());
        assert!(pb.is_hidden());
    }

    #[test]
    fn terminal_progress_bar_is_visible_and_draws_progress() {
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let pb = new_progress_bar(10, "downloading ruff".into(), target);
        assert!(!pb.is_hidden());
        pb.inc(3);
        let drawn = term.contents();
        assert!(drawn.contains("downloading ruff"), "drawn was: {drawn}");
        assert!(drawn.contains("3/10"), "drawn was: {drawn}");
    }

    #[test]
    fn terminal_progress_bar_draws_its_label_before_any_progress() {
        // A bar that only renders on the first `inc` shows nothing at all
        // for work that reports no increments (or none yet) — the label
        // must be on screen from the moment the bar exists.
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let _pb = new_progress_bar(1, "Fetching R formatter…".into(), target);
        let drawn = term.contents();
        assert!(
            drawn.contains("Fetching R formatter…"),
            "label must render immediately, drawn was: {drawn:?}"
        );
    }

    #[test]
    fn non_tty_fallback_is_a_single_notice_line() {
        assert_eq!(
            non_tty_notice("Fetching R formatter…", false).as_deref(),
            Some("Fetching R formatter…")
        );
    }

    #[test]
    fn non_tty_fallback_is_silent_under_quiet() {
        assert_eq!(non_tty_notice("Fetching R formatter…", true), None);
    }

    #[test]
    fn progress_bar_carries_len_and_message() {
        let pb = new_progress_bar(
            42,
            "verifying checksums".into(),
            ProgressDrawTarget::hidden(),
        );
        assert_eq!(pb.length(), Some(42));
        assert_eq!(pb.message(), "verifying checksums");
    }
}
