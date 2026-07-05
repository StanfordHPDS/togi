# Recorded ruff output

Real output of **ruff 0.14.0** (the version pinned in `src/tools/versions.rs`),
recorded by running ruff against the files in `input/`. The python adapter's
parsing is tested against these files so the tests stay offline and
deterministic.

One sanitization: `ruff check --output-format json` reports absolute file
paths, so the recording directory prefix was rewritten to `/project`.
Everything else is byte-for-byte what ruff produced.

| File | Command | Exit |
|---|---|---|
| `format-check-mixed.txt` | `ruff format --check misformatted.py clean.py notebook.ipynb` (stdout) | 1 |
| `format-check-clean.txt` | `ruff format --check clean.py` (stdout) | 0 |
| `format-write-mixed.txt` | `ruff format misformatted.py clean.py notebook.ipynb` (stdout) | 0 |
| `format-check-error.stderr.txt` | `ruff format --check syntax_error.py` (stderr) | 2 |
| `check-violations.json` | `ruff check --output-format json violations.py clean.py notebook.ipynb` (stdout) | 1 |
| `check-clean.json` | `ruff check --output-format json clean.py` (stdout) | 0 |
| `check-fix-remaining.json` | `ruff check --fix --output-format json fixme.py` (stdout; `fixme.py` was a copy of `violations.py` — the safe F401 fix was applied to the file, the unsafe/unfixable findings remained) | 1 |
| `check-syntax-error.json` | `ruff check --output-format json syntax_error.py` (stdout) | 1 |

To re-record (e.g. after a ruff version bump), run the commands above with the
pinned ruff (`uvx ruff@<version>`) from a scratch copy of `input/` and rewrite
the absolute path prefix to `/project`.
