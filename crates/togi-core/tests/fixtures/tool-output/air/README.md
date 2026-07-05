# Recorded `air` output

Real stderr captured from **air 0.10.0** (the pinned default in
`src/tools/versions.rs`) on macOS arm64. air writes all of its messages to
stderr; stdout stays empty. Every invocation used `--no-color`, exactly as
the adapter runs the tool.

| Fixture | Command | Exit code |
|---|---|---|
| `format-check-would-reformat.txt` | `air format --check --no-color needs_formatting.R analysis/model.r clean.R` | 1 |
| `format-check-clean.txt` | `air format --check --no-color clean.R` | 0 |
| `format-check-syntax-error.txt` | `air format --check --no-color broken.R needs_formatting.R clean.R` | 255 |
| `format-in-place-syntax-error.txt` | `air format --no-color broken.R inplace.R` | 255 |

Input files: `needs_formatting.R` / `analysis/model.r` / `inplace.R` held
badly formatted but valid R, `clean.R` was already formatted, `broken.R`
contained a syntax error. To re-record with a newer air, recreate files like
those, run the commands above, and redirect stderr into these paths.
