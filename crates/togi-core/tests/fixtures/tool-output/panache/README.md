# Recorded panache output

Real output recorded from **panache 2.60.0** (macOS aarch64), with external
formatters **air 0.10.0** (R chunks) and **ruff 0.14.0** (Python chunks)
enabled via a config equivalent to the default one togi generates:

```toml
[formatters]
r = "air"
python = "ruff"

[linters]
python = "ruff"
```

| File | Command | Notes |
| --- | --- | --- |
| `format-check-diff.txt` | `panache format --check --no-cache --no-color report.qmd notes.Rmd long.md README.md` | stdout, exit 1. `long.md` has two separate diff regions, so its `Diff in` header repeats; `README.md` was clean and produces no output. |
| `format-write.txt` | `panache format --no-cache --no-color report.qmd notes.Rmd README.md` | stdout, exit 0. One `Formatted <file>` line per changed file plus a summary line. |
| `lint-short.txt` | `panache lint --no-cache --no-color --message-format short lint.qmd notes.Rmd` | stdout, exit 0 (no `--check`). Per-file diagnostic groups, each followed by its own `Found N issue(s)` summary. The `F401` line comes from ruff linting the embedded Python chunk. |
| `format-error.txt` | `panache format --check --no-color nope.qmd` | stderr, exit 1. Shows the `Warning:`/`Error:` line shapes for a bad invocation. |
| `format-missing-path.txt` | `panache format --no-cache --no-color README.md nope.qmd` | stderr, exit 0. A missing input is only skipped with a warning and the run continues (stdout was `1 file left unchanged`); the run is fatal only when *every* input is missing (`format-error.txt`). |

## panache 3.14.0

Recorded from **panache 3.14.0** (macOS aarch64) with **ruff 0.16.10** on
`PATH`, using togi's generated default config (the table above plus
`[format] wrap = "sentence"` and `[lint.rules] missing-chunk-labels = false`),
from a directory initialized as a git repository.

| File | Command | Notes |
| --- | --- | --- |
| `lint-findings-exit1.stdout.txt` | `panache lint --message-format short --no-color --no-cache --config <default> lint-findings.qmd README.md` | stdout, exit 1. panache 3.14.0 exits 1 whenever lint finds anything, with or without `--check`; the findings are diagnostics, not a failed run. It prints absolute paths even for relative inputs, so the recording directory has been replaced with `/project/`. The `F821` and `F401` lines come from ruff 0.16.10 linting the embedded Python chunk; the severity of ruff findings depends on the ruff version. |
| `lint-findings-exit1.stderr.txt` | (same run) | stderr, exit 1. Only a blank line and the `Found N issue(s) across M file(s)` summary; no `Error:` line, unlike a real failure (`format-error.txt`). |

The exact input documents are checked in under `inputs/`. To re-record,
download the pinned panache/air/ruff releases, copy `inputs/` somewhere
writable, and run the commands above from that directory (with a config
mapping the `air`/`ruff` presets to the downloaded binaries, or with those
binaries on `PATH`).
