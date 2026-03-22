"""LoCoMo dataset loader.

LoCoMo evaluates 4 categories of conversational memory:
- Temporal: ordering events correctly in time
- Multi-hop: connecting facts across multiple conversations
- Contradictory: handling updated/conflicting information
- Aggregation: summarizing across multiple data points

Uses builtin test data by default, with support for external datasets.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class LoCoMoQuery:
    query_id: str
    question: str
    gold_answer: str
    category: str  # "temporal", "multihop", "contradictory", "aggregation"
    conversation_ids: list[str] = field(default_factory=list)


@dataclass
class LoCoMoConversation:
    conversation_id: str
    messages: list[dict[str, str]]
    timestamp: str = ""


def load_builtin() -> tuple[list[LoCoMoConversation], list[LoCoMoQuery]]:
    """Load builtin test dataset for development."""
    conversations = [
        LoCoMoConversation(
            conversation_id="lc-1",
            messages=[
                {"role": "user", "content": "I just got a new job at Google as a senior engineer"},
                {"role": "assistant", "content": "Congratulations! That's a great role."},
            ],
            timestamp="2026-01-15",
        ),
        LoCoMoConversation(
            conversation_id="lc-2",
            messages=[
                {"role": "user", "content": "Actually I changed my mind, I accepted the offer from Meta instead"},
                {"role": "assistant", "content": "Meta is also a great choice!"},
            ],
            timestamp="2026-01-20",
        ),
        LoCoMoConversation(
            conversation_id="lc-3",
            messages=[
                {"role": "user", "content": "I've been learning Rust for 3 months now. Also started Go last week."},
                {"role": "assistant", "content": "Both are great systems languages."},
            ],
            timestamp="2026-02-01",
        ),
        LoCoMoConversation(
            conversation_id="lc-4",
            messages=[
                {"role": "user", "content": "My Rust project uses async with Tokio. The Go project is a CLI tool."},
                {"role": "assistant", "content": "Nice combination of async and CLI work."},
            ],
            timestamp="2026-02-15",
        ),
        LoCoMoConversation(
            conversation_id="lc-5",
            messages=[
                {"role": "user", "content": "I ran 5 miles on Monday, 3 miles Tuesday, and 7 miles Wednesday"},
                {"role": "assistant", "content": "That's a solid training week so far."},
            ],
            timestamp="2026-03-01",
        ),
    ]

    queries = [
        # Temporal
        LoCoMoQuery("lq-1", "Which company did the user accept a job at first?", "Google", "temporal", ["lc-1", "lc-2"]),
        LoCoMoQuery("lq-2", "Did the user learn Rust or Go first?", "Rust", "temporal", ["lc-3"]),
        # Multi-hop
        LoCoMoQuery("lq-3", "What async runtime does the user use for their Rust project?", "Tokio", "multihop", ["lc-3", "lc-4"]),
        LoCoMoQuery("lq-4", "What type of project is the user building in Go?", "CLI tool", "multihop", ["lc-3", "lc-4"]),
        # Contradictory
        LoCoMoQuery("lq-5", "Where does the user currently work?", "Meta", "contradictory", ["lc-1", "lc-2"]),
        LoCoMoQuery("lq-6", "Did the user end up working at Google?", "No", "contradictory", ["lc-1", "lc-2"]),
        # Aggregation
        LoCoMoQuery("lq-7", "How many total miles did the user run this week?", "15", "aggregation", ["lc-5"]),
        LoCoMoQuery("lq-8", "How many programming languages is the user learning?", "2", "aggregation", ["lc-3"]),
    ]

    return conversations, queries


def load_external(data_dir: Path) -> tuple[list[LoCoMoConversation], list[LoCoMoQuery]]:
    """Load external LoCoMo dataset from JSON files."""
    conv_path = data_dir / "conversations.json"
    query_path = data_dir / "queries.json"

    with open(conv_path) as f:
        raw_convs = json.load(f)
    with open(query_path) as f:
        raw_queries = json.load(f)

    conversations = [
        LoCoMoConversation(
            conversation_id=c["conversation_id"],
            messages=c["messages"],
            timestamp=c.get("timestamp", ""),
        )
        for c in raw_convs
    ]

    queries = [
        LoCoMoQuery(
            query_id=q["query_id"],
            question=q["question"],
            gold_answer=q["gold_answer"],
            category=q["category"],
            conversation_ids=q.get("conversation_ids", []),
        )
        for q in raw_queries
    ]

    return conversations, queries
