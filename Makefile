.PHONY: build test lint format check

# Build everything
build:
	cargo build
	uv run maturin develop --release -m pensyve-python/Cargo.toml

# Run all tests
test: build
	cargo test
	uv run pytest tests/python/ -v

# Lint
lint:
	cargo clippy --workspace -- -D warnings
	uv run ruff check .
	uv run pyright

# Format
format:
	cargo fmt --all
	uv run ruff format .

# Check everything (CI)
check: lint test
	@echo "All checks passed!"
