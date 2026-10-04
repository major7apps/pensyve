# Contributing to Pensyve

Pensyve is an open-source project in maintenance mode. Releases cover security
fixes and dependency updates, with no new features planned. Please read the
[maintenance policy](MAINTENANCE.md) before starting work. Issues and pull
requests are handled on a best-effort basis, with no guaranteed response time.

Use [GitHub issues](https://github.com/major7apps/pensyve/issues) for public
bug reports and documentation corrections. Report security issues privately
as described in [SECURITY.md](SECURITY.md).

## Development setup

You need Rust 1.94 or newer, Python 3.10 or newer, and
[uv](https://docs.astral.sh/uv/). TypeScript SDK work also needs
[Bun](https://bun.sh), and Go SDK work needs [Go 1.21 or newer](https://go.dev).

```bash
git clone https://github.com/major7apps/pensyve.git
cd pensyve
uv sync --extra dev
make build
```

`make build` compiles Rust and builds the native Python extension with
Maturin. Build the extension before running Python examples or tests.

## Checks

Run the repository checks before pushing:

```bash
make check
```

`make check` runs Clippy, Ruff, Pyright, and the Rust and Python tests.
Run the SDK checks separately when changing those components:

```bash
# Run each command from the repository root.
(cd pensyve-ts && bun install && bun run check)
(cd pensyve-go && go vet ./... && go test ./...)
(cd pensyve-wasm && cargo check)
```

To run a single Rust crate or the Python integration tests:

```bash
cargo test -p pensyve-core
uv run pytest tests/python/ -v
```

CI also runs checks with PostgreSQL and other feature combinations. See
the [CI workflow](.github/workflows/ci.yml) for the full set of jobs and
[RELIABILITY.md](docs/RELIABILITY.md) for test details.

## Submitting a change

1. Check existing issues and the maintenance policy, then create a branch from `main` in your fork.
2. Keep the change focused, add tests for behavior changes, and update affected documentation.
3. Run the relevant checks and open a pull request that explains the problem, the change, and the results.

Use `cargo fmt` and Clippy for Rust, Ruff for Python, ESLint for TypeScript,
and `go vet` for Go. Commit messages use conventional prefixes such as
`fix:`, `docs:`, `test:`, and `chore:`.

See [AGENTS.md](AGENTS.md) for the directory map and coding conventions, and
[ARCHITECTURE.md](docs/ARCHITECTURE.md) for the engine's component boundaries.

## License

Contributions are licensed under [Apache 2.0](LICENSE).
