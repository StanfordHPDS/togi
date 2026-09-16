# togi

togi (研ぎ) is a formatter and
linter for data science projects. It covers R, Python, Quarto, Markdown, and
SQL behind a single stable interface, and it manages its own copies of the
underlying tools, [air], [ruff], [panache], and [sqlfluff]. You never install, version, or configure those
tools yourself, and when one of them changes, the togi interface does not.

- **One command for the whole project.** `togi format` and `togi lint` route
  every file to the right tool and merge the results into one report with one
  exit code.
- **Self-managing tools.** On first use, togi downloads the exact tool
  versions required into a private cache. Every run after that
  is offline. Projects can pin different versions when they need to.
- **Zero config by default.** Sensible defaults for every language; a
  `togi.toml` overrides only what it sets, and per-tool config a project
  already has (`air.toml`, `ruff.toml`, `.sqlfluff`) is respected.
- **Quiet on success, rich on failure.** Diagnostics normalize to
  `path:line:col: CODE message` no matter which tool found them, and
  `togi lint --format json` emits a stable schema for CI and editors.

[air]: https://github.com/posit-dev/air
[ruff]: https://github.com/astral-sh/ruff
[panache]: https://github.com/jolars/panache
[sqlfluff]: https://github.com/sqlfluff/sqlfluff

## Install

Every tagged release has prebuilt binaries for macOS, Linux, and Windows,
an installer script, and a Homebrew formula.

With the installer script:

```sh
curl -LsSf https://github.com/StanfordHPDS/togi/releases/latest/download/togi-installer.sh | sh
```

With Homebrew:

```sh
brew install StanfordHPDS/tap/togi
```

From source, with a Rust toolchain:

```sh
cargo install --git https://github.com/StanfordHPDS/togi togi
```

## Quickstart

Format the whole project in place:

```console
$ togi format
✓ 14 files formatted, 3 changed
```

Check formatting without rewriting anything---exit code 1 when something
would change, so it drops straight into CI:

```console
$ togi format --check
would reformat: analysis/model.py
would reformat: R/clean.R
error: 2 of 14 files would be reformatted
hint: run `togi format` to apply the changes
```

Lint everything, with normalized `path:line:col` diagnostics and `[*]`
marking findings a `--fix` run can clean up itself:

```console
$ togi lint
query.sql:4:5: LT02 [*] Expected indent of 2 spaces
violations.py:3:8: F401 [*] `os` imported but unused
error: found 2 issues (2 fixable with `togi lint --fix`)
hint: run `togi lint --fix` to apply the safe fixes, then fix the rest by hand

$ togi lint --fix
✓ no issues found in 14 files
```

Machine-readable output for CI and editors (`togi lint --format json`) prints
pure JSON on stdout with project-root-relative paths.

Inspect the managed tools---versions are baked per togi release, cached
privately, and never touch your system installs:

```console
$ togi tools list
air        0.10.0     github release   installed 2026-07-05
ruff       0.14.0     github release   installed 2026-07-05
panache    2.60.0     github release   installed 2026-07-05
sqlfluff   3.4.0      uv (PyPI)        installed 2026-07-05
uv         0.9.5      github release   installed 2026-07-05

$ togi version
togi 0.1.1
  air 0.10.0
  ruff 0.14.0
  panache 2.60.0
  sqlfluff 3.4.0
  uv 0.9.5
```

Stay current, and set up your shell:

```sh
togi upgrade                       # replace this binary with the latest release
togi completions zsh               # print a completion script for your shell
```

Exit codes: `0` success, `1` violations found / changes needed / a tool
failed, `2` usage error.

## Configuration

togi needs no configuration. Out of the box it honors your `.gitignore`, skips
hidden paths and the package-manager directories `renv/` and `rv/`, wraps
Quarto/Markdown prose one sentence per line, and turns off panache's
`missing-chunk-labels` lint (an existing panache config, project or user,
replaces these last two; see `docs/togi.toml.md`). For SQL, it lints large
files and leaves identifier case alone unless the project or user has
sqlfluff config. To change what it covers, create a `togi.toml` at the
project root; it overrides only the keys it sets:

```toml
[format]
exclude = ["vendor/**"]   # added on top of the built-in excludes

[sql]
dialect = "duckdb"
```

Every key, its default, and how project and user config layer are documented
in [docs/togi.toml.md](docs/togi.toml.md).

## Development

togi is a Rust workspace: `crates/togi-core` (library) and `crates/togi`
(binary). Every change must pass:

```sh
cargo test                           # offline tests; must always pass
cargo test --features online-tests   # network/tool-download tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT. See [LICENSE](LICENSE).
