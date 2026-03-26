# Contributing to Pensyve

## Setup

```bash
git clone https://github.com/major7apps/pensyve.git && cd pensyve
uv sync --extra dev
uv run maturin develop --release -m pensyve-python/Cargo.toml
make check  # lint + test
```

**Prerequisites:** Rust 1.88+, Python 3.10+ with [uv](https://github.com/astral-sh/uv), [Bun](https://bun.sh) (TS SDK), [Go 1.21+](https://go.dev) (Go SDK)

## Workflow

1. Fork and branch from `main`
2. Make focused changes, add tests
3. `make check` must pass
4. Open a PR

## Commit Prefixes

`feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `ci`, `chore`

## Code Style

- **Rust:** `clippy -D warnings`, no `unsafe` without justification
- **Python:** ruff rules in `pyproject.toml`, type hints on public APIs
- **TypeScript:** ESLint config, strict types
- **Go:** `go vet`, standard conventions

## For AI Agents

Pensyve welcomes contributions from AI coding agents. The repo is structured as flat subprojects (`pensyve-core/`, `pensyve-python/`, etc.) with a top-level `Makefile` and CI via GitHub Actions. See `CLAUDE.md` for agent-specific guidance.

## License

Contributions are licensed under [Apache 2.0](LICENSE).
