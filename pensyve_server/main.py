import logging
import os
import time
import uuid

from fastapi import Depends, FastAPI, HTTPException
from fastapi.middleware.cors import CORSMiddleware

import pensyve

from .auth import require_api_key
from .models import (
    ConsolidateResponse,
    EntityCreate,
    EntityResponse,
    EpisodeEndRequest,
    EpisodeEndResponse,
    EpisodeStartRequest,
    EpisodeStartResponse,
    ForgetResponse,
    InspectRequest,
    InspectResponse,
    MemoryResponse,
    MessageRequest,
    RecallRequest,
    RecallResponse,
    RememberRequest,
    StatsResponse,
)

logger = logging.getLogger(__name__)

app = FastAPI(
    title="Pensyve API",
    description="Universal memory runtime for AI agents",
    version="0.1.0",
    dependencies=[Depends(require_api_key)],
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)

# Global Pensyve instance
_pensyve = None
_episodes: dict[str, dict] = {}  # episode_id -> {"ep": Episode, "message_count": int, "created_at": float}
_EPISODE_TTL_SECONDS = 1800  # 30 minutes

# Tier 2 extraction (gated by env var)
_tier2_enabled = os.environ.get("PENSYVE_TIER2_ENABLED", "false").lower() == "true"
_extractor = None
if _tier2_enabled:
    from pensyve_server.extraction import Tier2Extractor

    _extractor = Tier2Extractor()


def get_pensyve():
    global _pensyve
    if _pensyve is None:
        path = os.environ.get("PENSYVE_PATH", None)
        namespace = os.environ.get("PENSYVE_NAMESPACE", "default")
        _pensyve = pensyve.Pensyve(path=path, namespace=namespace)
    return _pensyve


def _sweep_stale_episodes():
    """Remove episodes older than TTL to prevent memory leaks."""
    now = time.time()
    stale = [eid for eid, e in _episodes.items() if now - e["created_at"] > _EPISODE_TTL_SECONDS]
    for eid in stale:
        entry = _episodes.pop(eid, None)
        if entry:
            try:
                entry["ep"].__exit__(None, None, None)
            except Exception:
                logger.warning("Failed to close stale episode %s", eid, exc_info=True)


def _memory_to_response(m) -> MemoryResponse:
    return MemoryResponse(
        id=m.id,
        content=m.content,
        memory_type=m.memory_type,
        confidence=m.confidence,
        stability=m.stability,
        score=getattr(m, "score", None),
    )


def _apply_cursor_pagination(
    memories: list[MemoryResponse], cursor: str | None, limit: int
) -> tuple[list[MemoryResponse], str | None]:
    """Apply cursor-based pagination. Cursor is a memory ID; returns items after it."""
    if cursor:
        # Find the cursor position and skip past it
        found = False
        filtered = []
        for m in memories:
            if found:
                filtered.append(m)
            elif m.id == cursor:
                found = True
        memories = filtered

    # Apply limit + 1 to detect if there are more results
    if len(memories) > limit:
        next_cursor = memories[limit - 1].id
        memories = memories[:limit]
    else:
        next_cursor = None

    return memories, next_cursor


@app.post("/v1/entities", response_model=EntityResponse)
def create_entity(req: EntityCreate):
    p = get_pensyve()
    entity = p.entity(req.name, kind=req.kind)
    return EntityResponse(id=entity.id, name=entity.name, kind=entity.kind)


@app.post("/v1/episodes/start", response_model=EpisodeStartResponse)
def start_episode(req: EpisodeStartRequest):
    p = get_pensyve()
    entities = [p.entity(name) for name in req.participants]
    ep = p.episode(*entities)
    ep.__enter__()
    episode_id = str(uuid.uuid4())
    _episodes[episode_id] = {"ep": ep, "message_count": 0, "created_at": time.time()}
    # Sweep stale episodes
    _sweep_stale_episodes()
    return EpisodeStartResponse(episode_id=episode_id)


@app.post("/v1/episodes/message")
def add_message(req: MessageRequest):
    entry = _episodes.get(req.episode_id)
    if not entry:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    entry["ep"].message(req.role, req.content)
    entry["message_count"] += 1
    return {"status": "ok"}


@app.post("/v1/episodes/end", response_model=EpisodeEndResponse)
def end_episode(req: EpisodeEndRequest):
    entry = _episodes.pop(req.episode_id, None)
    if not entry:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    ep = entry["ep"]
    if req.outcome:
        ep.outcome(req.outcome)
    ep.__exit__(None, None, None)
    return EpisodeEndResponse(memories_created=entry["message_count"])


