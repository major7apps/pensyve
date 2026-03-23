import asyncio
import os
import time
import uuid
from collections import Counter
from contextlib import asynccontextmanager

import structlog
from fastapi import Depends, FastAPI, HTTPException
from fastapi.middleware.cors import CORSMiddleware

import pensyve

from .activity import ActivityTracker
from .auth import require_api_key
from .billing import UsageTracker
from .logging import RequestIdMiddleware, configure_logging
from .metrics import MetricsMiddleware
from .metrics import router as metrics_router
from .models import (
    A2ATaskRequest,
    A2ATaskResponse,
    ActivityResponse,
    ConsolidateResponse,
    EntityCreate,
    EntityResponse,
    EpisodeEndRequest,
    EpisodeEndResponse,
    EpisodeStartRequest,
    EpisodeStartResponse,
    FeedbackRequest,
    ForgetResponse,
    GdprErasureResponse,
    InspectRequest,
    InspectResponse,
    MemoryResponse,
    MessageRequest,
    RecallRequest,
    RecallResponse,
    RecentEventResponse,
    RememberRequest,
    RememberResponse,
    StatsResponse,
)
from .rate_limit import rate_limit_check
from .rbac import require_role
from .redis_client import close_redis

configure_logging()
logger = structlog.get_logger()


@asynccontextmanager
async def lifespan(app: FastAPI):
    db_dir = os.path.join(os.path.expanduser("~"), ".pensyve", _get_namespace())
    db_path = os.path.join(db_dir, "memories.db")
    _activity.set_db_path(db_path)

    async def _flush_loop():
        while True:
            await asyncio.sleep(30)
            try:
                count = await _activity.flush()
                if count > 0:
                    logger.info("activity_flush", count=count)
            except Exception:
                logger.exception("activity_flush_error")
            try:
                _sweep_stale_episodes()
            except Exception:
                logger.exception("sweep_stale_episodes_error")

    task = asyncio.create_task(_flush_loop())
    yield
    task.cancel()
    await close_redis()


app = FastAPI(
    title="Pensyve API",
    description="Universal memory runtime for AI agents",
    version="0.1.0",
    lifespan=lifespan,
    dependencies=[Depends(require_api_key), Depends(rate_limit_check)],
)

_allowed_origins = os.environ.get("PENSYVE_CORS_ORIGINS", "http://localhost:3000").split(",")

app.add_middleware(
    CORSMiddleware,
    allow_origins=_allowed_origins,
    allow_methods=["*"],
    allow_headers=["*"],
)
app.add_middleware(RequestIdMiddleware)
app.add_middleware(MetricsMiddleware)
app.include_router(metrics_router)

_pensyve = None
_episodes: dict[
    str, dict
] = {}  # episode_id -> {"ep": Episode, "message_count": int, "created_at": float}
_EPISODE_TTL_SECONDS = 1800  # 30 minutes

# Tier 2 extraction (gated by env var)
_tier2_enabled = os.environ.get("PENSYVE_TIER2_ENABLED", "false").lower() == "true"
_extractor = None
if _tier2_enabled:
    from pensyve_server.extraction import Tier2Extractor

    _extractor = Tier2Extractor()

_usage_tracker = UsageTracker()
_activity = ActivityTracker()


def _get_namespace() -> str:
    return os.environ.get("PENSYVE_NAMESPACE", "default")


def get_pensyve():
    global _pensyve
    if _pensyve is None:
        path = os.environ.get("PENSYVE_PATH", None)
        _pensyve = pensyve.Pensyve(path=path, namespace=_get_namespace())
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
    entity = p.entity(req.entity) if req.entity else None
    results = p.recall(
        req.query,
        entity=entity,
        limit=fetch_limit,
        types=req.types or None,
    )
    _usage_tracker.record_recall(_get_namespace())
    _activity.record("recall", req.query)
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


@app.post("/v1/feedback")
def submit_feedback(req: FeedbackRequest):
    """Record user feedback on a recalled memory to improve retrieval weights."""
    # For now, log the feedback. Full weight learning requires the Rust WeightLearner
    # to be exposed via PyO3 (future work).
    logger.info("Feedback: memory=%s relevant=%s", req.memory_id, req.relevant)
    _activity.record("feedback", f"memory={req.memory_id} relevant={req.relevant}")
    return {"status": "recorded"}


@app.post(
    "/v1/remember", response_model=RememberResponse, dependencies=[Depends(require_role("writer"))]
)
def remember(req: RememberRequest):
    p = get_pensyve()
    entity = p.entity(req.entity)
    mem = p.remember(entity=entity, fact=req.fact, confidence=req.confidence)

    extraction_tier = 1

    # Tier 2 fact extraction
    if _tier2_enabled and _extractor:
        try:
            extracted = _extractor.extract_facts(req.fact)
            for fact in extracted:
                fact_text = f"{fact.subject} {fact.predicate} {fact.object}"
                p.remember(entity=entity, fact=fact_text, confidence=fact.confidence)
            extraction_tier = 2
        except Exception:
            logger.warning("tier2_extraction_failed", exc_info=True)

    _usage_tracker.record_store(_get_namespace())
    _activity.record("remember", req.fact[:100])
    base = _memory_to_response(mem)
    return RememberResponse(
        id=base.id,
        content=base.content,
        memory_type=base.memory_type,
        confidence=base.confidence,
        stability=base.stability,
        score=base.score,
        extraction_tier=extraction_tier,
    )


