//! Error rendering: styled `error:` prefix, cause chain, and an optional
//! `hint:` line when a fix can be suggested.

use std::fmt;

use super::{ERROR_STYLE, HINT_STYLE, paint, stderr_colors};

/// A remediation hint riding along an error chain, attached via
/// [`HintExt::hint`].
///
/// Displays as the wrapped error so plain `{}`/`{:#}` formatting stays
/// sensible; [`render_error`] pulls it out of the chain and renders the hint
/// text as a trailing `hint:` line so commands can tell users what to do next.
#[derive(Debug)]
struct Hinted {
    hint: String,
    source: anyhow::Error,
}

impl fmt::Display for Hinted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.source, f)
    }
}

impl std::error::Error for Hinted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Attach a remediation hint to an error result.
pub trait HintExt<T> {
    /// Wrap the error with a hint that [`render_error`] shows as a
    /// `hint:` line.
    fn hint(self, hint: impl Into<String>) -> anyhow::Result<T>;
}

impl<T> HintExt<T> for anyhow::Result<T> {
    fn hint(self, hint: impl Into<String>) -> anyhow::Result<T> {
        self.map_err(|source| {
            anyhow::Error::new(Hinted {
                hint: hint.into(),
                source,
            })
        })
    }
}

/// Render an error for the terminal: `error: <message>`, one `caused by:`
/// line per cause, and a final `hint:` line when the chain carries a hint
/// attached via [`HintExt::hint`].
pub fn render_error(err: &anyhow::Error, use_color: bool) -> String {
    // Hints ride along as `Hinted` wrappers (see `HintExt`); pull them out so
    // they render as a trailing `hint:` line rather than as causes. A
    // `Hinted` displays as its source, so skipping it drops no message.
    let mut hint = None;
    let mut messages = Vec::new();
    for cause in err.chain() {
        match cause.downcast_ref::<Hinted>() {
            // Innermost hint wins: it was attached closest to the failure.
            Some(h) => hint = Some(h),
            None => messages.push(cause.to_string()),
        }
    }

    let mut out = String::new();
    let mut messages = messages.into_iter();
    let first = messages
        .next()
        .unwrap_or_else(|| "unknown error".to_string());
    out.push_str(&paint(ERROR_STYLE, "error:", use_color));
    out.push(' ');
    out.push_str(&first);
    for cause in messages {
        out.push('\n');
        out.push_str(&paint(ERROR_STYLE, "caused by:", use_color));
        out.push(' ');
        out.push_str(&cause);
    }
    if let Some(hint) = hint {
        out.push('\n');
        out.push_str(&paint(HINT_STYLE, "hint:", use_color));
        out.push(' ');
        out.push_str(&hint.hint);
    }
    out
}

/// Print `err` to stderr in the standard `togi` error format.
pub fn error(err: &anyhow::Error) {
    eprintln!("{}", render_error(err, stderr_colors()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    const ESC: &str = "\x1b[";

    #[test]
    fn renders_error_prefix_for_a_simple_error() {
        let err = anyhow!("could not read togi.toml");
        assert_eq!(render_error(&err, false), "error: could not read togi.toml");
    }

    #[test]
    fn renders_cause_chain_outermost_first() {
        let err = anyhow!("permission denied")
            .context("could not open `togi.toml`")
            .context("failed to load config");
        let out = render_error(&err, false);
        assert_eq!(
            out,
            "error: failed to load config\n\
             caused by: could not open `togi.toml`\n\
             caused by: permission denied"
        );
    }

    #[test]
    fn renders_hint_as_trailing_hint_line_not_a_cause() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("`ruff` not found in tool cache"))
            .hint("run `togi tools update` to install it")
            .unwrap_err();
        let out = render_error(&err, false);
        assert_eq!(
            out,
            "error: `ruff` not found in tool cache\n\
             hint: run `togi tools update` to install it"
        );
        assert!(!out.contains("caused by"));
    }

    #[test]
    fn renders_hint_after_cause_chain() {
        let err: anyhow::Error =
            Err::<(), _>(anyhow!("connection refused").context("could not download ruff"))
                .hint("check your network connection or proxy settings")
                .unwrap_err();
        let out = render_error(&err, false);
        assert_eq!(
            out,
            "error: could not download ruff\n\
             caused by: connection refused\n\
             hint: check your network connection or proxy settings"
        );
    }

    #[test]
    fn non_tty_error_output_has_no_ansi_codes() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("root cause").context("outer message"))
            .hint("try again")
            .unwrap_err();
        assert!(!render_error(&err, false).contains(ESC));
    }

    #[test]
    fn colored_error_output_styles_the_prefixes() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("boom"))
            .hint("do the thing")
            .unwrap_err();
        let out = render_error(&err, true);
        assert!(out.contains(ESC));
        assert!(out.contains("error:"));
        assert!(out.contains("hint:"));
    }
}
