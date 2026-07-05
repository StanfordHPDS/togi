//! What a formatter run reports back: how many files it touched and which
//! ones changed.

use std::path::PathBuf;

/// The result of one formatter invocation over a batch of files.
///
/// `changed` doubles as the per-file list behind `togi format --check`
/// summaries ("would reformat: ..."); in check mode it holds the files that
/// *would* change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatOutcome {
    /// How many files the tool processed.
    pub processed: usize,
    /// Files the tool changed (or would change under `--check`), in the
    /// order the tool reported them.
    pub changed: Vec<PathBuf>,
}

impl FormatOutcome {
    /// Files the tool changed (or would change).
    pub fn changed_count(&self) -> usize {
        self.changed.len()
    }

    /// Files the tool left alone.
    // Part of the outcome API alongside `changed_count`; today only tests
    // consume it (the commands report the changed count), hence the allow.
    #[allow(dead_code)]
    pub fn unchanged(&self) -> usize {
        self.processed.saturating_sub(self.changed.len())
    }

    /// Fold another adapter's outcome into this one, for the run-wide
    /// summary line across all adapters.
    pub fn merge(&mut self, other: FormatOutcome) {
        self.processed += other.processed;
        self.changed.extend(other.changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_derive_from_processed_and_change_list() {
        let outcome = FormatOutcome {
            processed: 5,
            changed: vec![PathBuf::from("a.py"), PathBuf::from("b.py")],
        };
        assert_eq!(outcome.changed_count(), 2);
        assert_eq!(outcome.unchanged(), 3);
    }

    #[test]
    fn merge_aggregates_counts_and_concatenates_change_lists() {
        // The run-wide summary is the fold of every adapter's outcome.
        let mut total = FormatOutcome::default();
        total.merge(FormatOutcome {
            processed: 3,
            changed: vec![PathBuf::from("a.R")],
        });
        total.merge(FormatOutcome {
            processed: 2,
            changed: vec![PathBuf::from("b.py"), PathBuf::from("c.py")],
        });

        assert_eq!(total.processed, 5);
        assert_eq!(
            total.changed,
            vec![
                PathBuf::from("a.R"),
                PathBuf::from("b.py"),
                PathBuf::from("c.py"),
            ]
        );
        assert_eq!(total.unchanged(), 2);
    }

    #[test]
    fn unchanged_never_underflows_on_inconsistent_tool_reports() {
        // A tool reporting more changed files than processed must not panic
        // the summary math.
        let outcome = FormatOutcome {
            processed: 1,
            changed: vec![PathBuf::from("a.py"), PathBuf::from("b.py")],
        };
        assert_eq!(outcome.unchanged(), 0);
    }
}
