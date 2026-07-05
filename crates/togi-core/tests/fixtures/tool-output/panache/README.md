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

The exact input documents are checked in under `inputs/`. To re-record,
download the pinned panache/air/ruff releases, copy `inputs/` somewhere
writable, and run the commands above from that directory (with a config
mapping the `air`/`ruff` presets to the downloaded binaries, or with those
binaries on `PATH`).
