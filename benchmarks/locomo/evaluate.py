"""LoCoMo evaluation harness."""

from __future__ import annotations

import time
from dataclasses import dataclass, field

import pensyve

from .dataset import LoCoMoConversation, LoCoMoQuery


@dataclass
class LoCoMoResults:
    total: int = 0
    hits: int = 0
    misses: int = 0
    by_category: dict[str, dict[str, int]] = field(default_factory=dict)
    ingest_time_ms: float = 0
    query_time_ms: float = 0
    missed_queries: list[dict] = field(default_factory=list)

    @property
    def accuracy(self) -> float:
        return self.hits / self.total if self.total > 0 else 0.0


def evaluate(
    conversations: list[LoCoMoConversation],
    queries: list[LoCoMoQuery],
    limit: int = 10,
    verbose: bool = False,
) -> LoCoMoResults:
    """Run LoCoMo evaluation."""
    import tempfile

    results = LoCoMoResults()

    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir, namespace="locomo-eval")

        # Ingest conversations as episodes
        t0 = time.time()
        user_entity = p.entity("locomo-user")
        assistant_entity = p.entity("locomo-assistant")

        for conv in conversations:
            ep = p.episode(user_entity, assistant_entity)
            with ep:
                for msg in conv.messages:
                    ep.message(msg["role"], msg["content"])

        results.ingest_time_ms = (time.time() - t0) * 1000

        # Run queries
        t0 = time.time()
        for query in queries:
            category = query.category
            if category not in results.by_category:
                results.by_category[category] = {"hits": 0, "misses": 0, "total": 0}

            results.total += 1
            results.by_category[category]["total"] += 1

            memories = p.recall(query.question, limit=limit)
            recalled_text = " ".join(m.content for m in memories).lower()
            gold = query.gold_answer.lower()

            hit = gold in recalled_text
            if hit:
                results.hits += 1
                results.by_category[category]["hits"] += 1
            else:
                results.misses += 1
                results.by_category[category]["misses"] += 1
                results.missed_queries.append(
                    {
                        "query_id": query.query_id,
                        "question": query.question,
                        "gold_answer": query.gold_answer,
                        "category": category,
                    }
                )

            if verbose:
                status = "HIT" if hit else "MISS"
                print(f"  [{status}] [{category}] {query.question} (expected: {gold})")

        results.query_time_ms = (time.time() - t0) * 1000

    return results
