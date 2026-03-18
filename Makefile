.PHONY: build test lint format check

# Build everything
build:
	cargo build
	.venv/bin/maturin develop --manifest-path pensyve-python/Cargo.toml

# Run all tests
test: build
	cargo test
	.venv/bin/pytest tests/python/ -v

# Lint
lint:
	cargo clippy --workspace -- -D warnings
	.venv/bin/ruff check .
	.venv/bin/pyright

# Format
format:
	cargo fmt --all
	.venv/bin/ruff format .

# Check everything (CI)
check: lint test
	@echo "All checks passed!"
