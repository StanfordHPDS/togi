//! Terminal output for `togi`: styling, error rendering, progress, prompts.
//!
//! This is the ONLY module allowed to print to the terminal. All
//! helpers degrade gracefully: color is dropped when `NO_COLOR` is set, when
//! `TERM=dumb`, or when the stream is not a TTY, and progress bars hide
//! themselves on non-TTY stderr.

mod error;
mod progress;
mod prompt;

#[allow(unused_imports)] // consumed only by unit tests today; part of the ui API
pub use error::render_error;
pub use error::{HintExt, error};
#[allow(unused_imports)] // re-exported for later commands; ui lands before its callers
pub use progress::progress_bar;
pub use prompt::{confirm, multiselect, multiselect_all, select, set_non_interactive, text};

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Set the process-wide quiet mode (wired to the global `--quiet` flag).
/// When quiet, informational stdout output ([`println`], [`success`]) is
/// suppressed; errors (and warnings) still print to stderr.
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// Set the process-wide verbose mode (wired to the global `--verbose` flag).
/// When verbose, [`verbose`] lines print to stderr; otherwise they are
/// suppressed.
pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// How color should be decided for output streams. `Auto` (the default)
/// inspects `NO_COLOR`, `TERM`, and whether the stream is a TTY; the global
/// `--no-color` flag maps to `Never`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

static COLOR_CHOICE: AtomicU8 = AtomicU8::new(0);

/// Set the process-wide color choice (wired to the global `--no-color` flag).
pub fn set_color_choice(choice: ColorChoice) {
    let raw = match choice {
        ColorChoice::Auto => 0,
        ColorChoice::Always => 1,
        ColorChoice::Never => 2,
    };
    COLOR_CHOICE.store(raw, Ordering::Relaxed);
}

fn color_choice() -> ColorChoice {
    match COLOR_CHOICE.load(Ordering::Relaxed) {
        1 => ColorChoice::Always,
        2 => ColorChoice::Never,
        _ => ColorChoice::Auto,
    }
}

/// Pure color decision, factored out of env/TTY probing so it is unit-testable.
fn resolve_color(
    choice: ColorChoice,
    no_color: Option<&str>,
    term: Option<&str>,
    is_tty: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            // Per https://no-color.org: disable when present and non-empty.
            if no_color.is_some_and(|v| !v.is_empty()) {
                return false;
            }
            if term == Some("dumb") {
                return false;
            }
            is_tty
        }
    }
}

fn env_resolve_color(is_tty: bool) -> bool {
    resolve_color(
        color_choice(),
        std::env::var("NO_COLOR").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        is_tty,
    )
}

/// Whether styled output should be emitted on stdout right now.
pub fn stdout_colors() -> bool {
    env_resolve_color(std::io::stdout().is_terminal())
}

/// Whether styled output should be emitted on stderr right now.
pub(crate) fn stderr_colors() -> bool {
    env_resolve_color(std::io::stderr().is_terminal())
}

pub const ERROR_STYLE: anstyle::Style = anstyle::Style::new()
    .bold()
    .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)));
pub const WARN_STYLE: anstyle::Style = anstyle::Style::new()
    .bold()
    .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)));
pub(crate) const SUCCESS_STYLE: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)));
pub(crate) const HINT_STYLE: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
pub const INFO_STYLE: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Blue)));
/// The `path:line:col` prefix of a diagnostic line, styled like ruff.
pub const DIAGNOSTIC_LOCATION_STYLE: anstyle::Style = anstyle::Style::new()
    .bold()
    .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
/// The dimmed prefix of a `-v` "running …" invocation log.
pub(crate) const VERBOSE_STYLE: anstyle::Style = anstyle::Style::new().dimmed();

