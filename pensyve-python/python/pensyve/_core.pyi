"""Type stubs for pensyve._core (PyO3 extension module)."""

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
    ) -> None:
        """Create or open a Pensyve instance.

        Args:
            path: Directory for storage files (default: ~/.pensyve/default).
            namespace: Namespace name (default: "default").
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

        Args:
            query: Search query string.
            entity: Optional entity to filter by.
            limit: Maximum number of results (default: 5).
            types: Optional list of memory type strings to filter by.
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

    def message(self, role: str, content: str) -> None:
        """Record a message in this episode.

        Args:
            role: The role of the speaker (e.g. "user", "assistant").
            content: The message content.
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
        """Type of this memory: 'episodic', 'semantic', or 'procedural'."""
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
        """When the described event occurred (ISO 8601). Only set for episodic memories."""
        ...

    @property
    def superseded_by(self) -> str | None:
        """ID of the memory that superseded this one, if any. Only set for episodic memories."""
        ...
