# Contributing to Pensyve

Thank you for your interest in contributing to Pensyve — a universal memory runtime for AI agents.

## Development Setup

### Prerequisites

- Rust 1.88+ (install via [rustup](https://rustup.rs/))
- Python 3.12+ with [uv](https://docs.astral.sh/uv/) (for Python SDK)
- SQLite 3.40+ (included on most systems)

### Quick Start

```bash
git clone https://github.com/major7apps/pensyve.git
cd pensyve
cargo test -p pensyve-core --no-default-features
```

### Running Tests

```bash
# Core engine tests (no model download required)
cargo test -p pensyve-core --no-default-features

# Full test suite (requires embedding model download ~90MB)
cargo test -p pensyve-core

# Benchmarks
cargo bench -p pensyve-core --bench cognitive_engine

# Python SDK
cd pensyve-python && uv run maturin develop && uv run pytest
```

### Project Structure

```
pensyve/
├── pensyve-core/          # Rust core engine
│   ├── src/
│   │   ├── activation.rs  # ACT-R base-level activation
│   │   ├── retrieval.rs   # RRF retrieval pipeline
│   │   ├── graph.rs       # Typed-edge beam search
│   │   ├── consolidation.rs # Memory consolidation
│   │   ├── decay.rs       # FSRS memory decay
│   │   └── ...
│   ├── benches/           # Criterion benchmarks
│   └── tests/             # Integration tests
├── pensyve-python/        # Python SDK (PyO3)
├── pensyve-mcp/           # MCP server
├── pensyve-cli/           # CLI tool
├── pensyve-benchmarks/    # Evaluation framework
└── pensyve-wasm/          # WASM build
```

## How to Contribute

### Reporting Issues

Use GitHub Issues with the appropriate template:
- **Bug Report**: Include Rust version, OS, and minimal reproduction steps
- **Feature Request**: Describe the use case and proposed solution
- **Benchmark**: Share your benchmark results (hardware, corpus size, metrics)

### Pull Requests

1. Fork the repository
2. Create a feature branch: `git checkout -b feat/your-feature`
3. Write tests first (TDD)
4. Ensure all tests pass: `cargo test --workspace --no-default-features`
5. Run clippy: `cargo clippy --workspace -- -D warnings`
6. Commit with conventional commits: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`
7. Open a PR against `main`

### Code Style

- Follow existing patterns in the codebase
- Use `rustfmt` defaults
- Add `#[cfg(test)] mod tests` with unit tests in each module
- Document public APIs with `///` doc comments
- Keep functions focused and files under 500 lines

### Architecture Decisions

The retrieval engine uses a Cognitive Activation Model grounded in ACT-R theory:
- **Base-Level Activation** B(m) — power-law decay over access history
- **Reciprocal Rank Fusion** — 6 independent rankings merged without score normalization
- **Typed-Edge Beam Search** — intent-aware graph traversal
- See the [white paper](papers/) for full algorithmic details

## License

Pensyve is licensed under Apache 2.0. By contributing, you agree that your contributions will be licensed under the same terms.
