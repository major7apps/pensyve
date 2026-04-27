"""Type stubs for pensyve._core (PyO3 extension module)."""

from __future__ import annotations

from typing import Literal

__version__: str

def embedding_info() -> tuple[str, int]:
    """Return (model_name, dimensions) for the active embedding model."""
    ...

class Pensyve:
    """Main entry point for the Pensyve memory runtime."""

    def __init__(
        self,
        path: str | None = None,
        namespace: str | None = None,
        extractor: str | None = None,
        extractor_api_key: str | None = None,
        reranker: str | None = "BGERerankerBase",
    ) -> None:
        """Create or open a Pensyve instance.

        Args:
            path: Directory for storage files (default: ~/.pensyve/default).
            namespace: Namespace name (default: "default").
            extractor: Optional observation extractor.
                - `"haiku"` wires the Anthropic Haiku 4.5 extractor
                  (requires `ANTHROPIC_API_KEY` env var unless
                  `extractor_api_key` is provided).
                - `"local-vllm"` wires an OpenAI-compatible local-LLM
                  extractor for offline-first deployments (reads
                  `PENSYVE_LOCAL_LLM_URL`, `PENSYVE_LOCAL_LLM_MODEL`,
                  `PENSYVE_LOCAL_LLM_API_KEY` from the environment; defaults
                  to `http://localhost:8888/v1`, model `"local"`, no auth).
                - `None` (default) skips extraction entirely.
            extractor_api_key: Explicit API key for the configured extractor.
                Overrides `ANTHROPIC_API_KEY` (haiku) or
                `PENSYVE_LOCAL_LLM_API_KEY` (local-vllm).
            reranker: Cross-encoder reranker applied post-fusion in `recall`
                and `recall_grouped`. Default `"BGERerankerBase"` — the
                algorithm-spec default. Pass `None` to disable (skips the
                ~150MB fastembed model download, but this is a weaker
                algorithm than the spec describes). `"JINARerankerV1TurboEn"`
                is also supported as an English-only alternative.
        """
        ...

    def entity(self, name: str, kind: str = "user") -> Entity:
        """Get or create an entity.

        Args:
            name: Entity name.
            kind: One of "agent", "user", "team", "tool" (default: "user").
        """
        ...

    def episode(self, *participants: Entity) -> Episode:
        """Create an episode context manager.

        Args:
            *participants: Entity objects participating in this episode.
        """
        ...

    def recall(
        self,
        query: str,
        entity: Entity | None = None,
        limit: int = 5,
        types: list[str] | None = None,
    ) -> list[Memory]:
        """Recall memories matching a query.

        Applies the full Pensyve retrieval pipeline: vector search + RRF
        fusion + graph traversal + cross-encoder reranking. Graph is built
        fresh per-call from storage (O(entities + edges), sub-ms for
        typical namespaces). Reranker is applied when configured in
        ``__init__`` (default: ``BGERerankerBase``).

        Args:
            query: Search query string.
            entity: Optional entity to filter by.
            limit: Maximum number of results (default: 5).
            types: Optional list of memory type strings to filter by.
        """
        ...

    def recall_grouped(
        self,
        query: str,
        *,
        limit: int = 50,
        order: Literal["chronological", "relevance"] = "chronological",
        max_groups: int | None = None,
    ) -> list[SessionGroup]:
        """Recall memories matching a query, clustered by source session.

        Runs the full Pensyve retrieval pipeline (vector + RRF + graph +
        reranker) and then groups the top-``limit`` results by
        ``episode_id``. Memories from the same session cluster into a
        single :class:`SessionGroup` sorted by event time within the group.
        Semantic and procedural memories (which have no episode) appear as
        singleton groups with ``session_id=None``.

        Args:
            query: Search query string.
            limit: Maximum number of memories to consider across all groups
                (default: 50).
            order: "chronological" (default, oldest session first) or
                "relevance" (highest-scoring session first).
            max_groups: Optional cap on the number of groups returned.

        Raises:
            ValueError: If ``order`` is not one of the supported values.
        """
        ...

    def remember(
        self,
        entity: Entity,
        fact: str,
        confidence: float = 0.8,
    ) -> Memory:
        """Store an explicit semantic memory.

        Args:
            entity: The entity this fact is about.
            fact: The fact to remember.
            confidence: Confidence level in [0, 1] (default: 0.8).
        """
        ...

    def forget(
        self,
        entity: Entity,
        hard_delete: bool = False,
    ) -> dict[str, int]:
        """Archive or delete all memories about an entity.

        Args:
            entity: The entity whose memories to forget.
            hard_delete: If True, permanently delete (default: False).
        """
        ...

    def stats(self) -> dict[str, int]:
        """Return aggregate memory counts.

        Returns:
            Dict with keys: entities, episodic, semantic, procedural.
        """
        ...

    def consolidate(self) -> dict[str, int]:
        """Run consolidation (episodic->semantic promotion, FSRS decay, archival).

        Returns:
            Dict with keys: promoted, decayed, archived (counts).
        """
        ...

