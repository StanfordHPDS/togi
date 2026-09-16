# `togi.toml` configuration reference

togi works with **zero configuration**. Every key below is optional and has a
built-in default; a config file only *overrides* those defaults.

## Where config lives and how it layers

togi reads configuration from two files:

- **Project config** — `togi.toml`, discovered by walking up from the current
  directory. The walk stops at the git root (a directory containing `.git`)
  or the filesystem root, so a `togi.toml` in some unrelated parent directory
  never leaks into your repo. `--config <path>` bypasses discovery and reads
  exactly that file.
- **User config** — `config.toml` in your platform config directory:
  - Linux: `~/.config/togi/config.toml`
  - macOS: `~/Library/Application Support/togi/config.toml`
  - Windows: `%APPDATA%\togi\config\config.toml`

Values are resolved by layering, lowest priority first:

```
built-in defaults  ←  user config  ←  project config  ←  CLI flags
```

Each layer overrides only the keys it sets; everything else falls through
from the layer beneath. Layering is **key-by-key**, not table-by-table:
setting `exclude` under `[format]` in project config does not discard
`languages` from a lower layer. For `[tools]`, pins and `args` merge per tool
name — a higher layer's pin for `air` replaces a lower one but leaves other
tools' pins intact, and a tool's `args` list replaces wholesale (lists never
concatenate across layers).

**Unknown keys warn, they do not error.** A key togi does not recognize is
ignored with a warning (forward compatibility), so a newer `togi.toml` still
loads on an older binary. A *wrong type* for a known key (for example
`dialect = 3`) is a real error.

## Complete annotated example

Every key, set to a representative value. This whole file parses; the
reference tests load it against the real binary.

```toml
[format]
# Languages `togi format` covers; the default is everything togi knows.
languages = ["r", "python", "quarto", "sql", "markdown"]
exclude = ["renv/**", "vendor/**"]  # gitignore-style globs, additive to .gitignore

[lint]
# Plain Markdown is formatted but not linted by default.
languages = ["r", "python", "quarto", "sql"]
exclude = []

[sql]
dialect = "bigquery"   # passed to sqlfluff when no .sqlfluff applies

# Version pins for managed tools; omit to use the versions baked into this
# togi release. A bare `name = "x.y.z"` is a pin.
[tools]
ruff = "0.14.0"

# A tool needing passthrough args (and optionally a pin) uses a
# `[tools.<name>]` table instead — TOML forbids `air = "..."` and a
# `[tools.air]` table in the same file.
[tools.air]
version = "0.10.0"
args = ["--verbose"]
```

## `[format]` and `[lint]`

Which languages `togi format` / `togi lint` operate on, and which paths to
skip. The two tables are independent, which is how plain Markdown gets
formatted but not linted by default.

| Key | Type | Default (`[format]`) | Default (`[lint]`) | Description |
|---|---|---|---|---|
| `languages` | array of strings | `["r", "python", "quarto", "sql", "markdown"]` | `["r", "python", "quarto", "sql"]` | Language buckets to include. Recognized names: `r`, `python`, `quarto`, `markdown`, `sql` (case-insensitive). Unrecognized names warn and are skipped, so a typo never silently disables a run. |
| `exclude` | array of strings | `[]` | `[]` | gitignore-style glob patterns, **additive** to the repo's `.gitignore` and to togi's built-in excludes, anchored at the project root. Matching files are never formatted or linted. |

Beyond `.gitignore` and your `exclude`, togi always skips the package-manager
directories `renv/` and `rv/` — they hold installed R libraries and generated
files (like `renv/activate.R`) that no formatter should touch. Your `exclude`
adds to these built-ins; it does not replace them. uv's `.venv/` needs no entry
because togi skips hidden files and directories.

For Quarto and Markdown, togi wraps prose one sentence per line by default
(panache's `wrap = "sentence"`), rather than reflowing paragraphs to a fixed
width. togi's default also turns off panache's `missing-chunk-labels` lint,
which otherwise flags executable code chunks without a `#| label:`. Both
apply only when neither the project nor the user has a panache config of
their own. A project `.panache.toml`, `panache.toml`, or
`.config/panache.toml` found while walking up from the input files, or a
user-level config at `~/.config/panache/config.toml` (or
`$XDG_CONFIG_HOME/panache/config.toml`), takes full control.

## `[sql]`

| Key | Type | Default | Description |
|---|---|---|---|
| `dialect` | string | `"bigquery"` | SQL dialect passed to sqlfluff. Only applied when neither the project nor the user has configured sqlfluff (a `.sqlfluff` file wins). |

When neither the project nor the user has sqlfluff config, togi also applies two
sqlfluff settings alongside the dialect: `large_file_skip_byte_limit = 0`, so
large SQL files are linted rather than skipped, and
`unquoted_identifiers_policy = none` for the `capitalisation.identifiers`
rule, so identifier case is left as written.

Any sqlfluff config that sqlfluff itself would read takes full control
instead. That means:

- a `.sqlfluff` file
- a sqlfluff section in `setup.cfg`, `tox.ini`, or `pep8.ini`
- a `tool.sqlfluff` table in `pyproject.toml`
- a `pyproject.toml` that togi cannot read or parse, so sqlfluff reports the
  problem itself

found in any directory sqlfluff searches:

- the working directory
- the directories between the home directory and each input file
- the directories from the working directory (or its nearest ancestor shared
  with the file) down to each input file
- the home directory itself
- sqlfluff's user config directory: `~/.config/sqlfluff` whenever that
  directory exists, on every platform, Windows included; otherwise
  `$XDG_CONFIG_HOME/sqlfluff` on Linux and macOS when `XDG_CONFIG_HOME` is
  set, and without it `~/.config/sqlfluff` on Linux and
  `~/Library/Application Support/sqlfluff` on macOS; on Windows,
  `%LOCALAPPDATA%\sqlfluff\sqlfluff`

togi passes these two settings to sqlfluff as a generated `--config` file.
sqlfluff honors only the last `--config` it receives and does not merge
config files, so a `--config` in `[tools.sqlfluff] args` replaces togi's
generated config entirely, and these two settings no longer apply.

## `[tools]` and `[tools.<name>]`

Version pins and passthrough arguments for the managed tools (`air`, `ruff`,
`panache`, `sqlfluff`; `uv` bootstraps sqlfluff). Omit everything here to use
the versions baked into this togi release — `togi version` lists them.

There are two shapes:

- **A bare pin** under `[tools]`: `ruff = "0.14.0"`. The value is the exact
  version togi installs and runs.
- **A `[tools.<name>]` table** when a tool needs passthrough args (and
  optionally a pin). Use this form instead of a bare pin whenever you also
  set `args`, because TOML forbids a `name = "..."` key and a `[tools.name]`
  table for the same name.

| Key | Type | Default | Description |
|---|---|---|---|
| `<name>` (under `[tools]`) | string | release default | Version pin for the tool `<name>`, e.g. `air = "0.10.0"`. `togi tools update` installs pinned versions; unpinned tools stay on the release default. |
| `version` (under `[tools.<name>]`) | string | release default | Version pin, equivalent to the bare-pin form; used when the same table also sets `args`. |
| `args` (under `[tools.<name>]`) | array of strings | `[]` | Extra arguments appended to every invocation of the tool — the escape hatch for options togi does not expose directly. |

```toml
# Pass a rule override to sqlfluff without pinning its version.
[tools.sqlfluff]
args = ["--exclude-rules", "LT05"]
```
