from typing import Any

from pydantic import BaseModel, ConfigDict, Field


class EntityCreate(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    name: str = Field(min_length=1, max_length=10000)
    kind: str = "user"  # "agent", "user", "team", "tool"


class EntityResponse(BaseModel):
    id: str
    name: str
    kind: str


class EpisodeStartRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    participants: list[str]  # entity names


class EpisodeStartResponse(BaseModel):
    episode_id: str


class MessageRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    episode_id: str
    role: str
    content: str = Field(min_length=1, max_length=100000)


class EpisodeEndRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    episode_id: str
    outcome: str | None = None  # "success", "failure", "partial"


class EpisodeEndResponse(BaseModel):
    memories_created: int


class RecallRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    query: str = Field(min_length=1, max_length=10000)
    entity: str | None = None
    limit: int = Field(default=5, ge=1, le=100)
    types: list[str] | None = None


class MemoryResponse(BaseModel):
    id: str
    content: str
    memory_type: str
    confidence: float
    stability: float
    score: float | None = None


class RememberRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    entity: str
    fact: str = Field(min_length=1, max_length=100000)
    confidence: float = 0.8


class RememberResponse(MemoryResponse):
    extraction_tier: int = 1  # 1=patterns only, 2=local LLM, 3=API LLM


class ForgetResponse(BaseModel):
    forgotten_count: int


class ConsolidateResponse(BaseModel):
    promoted: int
    decayed: int
    archived: int


class StatsResponse(BaseModel):
    namespace: str
    entities: int
    episodic_memories: int
    semantic_memories: int
    procedural_memories: int


class RecallResponse(BaseModel):
    memories: list[MemoryResponse]
    contradictions: list[dict[str, str]] = []
    cursor: str | None = None


class InspectRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    entity: str
    limit: int = Field(default=50, ge=1, le=100)
    cursor: str | None = None


class InspectResponse(BaseModel):
    entity: str
    episodic: list[MemoryResponse] = []
    semantic: list[MemoryResponse] = []
    procedural: list[MemoryResponse] = []
    cursor: str | None = None


class ActivityResponse(BaseModel):
    date: str
    recalls: int
    remembers: int
    forgets: int


class RecentEventResponse(BaseModel):
    id: str
    type: str
    content: str
    timestamp: str


class FeedbackRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    memory_id: str
    relevant: bool
    signals: list[float] | None = None  # Optional; server can look up from last recall


class GdprErasureResponse(BaseModel):
    memories_deleted: int
    edges_deleted: int
    entities_deleted: int
    complete: bool
    warnings: list[str]


class A2ATaskRequest(BaseModel):
    model_config = ConfigDict(strict=True, extra="forbid")

    task_id: str
    capability: str
    input: dict[str, Any]
    from_agent: str


class A2ATaskResponse(BaseModel):
    task_id: str
    status: str
    output: dict[str, Any]
    error: str | None = None
