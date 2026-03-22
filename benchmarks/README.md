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

## LongMemEval_S Benchmark

Evaluates conversational memory recall against the [LongMemEval](https://github.com/xiaowu0162/LongMemEval)
benchmark (single-session variant). The dataset contains ~500 queries across ~40 history sessions
covering information extraction, temporal reasoning, knowledge updates, and multi-session reasoning.

### Setup

```bash
cd /path/to/pensyve
source .venv/bin/activate

# Install benchmark dependencies (includes huggingface_hub)
uv pip install -e '.[benchmarks]'

# Download and prepare the full LongMemEval_S dataset
python benchmarks/longmemeval/prepare.py
```

The prepare script downloads from HuggingFace (`leowei/LongMemEval`), converts to the
benchmark harness format, and saves to `benchmarks/longmemeval/data/`.

If the dataset requires authentication, log in first:
```bash
huggingface-cli login
```

### Run

```bash
# Run with the full dataset
python benchmarks/longmemeval/run.py --data-dir benchmarks/longmemeval/data/ --verbose

# Run with the builtin test dataset (5 conversations, 16 queries)
python benchmarks/longmemeval/run.py --verbose

# Adjust recall limit
python benchmarks/longmemeval/run.py --data-dir benchmarks/longmemeval/data/ --limit 20
```

### Prepare Script Options

```bash
# Download to a custom directory
python benchmarks/longmemeval/prepare.py --output-dir /tmp/longmemeval_data

# Force re-download even if data exists
python benchmarks/longmemeval/prepare.py --force
```

### Metrics

- **Accuracy**: % of queries where the gold answer appears (case-insensitive) in recalled memories
- **Ingest time**: Time to process all conversations as episodes
- **Avg query**: Average recall latency per query
- **Per-query results**: Hit/miss breakdown by query (with `--verbose`)
