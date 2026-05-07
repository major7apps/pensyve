"""Type stubs for pensyve._core (PyO3 extension module)."""

from __future__ import annotations

from typing import Any, Literal

__version__: str

def embedding_info() -> tuple[str, int]:
    """Return (model_name, dimensions) for the active embedding model."""
    ...

class HaikuExtractionCache:
    """Opaque prewarmed observation cache for `extractor="haiku-cached"`.

    Built by `prewarm_haiku_extraction_cache(...)`. Carries one cache entry
    per unique episode-message fingerprint submitted to the prewarm pass.
    """

    def __len__(self) -> int: ...
    def size(self) -> int:
        """Number of cached entries (alias for `__len__`)."""
        ...

def prewarm_haiku_extraction_cache(
    messages_groups: list[list[dict[str, Any]]],
    api_key: str | None = None,
    poll_interval_secs: int = 30,
    max_wait_secs: int = 7200,
) -> HaikuExtractionCache:
    """Submit every episode in one Anthropic Messages Batches call.

    Returns a populated `HaikuExtractionCache` keyed by content fingerprint.
    Pass the result as `Pensyve(extractor="haiku-cached", extractor_cache=...)`
    to drive Pensyve's per-question ingest path off the cache without
    further per-episode HTTP traffic.

    Args:
        messages_groups: One list per episode. Each inner list contains
            dicts with `role` (str), `content` (str), and optional
            `event_time` (RFC3339 string). The same shape Pensyve will
            hand to the extractor at live ingest time, so both sides
            agree on the fingerprint.
        api_key: Overrides `ANTHROPIC_API_KEY` env var when provided.
        poll_interval_secs: Status-poll cadence (default 30s).
        max_wait_secs: Ceiling for the batch to settle (default 7200 = 2h).
    """
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
        extractor_base_url: str | None = None,
        extractor_model: str | None = None,
        extractor_max_concurrency: int | None = None,
        agent_id: str | None = None,
        user_id: str | None = None,
        k_budget: dict[str, int] | None = None,
        ms_card_days: int | None = None,
    ) -> None:
        """Create or open a Pensyve instance.

        Args:
            path: Directory for storage files (default: ~/.pensyve/default).
            namespace: Namespace name (default: "default").
            extractor: Optional observation extractor. Supported values:
                - `"local-llm"` / `"local-vllm"`: OpenAI-compatible local
                  backend; offline-first.
                - `"batched-local-llm"`: same inner extractor wrapped in
                  a semaphore-gated batch path; activates via
                  `flush_extractions()`.
                - `None` (default) skips extraction entirely.
            extractor_api_key: Explicit API key for the configured extractor.
            reranker: Cross-encoder reranker applied post-fusion. Default
                `"BGERerankerBase"`. Pass `None` to disable.
            extractor_base_url: Override for the local-LLM endpoint
                (precedes `PENSYVE_EXTRACTOR_URL`).
            extractor_model: Override for the local-LLM model id (precedes
                `PENSYVE_EXTRACTOR_MODEL`).
            extractor_max_concurrency: In-flight ceiling for
                `extractor="batched-local-llm"`.
            agent_id: G1 multi-tenant scope — UUID-shaped string.
            user_id: G1 multi-tenant scope — UUID-shaped string.
            k_budget: G4 retrieval-side k-budget per `question_type`
                family. Dict shape:
                `{"ss_pref": int, "ms": int, "ssu": int}`. Missing keys
                fall back to the locked defaults
                `{"ss_pref": 22, "ms": 50, "ssu": 12}`. Precedence:
                kwarg > `PENSYVE_K_BUDGET_*` env > default. Pre-reg lock
                at `pensyve-docs@8930c4a`.
            ms_card_days: G4 MS-card-v2 cross-session day threshold.
                Default `2`. Precedence: kwarg > `PENSYVE_MS_CARD_DAYS`
                env > default. Pre-reg lock at `pensyve-docs@8930c4a`.
        """
        ...

    @property
    def k_budget(self) -> dict[str, int]:
        """Resolved k-budget per `question_type` family.

        Returns a dict with keys ``ss_pref``, ``ms``, ``ssu``. Reflects
        the kwarg > env > default precedence locked at
        ``pensyve-docs@8930c4a``.
        """
        ...

    @property
    def ms_card_days(self) -> int:
        """Resolved MS-card-v2 cross-session day threshold.

        Reflects the kwarg > env > default precedence locked at
        ``pensyve-docs@8930c4a`` (default = 2).
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
        types: list[str] | None = None,
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
            types: Optional list of memory type strings to filter the
                candidate pool *before* grouping. Mirrors the equivalent
                kwarg on :meth:`recall`. Default ``None`` (no filter).

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

    def build_retrieval_card_g3(
        self,
        db_path: str,
        question_type: str,
        g2_cards: list[str],
        g3_features: list[str],
    ) -> str | None:
        """G3 retrieval-card composition (binding pre-reg
        ``pensyve-docs@64481dc`` §3.4 item 11 + §7 item 11).

        Builds the G3 ``CompositeCard`` against an external SQLite store
        and returns the synthesized card text (English prose, possibly
        multi-section joined with ``\\n\\n``), or ``None`` when every
        selected card defers.

        Args:
            db_path: Path to a Pensyve SQLite store. May be the directory
                containing ``memories.db`` OR the file itself; both shapes
                are normalized to the directory before opening.
            question_type: LongMemEval question_type string (e.g.
                ``"single-session-preference"``, ``"multi-session"``).
                Threaded into each card's ``build()`` call.
            g2_cards: G2 base composition; subset of
                ``["peer", "ms", "ssu"]``. Order does not matter (the G2
                priority order is fixed).
            g3_features: G3 layering knobs; subset of
                ``["router", "summarizer", "typed_slots", "diversity"]``.
                Translated to the ``PENSYVE_RETRIEVAL_CARDS_G3`` env-var
                value: ``[]`` → unset (G2-equivalent baseline), single
                feature → that feature's name, all four → ``"full"``.
                ``"summarizer"`` additionally pulls ``SupersessionCard``
                into the composite chain.
        """
        ...

    def recall_with_diversity(
        self,
        query: str,
        k: int = 22,
        lambda_: float = 0.5,
    ) -> list[Memory]:
        """Recall with MMR diversity reorder (binding pre-reg
        ``pensyve-docs@64481dc`` §3.4 item 11 + §7 item 11).

        Passes ``lambda_`` directly to
        :py:meth:`RecallEngine.with_mmr_lambda` so the diversity reorder
        activates without process-env mutation (round-4 fix).
        Behaviorally identical to :meth:`recall` when ``lambda_ <= 0.0``;
        reorders by ``lambda_·sim − (1−lambda_)·max_j sim`` otherwise.

        Args:
            query: Search query string.
            k: Maximum number of results (default: 22).
            lambda_: MMR balance, clamped to ``[0.0, 1.0]`` by the engine.
                The Python kwarg uses a trailing underscore because
                ``lambda`` is a reserved word.
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
