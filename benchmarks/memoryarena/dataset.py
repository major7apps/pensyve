"""MemoryArena dataset — agentic memory evaluation.

Tests whether an agent can:
1. Recall learned preferences and apply them
2. Avoid repeating past mistakes (procedural memory)
3. Use the correct procedure when multiple exist
4. Handle contradictory information correctly
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class ArenaScenario:
    scenario_id: str
    category: str  # "preference", "mistake_avoidance", "procedure_selection", "contradiction"
    setup_messages: list[dict[str, str]]  # conversation history to ingest
    test_query: str  # query to run after ingesting
    correct_action: str  # expected keyword in the recalled response
    incorrect_action: str  # action that should NOT be taken


def load_builtin() -> list[ArenaScenario]:
    """Load builtin MemoryArena test scenarios."""
    return [
        # Preference recall
        ArenaScenario(
            scenario_id="ma-1",
            category="preference",
            setup_messages=[
                {"role": "user", "content": "I always want code examples in Python, never JavaScript"},
                {"role": "assistant", "content": "Noted, I'll use Python for all code examples."},
            ],
            test_query="What language should code examples use?",
            correct_action="python",
            incorrect_action="javascript",
        ),
        ArenaScenario(
            scenario_id="ma-2",
            category="preference",
            setup_messages=[
                {"role": "user", "content": "Use dark mode themes in all UI designs"},
                {"role": "assistant", "content": "Got it, dark mode for all UI work."},
            ],
            test_query="What theme should UI designs use?",
            correct_action="dark",
            incorrect_action="light",
        ),
        # Mistake avoidance
        ArenaScenario(
            scenario_id="ma-3",
            category="mistake_avoidance",
            setup_messages=[
                {"role": "user", "content": "Don't use pip install directly, always use uv"},
                {"role": "assistant", "content": "I'll use uv for all Python package management."},
                {"role": "user", "content": "Last time you used pip and it broke the venv"},
                {"role": "assistant", "content": "Sorry about that, I'll stick to uv going forward."},
            ],
            test_query="How should I install Python packages?",
            correct_action="uv",
            incorrect_action="pip",
        ),
        ArenaScenario(
            scenario_id="ma-4",
            category="mistake_avoidance",
            setup_messages=[
                {"role": "user", "content": "Never use force push to main, it caused an outage last month"},
                {"role": "assistant", "content": "Understood, no force pushing to main."},
            ],
            test_query="Can I force push to the main branch?",
            correct_action="no",
            incorrect_action="yes",
        ),
        # Procedure selection
        ArenaScenario(
            scenario_id="ma-5",
            category="procedure_selection",
            setup_messages=[
                {"role": "user", "content": "For database migrations, always use alembic, not raw SQL"},
                {"role": "assistant", "content": "I'll use alembic for all migration work."},
            ],
            test_query="How should I handle database schema changes?",
            correct_action="alembic",
            incorrect_action="sql",
        ),
        ArenaScenario(
            scenario_id="ma-6",
            category="procedure_selection",
            setup_messages=[
                {"role": "user", "content": "Deploy to staging first, then prod. Never skip staging."},
                {"role": "assistant", "content": "Understood, staging before production always."},
            ],
            test_query="What's the deployment procedure?",
            correct_action="staging",
            incorrect_action="prod",
        ),
        # Contradiction handling
        ArenaScenario(
            scenario_id="ma-7",
            category="contradiction",
            setup_messages=[
                {"role": "user", "content": "We use PostgreSQL for our database"},
                {"role": "assistant", "content": "Got it, PostgreSQL."},
                {"role": "user", "content": "Actually we switched to CockroachDB last week"},
                {"role": "assistant", "content": "Updated, using CockroachDB now."},
            ],
            test_query="What database do we use?",
            correct_action="cockroachdb",
            incorrect_action="postgresql",
        ),
        ArenaScenario(
            scenario_id="ma-8",
            category="contradiction",
            setup_messages=[
                {"role": "user", "content": "The API is at api.example.com"},
                {"role": "assistant", "content": "Noted."},
                {"role": "user", "content": "We migrated the API to api.newdomain.io"},
                {"role": "assistant", "content": "Updated to api.newdomain.io."},
            ],
            test_query="What is the API endpoint?",
            correct_action="newdomain",
            incorrect_action="example.com",
        ),
    ]