class Entity:
    """Represents an entity (agent, user, team, or tool)."""

    @property
    def id(self) -> str:
        """UUID of this entity as a string."""
        ...

    @property
    def name(self) -> str:
        """Name of this entity."""
        ...

    @property
    def kind(self) -> str:
        """Kind of this entity: 'agent', 'user', 'team', or 'tool'."""
        ...

class Episode:
    """An episode context manager that records messages and creates memories on exit."""

    def message(
        self,
        role: str,
        content: str,
        when: str | None = None,
    ) -> None:
        """Record a message in this episode.

        Args:
            role: The role of the speaker (e.g. "user", "assistant").
            content: The message content.
            when: Optional RFC3339 / ISO 8601 timestamp describing when the
                event in this message occurred (e.g. "2023-03-04T08:09:00Z").
                Defaults to the current UTC time at episode commit. Pass an
                explicit value when ingesting historical or backfilled data
                where the real-world event time differs from the encoding
                time. Raises `ValueError` if the string is not parseable.
        """
        ...

    def outcome(self, result: str) -> None:
        """Set the episode outcome.

        Args:
            result: One of "success", "failure", "partial".
        """
        ...

    def __enter__(self) -> Episode: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: object | None,
    ) -> bool: ...

class Memory:
    """Represents a retrieved memory."""

    @property
    def id(self) -> str:
        """UUID of this memory as a string."""
        ...

    @property
    def content(self) -> str:
        """Content text of this memory."""
        ...

    @property
    def memory_type(self) -> str:
        """Type of this memory: 'episodic', 'semantic', 'procedural', or 'observation'."""
        ...

    @property
    def confidence(self) -> float:
        """Confidence level in [0, 1]."""
        ...

    @property
    def stability(self) -> float:
        """Stability level in [0, 1]."""
        ...

    @property
    def score(self) -> float:
        """Retrieval score from the recall engine."""
        ...

    @property
    def salience(self) -> float | None:
        """Salience at encoding time [0, 1]. Only set for episodic memories."""
        ...

    @property
    def storage_strength(self) -> float | None:
        """Storage strength (monotonically increases). Only set for episodic memories."""
        ...

    @property
    def event_time(self) -> str | None:
        """When the described event occurred (ISO 8601). Set for episodic and
        observation memories; None for semantic / procedural."""
        ...

    @property
    def superseded_by(self) -> str | None:
        """ID of the memory that superseded this one, if any. Only set for episodic memories."""
        ...

    @property
    def entity_type(self) -> str | None:
        """Observation category, e.g. 'game_played'. Only set when memory_type == 'observation'."""
        ...

    @property
    def instance(self) -> str | None:
        """Specific instance named by the observation. Only set for observations."""
        ...

    @property
    def action(self) -> str | None:
        """User action for the observation, e.g. 'played'. Only set for observations."""
        ...

    @property
    def quantity(self) -> float | None:
        """Numeric quantity when the observation recorded one. Only set for observations."""
        ...

    @property
    def unit(self) -> str | None:
        """Unit paired with `quantity`, e.g. 'hours'. Only set for observations."""
        ...

    @property
    def episode_id(self) -> str | None:
        """Source episode for the observation. Only set for observations."""
        ...

class SessionGroup:
    """A cluster of recalled memories sharing a source conversation session.

    Returned by :meth:`Pensyve.recall_grouped`. Memories from the same
    episode are clustered into one group, sorted by event time within the
    group. Semantic and procedural memories surface as singleton groups
    with ``session_id=None``.
    """

    @property
    def session_id(self) -> str | None:
        """Episode UUID as a string, or ``None`` for semantic / procedural memories."""
        ...

    @property
    def session_time(self) -> str:
        """Representative timestamp (ISO 8601 / RFC 3339). Earliest event time in the group."""
        ...

    @property
    def memories(self) -> list[Memory]:
        """Memories in conversation order (sorted by event time ascending)."""
        ...

    @property
    def group_score(self) -> float:
        """Max RRF score across the group's memories."""
        ...

    def __len__(self) -> int: ...