@app.delete(
    "/v1/entities/{entity_name}",
    response_model=ForgetResponse,
    dependencies=[Depends(require_role("writer"))],
)
def forget(entity_name: str, hard_delete: bool = False):
    p = get_pensyve()
    entity = p.entity(entity_name)
    result = p.forget(entity=entity, hard_delete=hard_delete)
    _activity.record("forget", entity_name)
    return ForgetResponse(forgotten_count=result["forgotten_count"])


@app.delete(
    "/v1/gdpr/erase/{entity_name}",
    response_model=GdprErasureResponse,
    dependencies=[Depends(require_role("owner"))],
)
def gdpr_erase(entity_name: str):
    """GDPR Article 17: Right to erasure. Cascading delete of all entity data."""
    p = get_pensyve()
    entity = p.entity(entity_name)
    result = p.forget(entity=entity, hard_delete=True)
    _activity.record("gdpr_erasure", f"entity={entity_name}")
    return GdprErasureResponse(
        memories_deleted=result.get("forgotten_count", 0),
        edges_deleted=0,
        entities_deleted=1,
        complete=True,
        warnings=[],
    )


@app.post(
    "/v1/consolidate",
    response_model=ConsolidateResponse,
    dependencies=[Depends(require_role("owner"))],
)
def consolidate():
    p = get_pensyve()
    result = p.consolidate()
    _activity.record("consolidate", f"promoted={result.get('promoted', 0)}")
    return ConsolidateResponse(
        promoted=result.get("promoted", 0),
        decayed=result.get("decayed", 0),
        archived=result.get("archived", 0),
    )


@app.get("/v1/stats", response_model=StatsResponse)
def get_stats():
    p = get_pensyve()

    # TODO: Replace with direct storage-level count query when Pensyve.stats()
    # is exposed via PyO3. Current approach runs a full recall pipeline which is
    # expensive and caps at 10_000 results, so counts may be approximate.
    all_memories = p.recall("*", limit=10_000)
    type_counts = Counter(m.memory_type for m in all_memories)

    return StatsResponse(
        namespace=_get_namespace(),
        entities=0,  # Not available via SDK — requires storage-level count query
        episodic_memories=type_counts.get("episodic", 0),
        semantic_memories=type_counts.get("semantic", 0),
        procedural_memories=type_counts.get("procedural", 0),
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


@app.get("/v1/usage")
def get_usage():
    namespace = _get_namespace()
    usage = _usage_tracker.get_usage(namespace)
    return {
        "namespace": namespace,
        "api_calls": usage.api_calls,
        "recalls": usage.recalls,
        "memories_stored": usage.memories_stored,
    }


@app.get("/v1/activity", response_model=list[ActivityResponse])
def get_activity(days: int = 30):
    return _activity.daily_summary(days)


@app.get("/v1/activity/recent", response_model=list[RecentEventResponse])
def get_recent_activity(limit: int = 10):
    events = _activity.recent(limit)
    return [
        RecentEventResponse(id=e.id, type=e.event_type, content=e.content, timestamp=e.timestamp)
        for e in events
    ]


@app.get("/v1/a2a/agent-card")
def a2a_agent_card():
    """Return the A2A agent card describing Pensyve's capabilities."""
    base_url = os.environ.get("PENSYVE_BASE_URL", "http://localhost:8000")
    return {
        "name": "pensyve-memory",
        "description": "Universal memory runtime for AI agents",
        "protocol": "a2a/v1",
        "capabilities": [
            {"id": "memory.recall", "description": "Query memories by semantic similarity"},
            {"id": "memory.remember", "description": "Store a new memory"},
            {"id": "memory.forget", "description": "Delete memories for an entity"},
        ],
        "endpoint": base_url,
        "auth": {"auth_type": "api_key", "header": "X-Pensyve-Key"},
    }


@app.post(
    "/v1/a2a/task", response_model=A2ATaskResponse, dependencies=[Depends(require_role("writer"))]
)
def a2a_task(req: A2ATaskRequest):
    """Handle an A2A task request by routing to the appropriate capability."""
    p = get_pensyve()
    _activity.record("a2a_task", f"capability={req.capability} from={req.from_agent}")

    try:
        if req.capability == "memory.recall":
            query = req.input.get("query", "")
            limit = req.input.get("limit", 5)
            entity_name = req.input.get("entity")
            entity = p.entity(entity_name) if entity_name else None
            results = p.recall(query, entity=entity, limit=limit)
            memories = [{"content": m.content, "score": getattr(m, "score", 0)} for m in results]
            return A2ATaskResponse(
                task_id=req.task_id, status="completed", output={"memories": memories}
            )

        elif req.capability == "memory.remember":
            entity_name = req.input["entity"]
            fact = req.input["fact"]
            confidence = req.input.get("confidence", 0.8)
            entity = p.entity(entity_name)
            mem = p.remember(entity=entity, fact=fact, confidence=confidence)
            return A2ATaskResponse(
                task_id=req.task_id, status="completed", output={"memory_id": mem.id}
            )

        elif req.capability == "memory.forget":
            entity_name = req.input["entity"]
            entity = p.entity(entity_name)
            result = p.forget(entity=entity)
            return A2ATaskResponse(
                task_id=req.task_id,
                status="completed",
                output={"forgotten_count": result["forgotten_count"]},
            )

        else:
            return A2ATaskResponse(
                task_id=req.task_id,
                status="failed",
                output={},
                error=f"Unknown capability: {req.capability}",
            )

    except Exception as e:
        return A2ATaskResponse(task_id=req.task_id, status="failed", output={}, error=str(e))


@app.get("/v1/health")
def health():
    return {"status": "ok", "version": "0.1.0"}
