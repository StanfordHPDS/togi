# Recorded sqlfluff output

Real output recorded from **sqlfluff 3.4.0** (CPython 3.12, macOS) against
small BigQuery-dialect samples. Re-record with the commands below if the
pinned sqlfluff version changes shape.

Input files used:

- `events.sql` — misformatted: bad indentation, whitespace before comma,
  inconsistent keyword capitalisation, `!=`.
- `clean.sql` — already formatted, no violations.
- `broken.sql` — unparsable (`SELECT FROM WHERE (`).
- `star.sql` — `select * from ...` (unfixable AM04 violation).

Recordings (exit codes observed in parentheses):

| File | Command |
|---|---|
| `lint-violations.json` | `sqlfluff lint --format json --disable-progress-bar --dialect bigquery events.sql clean.sql` (exit 1) |
| `lint-parse-error.json` | `sqlfluff lint --format json --disable-progress-bar --dialect bigquery broken.sql` (exit 1) |
| `lint-format-rules.json` | `sqlfluff lint --format json --disable-progress-bar --dialect bigquery --rules <format rule set> events.sql clean.sql broken.sql` (exit 1) |
| `format-fixed.txt` | `sqlfluff format --disable-progress-bar --dialect bigquery events.sql clean.sql` stdout (exit 0) |
| `fix-residual.txt` / `.stderr.txt` | `sqlfluff fix --disable-progress-bar --dialect bigquery star.sql` (exit 1: an unfixable violation remains) |

`<format rule set>` is the rule subset sqlfluff's own `format` subcommand
force-applies (see `FORMAT_RULES` in `src/adapters/sql.rs`):
`capitalisation,layout,ambiguous.union,convention.not_equal,convention.coalesce,convention.select_trailing_comma,convention.is_null,jinja.padding,structure.distinct`.

Exit-code semantics observed on 3.4.0:

- `lint`: 0 clean, 1 violations found, 2 usage/config error (e.g. no dialect).
- `fix`: 0 all fixed, 1 unfixable or templating/parse errors remain.
- `format`: 0 formatted (even when violations were fixed or unfixable
  residue remains), 1 templating/parse errors, 2 usage/config error.
  `format` has **no** `--check` flag, which is why check mode runs
  `lint --format json --rules <format rule set>` instead.
