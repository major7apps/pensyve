"""MemoryArena evaluation harness."""

from __future__ import annotations

import time
from dataclasses import dataclass, field

import pensyve

from .dataset import ArenaScenario


@dataclass
class ArenaResults:
    total: int = 0
    correct: int = 0
    incorrect: int = 0
    neutral: int = 0  # neither correct nor incorrect action found
    by_category: dict[str, dict[str, int]] = field(default_factory=dict)
    ingest_time_ms: float = 0
    query_time_ms: float = 0
    failures: list[dict] = field(default_factory=list)

    @property
    def accuracy(self) -> float:
        return self.correct / self.total if self.total > 0 else 0.0

    @property
    def safety_rate(self) -> float:
        """Rate at which the agent avoids known-bad actions."""
        return 1.0 - (self.incorrect / self.total) if self.total > 0 else 1.0


def evaluate(
    scenarios: list[ArenaScenario],
    limit: int = 10,
    verbose: bool = False,
) -> ArenaResults:
    """Run MemoryArena evaluation."""
    import tempfile

    results = ArenaResults()

    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir, namespace="arena-eval")
        user = p.entity("arena-user")
        agent = p.entity("arena-agent")

        for scenario in scenarios:
            category = scenario.category
            if category not in results.by_category:
                results.by_category[category] = {
                    "correct": 0,
                    "incorrect": 0,
                    "neutral": 0,
                    "total": 0,
                }

            # Ingest setup messages
            t0 = time.time()
            ep = p.episode(user, agent)
            with ep:
                for msg in scenario.setup_messages:
                    ep.message(msg["role"], msg["content"])
            results.ingest_time_ms += (time.time() - t0) * 1000

            # Query
            t0 = time.time()
            memories = p.recall(scenario.test_query, limit=limit)
            results.query_time_ms += (time.time() - t0) * 1000

            recalled_text = " ".join(m.content for m in memories).lower()
            has_correct = scenario.correct_action.lower() in recalled_text
            has_incorrect = scenario.incorrect_action.lower() in recalled_text

            results.total += 1
            results.by_category[category]["total"] += 1

            if has_correct and not has_incorrect:
                results.correct += 1
                results.by_category[category]["correct"] += 1
                status = "CORRECT"
            elif has_incorrect:
                results.incorrect += 1
                results.by_category[category]["incorrect"] += 1
                status = "INCORRECT"
                results.failures.append(
                    {
                        "scenario_id": scenario.scenario_id,
                        "category": category,
                        "question": scenario.test_query,
                        "expected": scenario.correct_action,
                        "got_incorrect": scenario.incorrect_action,
                    }
                )
            else:
                results.neutral += 1
                results.by_category[category]["neutral"] += 1
                status = "NEUTRAL"

            if verbose:
                print(f"  [{status}] [{category}] {scenario.test_query}")

    return results
