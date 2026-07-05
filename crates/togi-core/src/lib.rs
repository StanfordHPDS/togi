//! Library behind `togi`, the polyglot formatter/linter for data science
//! projects (R, Python, Quarto/Markdown, SQL).
//!
//! - [`adapters`]: the `Formatter`/`Linter` traits, normalized diagnostics,
//!   the language registry, and the parallel runner.
//! - [`tools`]: the managed toolchain — download, verify, and cache the
//!   underlying tools so users never install them by hand.
//! - [`fsx`]: gitignore-aware file discovery bucketed by language.
//! - [`config`]: `togi.toml` discovery, parsing, and layering.
//! - [`term`]: styled terminal output, progress, and prompts — the only
//!   module allowed to print.

pub mod adapters;
pub mod config;
pub mod fsx;
pub mod term;
pub mod tools;
