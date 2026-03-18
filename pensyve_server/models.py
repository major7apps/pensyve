from pydantic import BaseModel, Field
from typing import Optional


class EntityCreate(BaseModel):
    name: str
    kind: str = "user"  # "agent", "user", "team", "tool"


class EntityResponse(BaseModel):
    id: str
    name: str
    kind: str


class EpisodeStartRequest(BaseModel):
    participants: list[str]  # entity names


class EpisodeStartResponse(BaseModel):
    episode_id: str


class MessageRequest(BaseModel):
    episode_id: str
    role: str
    content: str


class EpisodeEndRequest(BaseModel):
    episode_id: str
    outcome: Optional[str] = None  # "success", "failure", "partial"


class EpisodeEndResponse(BaseModel):
    memories_created: int


class RecallRequest(BaseModel):
    query: str
    entity: Optional[str] = None
    limit: int = 5
    types: Optional[list[str]] = None


class MemoryResponse(BaseModel):
    id: str
    content: str
    memory_type: str
    confidence: float
    stability: float
    score: Optional[float] = None


class RememberRequest(BaseModel):
    entity: str
    fact: str
    confidence: float = 0.8


class ForgetRequest(BaseModel):
    entity: str
    hard_delete: bool = False


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
