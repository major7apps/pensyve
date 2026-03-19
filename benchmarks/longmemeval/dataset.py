"""LongMemEval_S dataset loader.

Provides dataclasses and loading functions for the LongMemEval_S benchmark,
a small-scale conversational memory evaluation dataset.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class MemEvalConversation:
    """A single conversation to be ingested as an episode."""

    conversation_id: str
    messages: list[dict[str, str]]  # [{"role": "user"|"assistant", "content": "..."}]
    metadata: dict[str, str] = field(default_factory=dict)


@dataclass
class MemEvalQuery:
    """A query with a known gold answer for evaluation."""

    query_id: str
    question: str
    gold_answer: str
    conversation_id: str
    difficulty: str = "easy"  # "easy", "medium", "hard"


@dataclass
class MemEvalDataset:
    """Complete benchmark dataset."""

    conversations: list[MemEvalConversation]
    queries: list[MemEvalQuery]

    @property
    def num_conversations(self) -> int:
        return len(self.conversations)

    @property
    def num_queries(self) -> int:
        return len(self.queries)


def load_longmemeval_s(data_dir: str | Path | None = None) -> MemEvalDataset:
    """Load the LongMemEval_S dataset.

    If data_dir is provided and contains conversations.json + queries.json,
    load from those files. Otherwise, fall back to the builtin test dataset.

    Args:
        data_dir: Optional directory containing JSON dataset files.

    Returns:
        A MemEvalDataset instance.
    """
    if data_dir is not None:
        data_path = Path(data_dir)
        conv_file = data_path / "conversations.json"
        query_file = data_path / "queries.json"

        if conv_file.exists() and query_file.exists():
            return _load_from_json(conv_file, query_file)

    return _builtin_test_dataset()


def _load_from_json(conv_file: Path, query_file: Path) -> MemEvalDataset:
    """Load dataset from JSON files."""
    with open(conv_file) as f:
        raw_convs = json.load(f)

    conversations = [
        MemEvalConversation(
            conversation_id=c["conversation_id"],
            messages=c["messages"],
            metadata=c.get("metadata", {}),
        )
        for c in raw_convs
    ]

    with open(query_file) as f:
        raw_queries = json.load(f)

    queries = [
        MemEvalQuery(
            query_id=q["query_id"],
            question=q["question"],
            gold_answer=q["gold_answer"],
            conversation_id=q["conversation_id"],
            difficulty=q.get("difficulty", "easy"),
        )
        for q in raw_queries
    ]

    return MemEvalDataset(conversations=conversations, queries=queries)


def _builtin_test_dataset() -> MemEvalDataset:
    """Return a small builtin test dataset with 5 conversations and 16 queries.

    The conversations cover diverse topics to test retrieval across different
    memory types and temporal ranges.
    """
    conversations = [
        MemEvalConversation(
            conversation_id="conv-001",
            messages=[
                {"role": "user", "content": "I'm working on a project called Helios."},
                {
                    "role": "assistant",
                    "content": "That's interesting! What is Helios about?",
                },
                {
                    "role": "user",
                    "content": (
                        "Helios is a solar panel monitoring system written in Rust. "
                        "It uses PostgreSQL for data storage and runs on Raspberry Pi."
                    ),
                },
                {
                    "role": "assistant",
                    "content": (
                        "A Rust-based solar monitoring system on Raspberry Pi sounds great. "
                        "PostgreSQL is a solid choice for time-series data."
                    ),
                },
            ],
        ),
        MemEvalConversation(
            conversation_id="conv-002",
            messages=[
                {
                    "role": "user",
                    "content": "My dog's name is Biscuit. She's a golden retriever.",
                },
                {
                    "role": "assistant",
                    "content": "Biscuit is a lovely name for a golden retriever!",
                },
                {
                    "role": "user",
                    "content": "She's 3 years old and loves swimming in the lake near our house.",
                },
                {
                    "role": "assistant",
                    "content": (
                        "Golden retrievers are natural swimmers. "
                        "That's wonderful that she has a lake nearby."
                    ),
                },
            ],
        ),
        MemEvalConversation(
            conversation_id="conv-003",
            messages=[
                {
                    "role": "user",
                    "content": "I just got back from a trip to Tokyo. It was amazing.",
                },
                {
                    "role": "assistant",
                    "content": "Tokyo is wonderful! What were the highlights?",
                },
                {
                    "role": "user",
                    "content": (
                        "The best part was visiting Akihabara and trying authentic ramen "
                        "at a small shop in Shinjuku. The chef's name was Tanaka-san."
                    ),
                },
                {
                    "role": "assistant",
                    "content": (
                        "Akihabara is incredible for electronics and anime culture. "
                        "Authentic ramen in Shinjuku sounds like a perfect experience."
                    ),
                },
            ],
        ),
        MemEvalConversation(
            conversation_id="conv-004",
            messages=[
                {
                    "role": "user",
                    "content": (
                        "I'm learning to play guitar. I practice every Tuesday and Thursday "
                        "evening for about an hour."
                    ),
                },
                {
                    "role": "assistant",
                    "content": "That's a great practice schedule! What style are you learning?",
                },
                {
                    "role": "user",
                    "content": (
                        "Mostly blues and jazz. My teacher is Sarah, and she recommended "
                        "starting with pentatonic scales."
                    ),
                },
                {
                    "role": "assistant",
                    "content": (
                        "Pentatonic scales are the foundation of blues guitar. "
                        "Sarah sounds like a knowledgeable teacher."
                    ),
                },
            ],
        ),
        MemEvalConversation(
            conversation_id="conv-005",
            messages=[
                {
                    "role": "user",
                    "content": (
                        "I started a new job at Meridian Labs as a senior engineer last month."
                    ),
                },
                {
                    "role": "assistant",
                    "content": "Congratulations on the new role! What does Meridian Labs do?",
                },
                {
                    "role": "user",
                    "content": (
                        "They build machine learning infrastructure for healthcare. "
                        "My team works on the data pipeline using Apache Kafka and Spark."
                    ),
                },
                {
                    "role": "assistant",
                    "content": (
                        "Healthcare ML is a fascinating field. Kafka and Spark are excellent "
                        "choices for robust data pipelines."
                    ),
                },
            ],
        ),
    ]

    queries = [
        # conv-001 queries
        MemEvalQuery(
            query_id="q-001",
            question="What project am I working on?",
            gold_answer="Helios",
            conversation_id="conv-001",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-002",
            question="What programming language is Helios written in?",
            gold_answer="Rust",
            conversation_id="conv-001",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-003",
            question="What database does Helios use?",
            gold_answer="PostgreSQL",
            conversation_id="conv-001",
            difficulty="medium",
        ),
        # conv-002 queries
        MemEvalQuery(
            query_id="q-004",
            question="What is my dog's name?",
            gold_answer="Biscuit",
            conversation_id="conv-002",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-005",
            question="What breed is my dog?",
            gold_answer="golden retriever",
            conversation_id="conv-002",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-006",
            question="What does my dog like to do?",
            gold_answer="swimming",
            conversation_id="conv-002",
            difficulty="medium",
        ),
        # conv-003 queries
        MemEvalQuery(
            query_id="q-007",
            question="Where did I travel recently?",
            gold_answer="Tokyo",
            conversation_id="conv-003",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-008",
            question="What district did I visit for electronics?",
            gold_answer="Akihabara",
            conversation_id="conv-003",
            difficulty="medium",
        ),
        MemEvalQuery(
            query_id="q-009",
            question="What was the chef's name at the ramen shop?",
            gold_answer="Tanaka",
            conversation_id="conv-003",
            difficulty="hard",
        ),
        # conv-004 queries
        MemEvalQuery(
            query_id="q-010",
            question="What instrument am I learning?",
            gold_answer="guitar",
            conversation_id="conv-004",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-011",
            question="What days do I practice guitar?",
            gold_answer="Tuesday and Thursday",
            conversation_id="conv-004",
            difficulty="medium",
        ),
        MemEvalQuery(
            query_id="q-012",
            question="Who is my guitar teacher?",
            gold_answer="Sarah",
            conversation_id="conv-004",
            difficulty="medium",
        ),
        MemEvalQuery(
            query_id="q-013",
            question="What music styles am I learning?",
            gold_answer="blues",
            conversation_id="conv-004",
            difficulty="medium",
        ),
        # conv-005 queries
        MemEvalQuery(
            query_id="q-014",
            question="Where do I work now?",
            gold_answer="Meridian Labs",
            conversation_id="conv-005",
            difficulty="easy",
        ),
        MemEvalQuery(
            query_id="q-015",
            question="What technologies does my team use for the data pipeline?",
            gold_answer="Kafka",
            conversation_id="conv-005",
            difficulty="medium",
        ),
        MemEvalQuery(
            query_id="q-016",
            question="What field does Meridian Labs focus on?",
            gold_answer="healthcare",
            conversation_id="conv-005",
            difficulty="medium",
        ),
    ]

    return MemEvalDataset(conversations=conversations, queries=queries)
