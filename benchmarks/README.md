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

## LoCoMo Benchmark

Evaluates conversational memory across 4 subcategories from the
[LoCoMo](https://arxiv.org/abs/2402.10691) benchmark:

- **Temporal**: ordering events correctly in time
- **Multi-hop**: connecting facts across multiple conversations
- **Contradictory**: handling updated/conflicting information
- **Aggregation**: summarizing across multiple data points

### Run

```bash
cd /path/to/pensyve
source .venv/bin/activate

# Run with the builtin test dataset (5 conversations, 8 queries)
python -m benchmarks.locomo.run --verbose

# Run with an external dataset
python -m benchmarks.locomo.run --data-dir /path/to/locomo_data/ --verbose

# Adjust recall limit
python -m benchmarks.locomo.run --limit 20

# Save results to JSON
python -m benchmarks.locomo.run --output results/locomo.json
```

The external dataset directory should contain `conversations.json` and `queries.json`.

### Metrics

- **Accuracy**: % of queries where the gold answer appears (case-insensitive) in recalled memories
- **Per-category accuracy**: Breakdown by temporal, multihop, contradictory, and aggregation
- **Ingest time**: Time to process all conversations as episodes
- **Query time**: Total recall latency across all queries
- **Missed queries**: Details of failed queries (with `--output`)

## MemoryArena Benchmark

Evaluates agentic memory — not just recall accuracy, but whether an agent makes correct
decisions based on memory. Tests four categories:

- **Preference recall**: Does the agent remember and apply user preferences?
- **Mistake avoidance**: Does the agent avoid repeating known-bad actions?
- **Procedure selection**: Does the agent use the correct procedure when multiple exist?
- **Contradiction handling**: Does the agent use the latest information when facts change?

### Run

```bash
cd /path/to/pensyve
source .venv/bin/activate
python -m benchmarks.memoryarena.run --verbose

# Save results to JSON
python -m benchmarks.memoryarena.run --verbose --output results/memoryarena.json
```

### Metrics

- **Accuracy**: % of scenarios where the correct action keyword is recalled and the incorrect one is not
- **Safety rate**: % of scenarios where the known-bad action is NOT recalled (1 - incorrect/total)
- **Ingest time**: Time to process all scenario conversations
- **Query time**: Total recall latency across all queries
- **Per-category breakdown**: Accuracy split by preference, mistake_avoidance, procedure_selection, contradiction

## Open Evaluation Dataset

Community-contributed evaluation scenarios in `eval-dataset/`. The dataset uses the
MemoryArena scenario format and can be loaded directly or used with other memory systems.

- `eval-dataset/scenarios.json` — MemoryArena-format scenarios
- See `eval-dataset/README.md` for the schema and contribution guidelines
