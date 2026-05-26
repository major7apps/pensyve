# Pensyve Reliability

Testing, performance, and correctness guarantees for the Pensyve memory runtime.

## Test Suite

All tests run via `make check` (the CI gate):

```bash
make test     # Rust unit + Python integration + TypeScript + Go
make lint     # clippy -D warnings + ruff check + pyright + go vet + eslint
make format   # cargo fmt --check + ruff format --check
```

Individual ecosystem commands:

```bash
cargo test --workspace                    # Rust (includes ignored tests with --include-ignored)
cargo clippy --workspace -- -D warnings   # Zero warnings policy
cargo fmt --all -- --check                # Consistent formatting

.venv/bin/pytest tests/python/ -v         # Python integration tests
cd pensyve-ts && bun test                 # TypeScript SDK tests
cd pensyve-go && go test ./...            # Go SDK tests
```

### Current Test Counts

| Ecosystem  | Tests | Status |
|---|---|---|
| Rust       | 127   | All passing (6 ignored; require model download) |
| Python     | 92    | All passing |
| TypeScript | 38    | All passing |
| Go         | 17    | All passing |
| **Total**  | **274** | **274 passing** |

New code adds tests; test count regressions block merge.

## Memory Model Guarantees

### FSRS Decay

Forgetting curve: `R(t, S) = (1 + t / (9 * S))^(-1)`

- Every retrieval reinforces memory stability (retrieval-induced reinforcement)
- Unaccessed memories decay naturally; no manual cleanup required
- Consolidation archives memories below the retrievability threshold
- Decay parameters match the spaced-repetition research literature

### Bayesian Procedural Reliability

Beta-binomial posterior: `reliability = (successes + 1) / (trials + 2)`

- Starts at 0.5 (uninformative prior)
- Converges with evidence; procedures below 0.1 after 10+ trials are pruned
- Fully deterministic given the same input sequence

### Consolidation ("Dreaming")

- Promotes repeated episodic facts to semantic memories (cosine > 0.8, 2+ episodes)
- Applies FSRS decay to all memories in scope
- Archives below-threshold memories
- Bounded to 60 seconds per cycle

## Performance

### Core Operations

Criterion benchmarks for hot-path operations:

| Operation | Target |
|---|---|
| Vector similarity (single query) | Sub-millisecond |
| BM25 FTS5 lookup | Sub-millisecond |
| FSRS decay calculation | Sub-microsecond |
| Embedding (ONNX, cached model) | ~50ms per query |

### Recall Latency (End-to-End)

Typical recall latency for a full pipeline (embed + retrieve + rerank):

- **p50**: ~3 seconds (includes embedding generation, multi-signal retrieval, cross-encoder reranking)
- Dominated by embedding and reranking; pure retrieval logic is sub-millisecond
- Optional Redis cache (`PENSYVE_REDIS_URL`) reduces repeat queries to single-digit milliseconds

### Storage Efficiency

- SQLite with WAL mode; concurrent reads, serialized writes
- Embeddings stored as raw f32 BLOBs (no encoding overhead)
- FTS5 virtual table for zero-copy BM25 scoring

## Design Principles

Four properties that define Pensyve's reliability posture:

1. **Single binary** — `cargo install pensyve-cli` gives you the full engine. No containers, no orchestration, no external services required.
2. **Offline-first** — SQLite default, ONNX embeddings, local LLM inference. Works on an airplane.
3. **Small footprint** — Under 300 MB total (binary + ONNX models). Runs on resource-constrained hardware.
4. **Local-LLM compatible** — Tier 2 extraction via `llama-cpp-python` with local GGUF models. No data leaves the device unless you opt in.
