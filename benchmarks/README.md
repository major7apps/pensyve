# Pensyve Benchmarks

## LongMemEval_S — public results

Pensyve's LongMemEval_S results (91.0% Sonnet 4.6, 86.8% Haiku 4.5,
91.4% Opus 4.6) and the full reader-ablation methodology are documented
in the technical report:

[`../docs/benchmarks/longmemeval-reader-ablation.pdf`](../docs/benchmarks/longmemeval-reader-ablation.pdf)

The reproduction harness for those numbers lives in a private research
repo until findings are formally released. The public repo intentionally
does not ship the reader/judge pipeline so the methodology stays
single-source-of-truth in the PDF.

## Synthetic Benchmark

Generates conversations with planted facts and measures top-k recall
accuracy. Useful as a quick local smoke test that does not require an
external dataset, API keys, or a reader model.

### Run

```bash
cd /path/to/pensyve
source .venv/bin/activate

# Generate + evaluate
python benchmarks/synthetic/run.py --generate --evaluate --verbose

# Just evaluate (reuses existing data)
python benchmarks/synthetic/run.py --evaluate
```

### Metrics

- **Accuracy**: % of queries where at least one expected keyword appears
  in top-5 recall results
- **Ingest time**: Time to process all conversations
- **Avg recall**: Average latency per query

## Open Evaluation Dataset

Community-contributed evaluation scenarios in `eval-dataset/`. The
dataset uses the MemoryArena scenario format and can be loaded directly
or used with other memory systems.

- `eval-dataset/scenarios.json` — scenarios in MemoryArena format
- `eval-dataset/README.md` — schema and contribution guidelines
