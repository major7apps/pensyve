"""Pydantic request and response models for the Pensyve API."""

from typing import Any

from pydantic import BaseModel, ConfigDict, Field


class EntityCreate(BaseModel):
    """Request body for creating or retrieving a named entity."""

    model_config = ConfigDict(strict=True, extra="forbid")

    name: str = Field(min_length=1, max_length=10000)
    kind: str = "user"  # "agent", "user", "team", "tool"


class EntityResponse(BaseModel):
    """Response containing the entity's ID, name, and kind."""

    id: str
    name: str
    kind: str


class EpisodeStartRequest(BaseModel):
    """Request body for starting a new conversational episode."""

    model_config = ConfigDict(strict=True, extra="forbid")

    participants: list[str]  # entity names


class EpisodeStartResponse(BaseModel):
    """Response containing the newly created episode ID."""

    episode_id: str


class MessageRequest(BaseModel):
    """Request body for appending a message to an active episode."""

    model_config = ConfigDict(strict=True, extra="forbid")

    episode_id: str
    role: str
    content: str = Field(min_length=1, max_length=100000)


class EpisodeEndRequest(BaseModel):
    """Request body for closing an active episode with an optional outcome label."""

    model_config = ConfigDict(strict=True, extra="forbid")

    episode_id: str
    outcome: str | None = None  # "success", "failure", "partial"


class EpisodeEndResponse(BaseModel):
    """Response indicating how many memories were created when the episode closed."""

    memories_created: int


class RecallRequest(BaseModel):
    """Request body for searching memories by semantic query."""

    model_config = ConfigDict(strict=True, extra="forbid")

    query: str = Field(min_length=1, max_length=10000)
    entity: str | None = None
    limit: int = Field(default=5, ge=1, le=100)
    types: list[str] | None = None


class MemoryResponse(BaseModel):
    """A single memory record returned by recall or inspect."""

    id: str
    content: str
    memory_type: str
    confidence: float
    stability: float
    score: float | None = None


class RememberRequest(BaseModel):
    """Request body for storing a new memory for an entity."""

    model_config = ConfigDict(strict=True, extra="forbid")

    entity: str
    fact: str = Field(min_length=1, max_length=100000)
    confidence: float = 0.8


class RememberResponse(MemoryResponse):
    """Response for a stored memory, including which extraction tier was used."""

    extraction_tier: int = 1  # 1=patterns only, 2=local LLM, 3=API LLM


class ForgetResponse(BaseModel):
    """Response indicating how many memories were deleted."""

    forgotten_count: int


class ConsolidateResponse(BaseModel):
    """Response with counts of memories promoted, decayed, and archived."""

    promoted: int
    decayed: int
    archived: int


class StatsResponse(BaseModel):
    """Aggregate memory counts for a namespace, broken down by memory type."""

    namespace: str
    entities: int
    episodic_memories: int
    semantic_memories: int
    procedural_memories: int


class RecallResponse(BaseModel):
    """Search results including memories, contradiction warnings, and a pagination cursor."""

    memories: list[MemoryResponse]
    contradictions: list[dict[str, str]] = []
    cursor: str | None = None


class InspectRequest(BaseModel):
    """Request body for inspecting all memories belonging to a specific entity."""

    model_config = ConfigDict(strict=True, extra="forbid")

    entity: str
    limit: int = Field(default=50, ge=1, le=100)
    cursor: str | None = None


class InspectResponse(BaseModel):
    """Entity memories grouped by type with a cursor for pagination."""

    entity: str
    episodic: list[MemoryResponse] = []
    semantic: list[MemoryResponse] = []
    procedural: list[MemoryResponse] = []
    cursor: str | None = None


class ActivityResponse(BaseModel):
    """Daily activity summary with per-operation counts."""

    date: str
    recalls: int
    remembers: int
    forgets: int


class RecentEventResponse(BaseModel):
    """A single recent activity event with its type, content, and timestamp."""

    id: str
    type: str
    content: str
    timestamp: str


class FeedbackRequest(BaseModel):
    """Request body for submitting relevance feedback on a recalled memory."""

    model_config = ConfigDict(strict=True, extra="forbid")

    memory_id: str
    relevant: bool
    signals: list[float] | None = None  # Optional; server can look up from last recall


class GdprErasureResponse(BaseModel):
    """Response confirming completion of a GDPR Article 17 right-to-erasure request."""

    memories_deleted: int
    edges_deleted: int
    entities_deleted: int
    complete: bool
    warnings: list[str]


class A2ATaskRequest(BaseModel):
    """Request body for dispatching an A2A protocol task to a named capability."""

    model_config = ConfigDict(strict=True, extra="forbid")

    task_id: str
    capability: str
    input: dict[str, Any]
    from_agent: str


class A2ATaskResponse(BaseModel):
    """Response from an A2A task execution, including status and capability output."""

    task_id: str
    status: str
    output: dict[str, Any]
    error: str | None = None