@app.post("/v1/recall", response_model=RecallResponse)
def recall(req: RecallRequest, cursor: str | None = None):
    p = get_pensyve()
    # Fetch extra to support pagination
    fetch_limit = req.limit + 50  # overfetch for cursor slicing
    kwargs: dict[str, object] = {"limit": fetch_limit}
    if req.entity:
        kwargs["entity"] = p.entity(req.entity)
    if req.types:
        kwargs["types"] = req.types
    results = p.recall(req.query, **kwargs)
    memories = [_memory_to_response(m) for m in results]

    # Apply cursor-based pagination
    memories, next_cursor = _apply_cursor_pagination(memories, cursor, req.limit)

    # Tier 2 contradiction detection
    contradictions: list[dict[str, str]] = []
    if _tier2_enabled and _extractor and memories:
        try:
            existing_facts = [
                {"subject": "", "predicate": "", "object": m.content}
                for m in memories
                if m.memory_type == "semantic"
            ]
            if existing_facts:
                contradictions = _extractor.detect_contradictions(req.query, existing_facts)
        except Exception:
            logger.warning("Tier 2 contradiction detection failed", exc_info=True)

    return RecallResponse(
        memories=memories,
        contradictions=contradictions,
        cursor=next_cursor,
    )


@app.post("/v1/remember", response_model=MemoryResponse)
def remember(req: RememberRequest):
    p = get_pensyve()
    entity = p.entity(req.entity)
    mem = p.remember(entity=entity, fact=req.fact, confidence=req.confidence)

    # Tier 2 fact extraction
    if _tier2_enabled and _extractor:
        try:
            extracted = _extractor.extract_facts(req.fact)
            for fact in extracted:
                fact_text = f"{fact.subject} {fact.predicate} {fact.object}"
                p.remember(entity=entity, fact=fact_text, confidence=fact.confidence)
        except Exception:
            logger.warning("Tier 2 fact extraction failed", exc_info=True)

    return _memory_to_response(mem)


@app.delete("/v1/entities/{entity_name}", response_model=ForgetResponse)
def forget(entity_name: str, hard_delete: bool = False):
    p = get_pensyve()
    entity = p.entity(entity_name)
    result = p.forget(entity=entity, hard_delete=hard_delete)
    return ForgetResponse(forgotten_count=result["forgotten_count"])


@app.post("/v1/consolidate", response_model=ConsolidateResponse)
def consolidate():
    p = get_pensyve()
    result = p.consolidate()
    return ConsolidateResponse(
        promoted=result.get("promoted", 0),
        decayed=result.get("decayed", 0),
        archived=result.get("archived", 0),
    )


@app.get("/v1/stats", response_model=StatsResponse)
def get_stats():
    p = get_pensyve()
    namespace = os.environ.get("PENSYVE_NAMESPACE", "default")

    # Single broad recall, then group client-side
    all_memories = p.recall("*", limit=1000)
    episodic_count = sum(1 for m in all_memories if m.memory_type == "episodic")
    semantic_count = sum(1 for m in all_memories if m.memory_type == "semantic")
    procedural_count = sum(1 for m in all_memories if m.memory_type == "procedural")

    return StatsResponse(
        namespace=namespace,
        entities=0,  # Not available via SDK — requires storage-level count query
        episodic_memories=episodic_count,
        semantic_memories=semantic_count,
        procedural_memories=procedural_count,
    )


@app.post("/v1/inspect", response_model=InspectResponse)
def inspect(req: InspectRequest):
    p = get_pensyve()
    entity = p.entity(req.entity)

    # Fetch a large batch and group by type
    fetch_limit = req.limit + 50
    results = p.recall("*", entity=entity, limit=fetch_limit)
    all_memories = [_memory_to_response(m) for m in results]

    # Apply cursor-based pagination on the full set first
    all_memories, next_cursor = _apply_cursor_pagination(all_memories, req.cursor, req.limit)

    # Group by type
    episodic = [m for m in all_memories if m.memory_type == "episodic"]
    semantic = [m for m in all_memories if m.memory_type == "semantic"]
    procedural = [m for m in all_memories if m.memory_type == "procedural"]

    return InspectResponse(
        entity=req.entity,
        episodic=episodic,
        semantic=semantic,
        procedural=procedural,
        cursor=next_cursor,
    )


@app.get("/v1/health")
def health():
    return {"status": "ok", "version": "0.1.0"}
