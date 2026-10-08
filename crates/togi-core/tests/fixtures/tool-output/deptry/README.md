# Recorded deptry output

Real output of **deptry 0.25.1**, recorded with its isolated uv tool Python
3.14 while reading dependency metadata from a Python 3.13 project virtual
environment. The input projects are under `input/`.

| File | Input | Result | Exit | stdout | stderr |
|---|---|---|---:|---|---|
| `violations.json` | `input/violations/` | Four findings: DEP001, two DEP002, and DEP005 | 1 | empty | Human-readable findings and a module-name fallback warning |
| `clean.json` | `input/clean/` | No findings | 0 | empty | Success message |
| `error.stderr.txt` | `input/error/` | No dependency manifest | 1 | empty | Unhandled `DependencySpecificationNotFoundError` traceback |

The `line` and `column` fields for findings attached to `pyproject.toml` are
genuine JSON nulls. In `error.stderr.txt`, the absolute uv tool environment
prefix was replaced with `/tool`; all other text is unchanged.

The JSON recordings used `--no-ansi --enforce-posix-paths`. deptry writes the
JSON file separately and sends all human-readable output, including ordinary
progress and success messages, to stderr.