/// Wrap `text` in `style` when `use_color` is true; plain text otherwise.
pub fn paint(style: anstyle::Style, text: &str, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

fn render_success(msg: &str, use_color: bool) -> String {
    format!("{} {msg}", paint(SUCCESS_STYLE, "✓", use_color))
}

fn render_warn(msg: &str, use_color: bool) -> String {
    format!("{} {msg}", paint(WARN_STYLE, "warning:", use_color))
}

fn render_verbose(msg: &str, use_color: bool) -> String {
    format!("{} {msg}", paint(VERBOSE_STYLE, "running", use_color))
}

/// Print an unstyled informational line to stdout. Suppressed by `--quiet`.
pub fn println(msg: &str) {
    if !is_quiet() {
        std::println!("{msg}");
    }
}

/// Print machine-readable output (`--format json`) to stdout. Never
/// suppressed by `--quiet`: quiet silences human chrome, not the data
/// another program asked for.
pub fn data(msg: &str) {
    std::println!("{msg}");
}

/// Print a green `✓`-prefixed success line to stdout. Suppressed by
/// `--quiet`.
pub fn success(msg: &str) {
    if !is_quiet() {
        std::println!("{}", render_success(msg, stdout_colors()));
    }
}

/// Print a `warning:`-prefixed line to stderr. Not suppressed by `--quiet`:
/// warnings flag conditions the user likely needs to act on, so they stay
/// visible alongside errors.
pub fn warn(msg: &str) {
    eprintln!("{}", render_warn(msg, stderr_colors()));
}

/// Print a dimmed `running <command>` line to stderr, but only under
/// `--verbose`. Always goes to stderr (never stdout), so machine output
/// like `--format json` stays clean regardless of verbosity.
pub fn verbose(msg: &str) {
    if is_verbose() {
        eprintln!("{}", render_verbose(msg, stderr_colors()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: &str = "\x1b[";

    #[test]
    fn auto_disables_color_when_not_a_tty() {
        assert!(!resolve_color(
            ColorChoice::Auto,
            None,
            Some("xterm-256color"),
            false
        ));
    }

    #[test]
    fn auto_enables_color_on_a_tty_with_clean_env() {
        assert!(resolve_color(
            ColorChoice::Auto,
            None,
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn no_color_env_disables_color_even_on_a_tty() {
        assert!(!resolve_color(
            ColorChoice::Auto,
            Some("1"),
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn empty_no_color_env_is_ignored_per_no_color_spec() {
        assert!(resolve_color(
            ColorChoice::Auto,
            Some(""),
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn dumb_term_disables_color() {
        assert!(!resolve_color(ColorChoice::Auto, None, Some("dumb"), true));
    }

    #[test]
    fn never_overrides_everything() {
        assert!(!resolve_color(
            ColorChoice::Never,
            None,
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn always_overrides_everything() {
        assert!(resolve_color(
            ColorChoice::Always,
            Some("1"),
            Some("dumb"),
            false
        ));
    }

    #[test]
    fn set_color_choice_round_trips() {
        set_color_choice(ColorChoice::Always);
        assert_eq!(color_choice(), ColorChoice::Always);
        set_color_choice(ColorChoice::Never);
        assert_eq!(color_choice(), ColorChoice::Never);
        set_color_choice(ColorChoice::Auto);
        assert_eq!(color_choice(), ColorChoice::Auto);
    }

    #[test]
    fn paint_without_color_emits_no_ansi_codes() {
        let out = paint(ERROR_STYLE, "boom", false);
        assert_eq!(out, "boom");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn paint_with_color_wraps_text_in_ansi_codes() {
        let out = paint(ERROR_STYLE, "boom", true);
        assert!(out.contains(ESC));
        assert!(out.contains("boom"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn success_line_without_color_has_check_prefix_and_no_ansi() {
        let out = render_success("42 files formatted", false);
        assert_eq!(out, "✓ 42 files formatted");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn success_line_with_color_contains_ansi() {
        assert!(render_success("done", true).contains(ESC));
    }

    #[test]
    fn warn_line_without_color_has_warning_prefix_and_no_ansi() {
        let out = render_warn("unknown key `foo` in togi.toml", false);
        assert_eq!(out, "warning: unknown key `foo` in togi.toml");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn warn_line_with_color_contains_ansi() {
        assert!(render_warn("careful", true).contains(ESC));
    }

    #[test]
    fn verbose_line_without_color_has_running_prefix_and_no_ansi() {
        let out = render_verbose("/tools/ruff check -- a.py", false);
        assert_eq!(out, "running /tools/ruff check -- a.py");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn verbose_line_with_color_contains_ansi() {
        assert!(render_verbose("/tools/ruff check", true).contains(ESC));
    }

    #[test]
    fn set_verbose_round_trips() {
        set_verbose(true);
        assert!(is_verbose());
        set_verbose(false);
        assert!(!is_verbose());
    }
}
