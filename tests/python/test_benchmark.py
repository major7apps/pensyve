"""Tests for the LongMemEval_S benchmark infrastructure."""

from __future__ import annotations

import json
import tempfile
from pathlib import Path

from benchmarks.longmemeval.dataset import (
    MemEvalConversation,
    MemEvalDataset,
    MemEvalQuery,
    load_longmemeval_s,
)
from benchmarks.longmemeval.evaluate import evaluate


class TestDataset:
    """Tests for dataset loading and structure."""

    def test_builtin_dataset_loads(self):
        dataset = load_longmemeval_s()
        assert dataset.num_conversations == 5
        assert dataset.num_queries == 16

    def test_builtin_conversations_have_messages(self):
        dataset = load_longmemeval_s()
        for conv in dataset.conversations:
            assert len(conv.messages) >= 2
            assert all("role" in m and "content" in m for m in conv.messages)

    def test_builtin_queries_have_gold_answers(self):
        dataset = load_longmemeval_s()
        for query in dataset.queries:
            assert query.gold_answer, f"Query {query.query_id} missing gold_answer"
            assert query.question, f"Query {query.query_id} missing question"

    def test_query_conversation_ids_are_valid(self):
        dataset = load_longmemeval_s()
        conv_ids = {c.conversation_id for c in dataset.conversations}
        for query in dataset.queries:
            assert query.conversation_id in conv_ids, (
                f"Query {query.query_id} references unknown conversation "
                f"{query.conversation_id}"
            )

    def test_load_from_json_files(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            convs = [
                {
                    "conversation_id": "test-conv",
                    "messages": [
                        {"role": "user", "content": "Hello"},
                        {"role": "assistant", "content": "Hi there"},
                    ],
                }
            ]
            queries = [
                {
                    "query_id": "test-q",
                    "question": "What did I say?",
                    "gold_answer": "Hello",
                    "conversation_id": "test-conv",
                }
            ]

            with open(Path(tmp_dir) / "conversations.json", "w") as f:
                json.dump(convs, f)
            with open(Path(tmp_dir) / "queries.json", "w") as f:
                json.dump(queries, f)

            dataset = load_longmemeval_s(tmp_dir)
            assert dataset.num_conversations == 1
            assert dataset.num_queries == 1
            assert dataset.queries[0].gold_answer == "Hello"

    def test_fallback_to_builtin_on_missing_dir(self):
        dataset = load_longmemeval_s("/nonexistent/path/that/does/not/exist")
        assert dataset.num_conversations == 5, "Should fall back to builtin dataset"

    def test_dataset_properties(self):
        dataset = MemEvalDataset(
            conversations=[
                MemEvalConversation("c1", [{"role": "user", "content": "x"}]),
                MemEvalConversation("c2", [{"role": "user", "content": "y"}]),
            ],
            queries=[
                MemEvalQuery("q1", "question?", "answer", "c1"),
            ],
        )
        assert dataset.num_conversations == 2
        assert dataset.num_queries == 1


class TestEvaluator:
    """Tests for the evaluation pipeline."""

    def test_evaluate_returns_report(self):
        dataset = load_longmemeval_s()
        report = evaluate(dataset, recall_limit=5)

        assert report.total_queries == 16
        assert report.hits + report.misses == 16
        assert 0.0 <= report.accuracy <= 1.0
        assert report.ingest_time_ms > 0
        assert report.query_time_ms > 0

    def test_evaluate_results_count_matches(self):
        dataset = load_longmemeval_s()
        report = evaluate(dataset, recall_limit=5)

        assert len(report.results) == report.total_queries

    def test_evaluate_miss_rate(self):
        dataset = load_longmemeval_s()
        report = evaluate(dataset, recall_limit=5)

        expected_miss = 1.0 - report.accuracy
        assert abs(report.miss_rate - expected_miss) < 1e-6

    def test_evaluate_small_dataset(self):
        """Test with a minimal single-conversation, single-query dataset."""
        dataset = MemEvalDataset(
            conversations=[
                MemEvalConversation(
                    conversation_id="mini",
                    messages=[
                        {"role": "user", "content": "My favorite color is blue."},
                        {"role": "assistant", "content": "Blue is a nice color!"},
                    ],
                ),
            ],
            queries=[
                MemEvalQuery(
                    query_id="mini-q",
                    question="What is my favorite color?",
                    gold_answer="blue",
                    conversation_id="mini",
                ),
            ],
        )
        report = evaluate(dataset, recall_limit=5)

        assert report.total_queries == 1
        assert len(report.results) == 1
