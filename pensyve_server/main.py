import os
import pensyve
from fastapi import FastAPI, HTTPException
from .models import (
    EntityCreate, EntityResponse,
    EpisodeStartRequest, EpisodeStartResponse,
    MessageRequest,
    EpisodeEndRequest, EpisodeEndResponse,
    RecallRequest, MemoryResponse,
    RememberRequest,
    ForgetResponse,
    ConsolidateResponse,
)

app = FastAPI(
    title="Pensyve API",
    description="Universal memory runtime for AI agents",
    version="0.1.0",
)

# Global Pensyve instance
_pensyve = None
_episodes = {}  # episode_id -> Episode object


def get_pensyve():
    global _pensyve
    if _pensyve is None:
        path = os.environ.get("PENSYVE_PATH", None)
        namespace = os.environ.get("PENSYVE_NAMESPACE", "default")
        _pensyve = pensyve.Pensyve(path=path, namespace=namespace)
    return _pensyve


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
    episode_id = str(id(ep))  # use object id as temp key
    _episodes[episode_id] = ep
    return EpisodeStartResponse(episode_id=episode_id)


@app.post("/v1/episodes/message")
def add_message(req: MessageRequest):
    ep = _episodes.get(req.episode_id)
    if not ep:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    ep.message(req.role, req.content)
    return {"status": "ok"}


@app.post("/v1/episodes/end", response_model=EpisodeEndResponse)
def end_episode(req: EpisodeEndRequest):
    ep = _episodes.pop(req.episode_id, None)
    if not ep:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    if req.outcome:
        ep.outcome(req.outcome)
    ep.__exit__(None, None, None)
    return EpisodeEndResponse(memories_created=1)  # approximate


@app.post("/v1/recall", response_model=list[MemoryResponse])
def recall(req: RecallRequest):
    p = get_pensyve()
    kwargs = {"limit": req.limit}
    if req.entity:
        kwargs["entity"] = p.entity(req.entity)
    if req.types:
        kwargs["types"] = req.types
    results = p.recall(req.query, **kwargs)
    return [
        MemoryResponse(
            id=m.id,
            content=m.content,
            memory_type=m.memory_type,
            confidence=m.confidence,
            stability=m.stability,
            score=getattr(m, "score", None),
        )
        for m in results
    ]


@app.post("/v1/remember", response_model=MemoryResponse)
def remember(req: RememberRequest):
    p = get_pensyve()
    entity = p.entity(req.entity)
    mem = p.remember(entity=entity, fact=req.fact, confidence=req.confidence)
    return MemoryResponse(
        id=mem.id,
        content=mem.content,
        memory_type=mem.memory_type,
        confidence=mem.confidence,
        stability=mem.stability,
    )


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


@app.get("/v1/health")
def health():
    return {"status": "ok", "version": "0.1.0"}
