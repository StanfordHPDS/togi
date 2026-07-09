//! File discovery for format/lint targets: gitignore-aware walking
//! with additive exclude globs, plus the extension → language registry used
//! to batch files per adapter.
//!
//! This module returns data only. It never prints to the terminal — all
//! output goes through `term`.

mod registry;
mod walk;

pub use registry::{ExtensionRegistry, Language, group_by_language};
pub use walk::{DEFAULT_EXCLUDES, FsxError, walk};
