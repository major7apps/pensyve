"""LongMemEval_S evaluator.

Ingests conversations via the Pensyve Python SDK (episodes), runs queries
via recall, and checks if gold answers appear in the recalled text.
"""

from __future__ import annotations

import tempfile
import time
from dataclasses import dataclass, field

import pensyve
from benchmarks.longmemeval.dataset import MemEvalDataset, MemEvalQuery


@dataclass
class QueryResult:
    """Result of a single query evaluation."""

    query_id: str
    question: str
    gold_answer: str
    hit: bool
    recalled_texts: list[str]
    elapsed_ms: float


@dataclass
class EvalReport:
    """Aggregated evaluation report."""

    accuracy: float
    total_queries: int
    hits: int
    misses: int
    results: list[QueryResult] = field(default_factory=list)
    ingest_time_ms: float = 0.0
    query_time_ms: float = 0.0

    @property
    def miss_rate(self) -> float:
        return 1.0 - self.accuracy


def evaluate(
    dataset: MemEvalDataset,
    *,
    storage_path: str | None = None,
    recall_limit: int = 10,
    weights: list[float] | None = None,
) -> EvalReport:
    """Run the full evaluation pipeline.

    1. Creates a fresh Pensyve instance.
    2. Ingests all conversations as episodes.
    3. Runs each query via recall.
    4. Checks if the gold answer appears in the recalled text.

    Args:
        dataset: The MemEvalDataset to evaluate against.
        storage_path: Optional storage path. Uses a temp dir if None.
        recall_limit: Number of memories to recall per query.
        weights: Optional 8-element weight vector for retrieval config override.

    Returns:
        An EvalReport with accuracy, hits, misses, and per-query results.
    """
    tmp_dir = None
    if storage_path is None:
        tmp_dir = tempfile.mkdtemp(prefix="pensyve_eval_")
        storage_path = tmp_dir

    try:
        return _run_evaluation(dataset, storage_path, recall_limit, weights)
    finally:
        if tmp_dir is not None:
            import shutil

            shutil.rmtree(tmp_dir, ignore_errors=True)


def _run_evaluation(
    dataset: MemEvalDataset,
    storage_path: str,
    recall_limit: int,
    weights: list[float] | None,
) -> EvalReport:
    """Inner evaluation logic."""
    p = pensyve.Pensyve(path=storage_path, namespace="longmemeval")

    user = p.entity("eval-user", "user")
    assistant = p.entity("eval-assistant", "agent")

    # Phase 1: Ingest conversations as episodes.
    ingest_start = time.perf_counter()

    for conv in dataset.conversations:
        with p.episode(user, assistant) as ep:
            for msg in conv.messages:
                ep.message(msg["role"], msg["content"])

    ingest_elapsed_ms = (time.perf_counter() - ingest_start) * 1000.0

    # Phase 2: Run queries and evaluate.
    query_start = time.perf_counter()
    results: list[QueryResult] = []

    for query in dataset.queries:
        result = _evaluate_query(p, query, recall_limit)
        results.append(result)

    query_elapsed_ms = (time.perf_counter() - query_start) * 1000.0

    # Aggregate.
    hits = sum(1 for r in results if r.hit)
    misses = len(results) - hits
    accuracy = hits / len(results) if results else 0.0

    return EvalReport(
        accuracy=accuracy,
        total_queries=len(results),
        hits=hits,
        misses=misses,
        results=results,
        ingest_time_ms=ingest_elapsed_ms,
        query_time_ms=query_elapsed_ms,
    )


def _evaluate_query(
    p: pensyve.Pensyve,
    query: MemEvalQuery,
    recall_limit: int,
) -> QueryResult:
    """Evaluate a single query by checking if gold_answer appears in recalled text."""
    q_start = time.perf_counter()

    memories = p.recall(query.question, limit=recall_limit)

    elapsed_ms = (time.perf_counter() - q_start) * 1000.0

    recalled_texts = [m.content for m in memories]

    # Check if gold answer appears (case-insensitive) in any recalled text.
    gold_lower = query.gold_answer.lower()
    hit = any(gold_lower in text.lower() for text in recalled_texts)

    return QueryResult(
        query_id=query.query_id,
        question=query.question,
        gold_answer=query.gold_answer,
        hit=hit,
        recalled_texts=recalled_texts,
        elapsed_ms=elapsed_ms,
    )
