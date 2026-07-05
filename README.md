# togi

togi (研ぎ — blade polishing) is a polyglot formatter and linter for data
science projects: R, Python, Quarto/Markdown, and SQL behind one stable
interface. It manages its own copies of the underlying tools (air, ruff,
panache, sqlfluff) — you never install or configure them yourself.

```
togi format [PATHS...] [--check]        # alias: fmt
togi lint   [PATHS...] [--fix] [--format json]
togi tools  list|update|clean
togi completions <shell>
togi version
togi upgrade
```

Zero-config by default; a project `togi.toml` overrides only what it sets.

## Development

```
cargo build
cargo test                           # offline tests; must always pass
cargo test --features online-tests   # network/tool-download tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
