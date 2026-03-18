# Pensyve Benchmarks

## Synthetic Benchmark

Generates conversations with planted facts, then measures recall accuracy.

### Run

```bash
# Generate + evaluate
cd /path/to/pensyve
source .venv/bin/activate
python benchmarks/synthetic/run.py --generate --evaluate --verbose

# Just evaluate (reuses existing data)
python benchmarks/synthetic/run.py --evaluate
```

### Metrics

- **Accuracy**: % of queries where at least one expected keyword appears in top-5 recall results
- **Ingest time**: Time to process all conversations
- **Avg recall**: Average latency per query

## LongMemEval (TODO)

Full LongMemEval benchmark integration planned for Phase 3.
