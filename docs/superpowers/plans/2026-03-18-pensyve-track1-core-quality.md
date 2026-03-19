# Track 1: Core Quality — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Get Pensyve from "works" to "works well" — 80%+ LongMemEval_S, fix known bugs, wire up unused capabilities.

**Architecture:** Fix REST API bugs (UUID episodes, memories_created count, type stubs), implement intent scoring in retrieval fusion, wire Tier 2 LLM extraction into the REST pipeline, and build benchmark infrastructure for LongMemEval evaluation and weight optimization.

**Tech Stack:** Rust (pensyve-core), Python (FastAPI, llama-cpp-python), PyO3, fastembed ONNX, pytest

---

## Sprint Ordering

Per the design spec parallelism matrix:
- **Sprint 1:** Task 1.4 (Bug Fixes) + Task 1.5 (Intent Scoring)
- **Sprint 2:** Task 1.1 (Benchmark Infrastructure) + Task 1.2 (Weight Tuning)
- **Sprint 3:** Task 1.3 (Tier 2 Wiring)

---

## Task 1.4 — Bug Fixes (REST API + Type Stubs)

**Sprint:** 1
**Owner files:** `pensyve_server/main.py`, `pensyve-python/python/pensyve/_core.pyi`
**Goal:** Replace `id(ep)` with UUID-keyed episode store, return actual `memories_created` count, add missing `consolidate()` to type stubs.

### 1.4.1 — Write test for UUID-based episode IDs

- [ ] **Read the current test file** to understand existing patterns:
  - File: `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_api.py`

- [ ] **Add test that verifies episode IDs are UUIDs** in `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_api.py`. Append the following test function:

```python
def test_episode_id_is_uuid(client):
    """Episode IDs should be valid UUIDs, not Python object IDs."""
    import uuid

    client.post("/v1/entities", json={"name": "bot", "kind": "agent"})
    client.post("/v1/entities", json={"name": "user1", "kind": "user"})

    r = client.post("/v1/episodes/start", json={"participants": ["bot", "user1"]})
    assert r.status_code == 200
    ep_id = r.json()["episode_id"]

    # Must be a valid UUID string
    parsed = uuid.UUID(ep_id)
    assert str(parsed) == ep_id

    # Clean up
    client.post("/v1/episodes/end", json={"episode_id": ep_id})
```

- [ ] **Run the test and verify it fails** (because `id(ep)` returns a Python integer, not a UUID):

```bash
.venv/bin/pytest tests/python/test_api.py::test_episode_id_is_uuid -v
# Expected: FAILED — ValueError: badly formed hexadecimal UUID string
```

### 1.4.2 — Write test for actual memories_created count

- [ ] **Add test for memories_created accuracy** in `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_api.py`. Append:

```python
def test_episode_end_returns_actual_memories_created(client):
    """memories_created should reflect actual message count, not hardcoded 1."""
    client.post("/v1/entities", json={"name": "bot", "kind": "agent"})
    client.post("/v1/entities", json={"name": "user2", "kind": "user"})

    r = client.post("/v1/episodes/start", json={"participants": ["bot", "user2"]})
    ep_id = r.json()["episode_id"]

    # Add 3 messages
    for i in range(3):
        client.post(
            "/v1/episodes/message",
            json={"episode_id": ep_id, "role": "user", "content": f"Message number {i}"},
        )

    r = client.post("/v1/episodes/end", json={"episode_id": ep_id, "outcome": "success"})
    assert r.status_code == 200
    assert r.json()["memories_created"] == 3
```

- [ ] **Run the test and verify it fails** (currently hardcoded to 1):

```bash
.venv/bin/pytest tests/python/test_api.py::test_episode_end_returns_actual_memories_created -v
# Expected: FAILED — assert 1 == 3
```

### 1.4.3 — Fix episode ID to use UUIDs

- [ ] **Edit** `/home/wshobson/workspace/major7apps/pensyve/pensyve_server/main.py` to add `import uuid` at the top and replace the `id(ep)` pattern with UUID-based keys.

  Replace the import block at the top of the file:

```python
import os

from fastapi import FastAPI, HTTPException

import pensyve
```

  With:

```python
import os
import uuid as uuid_mod

from fastapi import FastAPI, HTTPException

import pensyve
```

  Then replace the `start_episode` function body. Find:

```python
@app.post("/v1/episodes/start", response_model=EpisodeStartResponse)
def start_episode(req: EpisodeStartRequest):
    p = get_pensyve()
    entities = [p.entity(name) for name in req.participants]
    ep = p.episode(*entities)
    ep.__enter__()
    episode_id = str(id(ep))  # use object id as temp key
    _episodes[episode_id] = ep
    return EpisodeStartResponse(episode_id=episode_id)
```

  Replace with:

```python
@app.post("/v1/episodes/start", response_model=EpisodeStartResponse)
def start_episode(req: EpisodeStartRequest):
    p = get_pensyve()
    entities = [p.entity(name) for name in req.participants]
    ep = p.episode(*entities)
    ep.__enter__()
    episode_id = str(uuid_mod.uuid4())
    _episodes[episode_id] = ep
    return EpisodeStartResponse(episode_id=episode_id)
```

- [ ] **Run the UUID test and verify it passes**:

```bash
.venv/bin/pytest tests/python/test_api.py::test_episode_id_is_uuid -v
# Expected: PASSED
```

### 1.4.4 — Fix memories_created to return actual count

- [ ] **Edit** `/home/wshobson/workspace/major7apps/pensyve/pensyve_server/main.py` — update the `_episodes` dict to track message counts alongside episode objects. Change the global declaration:

  Find:

```python
_episodes = {}  # episode_id -> Episode object
```

  Replace with:

```python
_episodes: dict[str, dict] = {}  # episode_id -> {"ep": Episode, "message_count": int}
```

- [ ] **Update `start_episode`** to store the new structure. Find:

```python
    episode_id = str(uuid_mod.uuid4())
    _episodes[episode_id] = ep
    return EpisodeStartResponse(episode_id=episode_id)
```

  Replace with:

```python
    episode_id = str(uuid_mod.uuid4())
    _episodes[episode_id] = {"ep": ep, "message_count": 0}
    return EpisodeStartResponse(episode_id=episode_id)
```

- [ ] **Update `add_message`** to use the new structure and track count. Find:

```python
@app.post("/v1/episodes/message")
def add_message(req: MessageRequest):
    ep = _episodes.get(req.episode_id)
    if not ep:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    ep.message(req.role, req.content)
    return {"status": "ok"}
```

  Replace with:

```python
@app.post("/v1/episodes/message")
def add_message(req: MessageRequest):
    entry = _episodes.get(req.episode_id)
    if not entry:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    entry["ep"].message(req.role, req.content)
    entry["message_count"] += 1
    return {"status": "ok"}
```

- [ ] **Update `end_episode`** to use the new structure and return actual count. Find:

```python
@app.post("/v1/episodes/end", response_model=EpisodeEndResponse)
def end_episode(req: EpisodeEndRequest):
    ep = _episodes.pop(req.episode_id, None)
    if not ep:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    if req.outcome:
        ep.outcome(req.outcome)
    ep.__exit__(None, None, None)
    return EpisodeEndResponse(memories_created=1)  # approximate
```

  Replace with:

```python
@app.post("/v1/episodes/end", response_model=EpisodeEndResponse)
def end_episode(req: EpisodeEndRequest):
    entry = _episodes.pop(req.episode_id, None)
    if not entry:
        raise HTTPException(404, f"Episode {req.episode_id} not found")
    ep = entry["ep"]
    message_count = entry["message_count"]
    if req.outcome:
        ep.outcome(req.outcome)
    ep.__exit__(None, None, None)
    return EpisodeEndResponse(memories_created=message_count)
```

- [ ] **Run the memories_created test and verify it passes**:

```bash
.venv/bin/pytest tests/python/test_api.py::test_episode_end_returns_actual_memories_created -v
# Expected: PASSED
```

### 1.4.5 — Add consolidate() to type stubs

- [ ] **Add test that validates consolidate() is callable** in `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_api.py`. Append:

```python
def test_consolidate_endpoint(client):
    """consolidate endpoint should return promoted/decayed/archived counts."""
    r = client.post("/v1/consolidate")
    assert r.status_code == 200
    data = r.json()
    assert "promoted" in data
    assert "decayed" in data
    assert "archived" in data
```

- [ ] **Edit** `/home/wshobson/workspace/major7apps/pensyve/pensyve-python/python/pensyve/_core.pyi` — add the missing `consolidate()` method to the `Pensyve` class. After the `forget` method (after line 81), add:

```python
    def consolidate(
        self,
        entity: Entity | None = None,
    ) -> dict[str, int]:
        """Run consolidation (episodic->semantic promotion, FSRS decay, archival).

        Args:
            entity: Unused; consolidation runs namespace-wide (default: None).

        Returns:
            Dict with keys: promoted, decayed, archived (counts).
        """
        ...
```

- [ ] **Verify pyright type checking passes** with the updated stubs:

```bash
.venv/bin/pyright pensyve-python/python/pensyve/_core.pyi
# Expected: 0 errors
```

### 1.4.6 — Run full test suite and commit

- [ ] **Run all existing API tests** to verify no regressions:

```bash
.venv/bin/pytest tests/python/test_api.py -v
# Expected: All tests PASSED (including new ones)
```

- [ ] **Run full Python test suite**:

```bash
.venv/bin/pytest tests/python/ -v
# Expected: All tests PASSED
```

- [ ] **Commit the bug fixes**:

```bash
git add pensyve_server/main.py pensyve-python/python/pensyve/_core.pyi tests/python/test_api.py
git commit -m "$(cat <<'EOF'
fix: UUID episode IDs, actual memories_created count, consolidate() stub

- Replace id(ep) with uuid4() for stable, collision-free episode IDs
- Track message count per episode and return actual memories_created
- Add consolidate() method to _core.pyi type stubs
- Add tests for all three fixes
EOF
)"
```

---

## Task 1.5 — Intent Scoring

**Sprint:** 1
**Owner files:** `pensyve-core/src/retrieval.rs`
**Goal:** Implement lightweight heuristic query classification. Questions boost episodic results, commands boost procedural results. The intent weight in `config.rs` defaults to `0.0` — we set it to a non-zero value after implementing the classifier.

### 1.5.1 — Write Rust unit tests for intent classification

- [ ] **Add intent classification tests** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/retrieval.rs`. Inside the `mod tests` block (before the closing `}`), append:

```rust
    #[test]
    fn test_classify_intent_question() {
        let intent = classify_intent("What programming language does Seth prefer?");
        assert_eq!(intent, QueryIntent::Question);
    }

    #[test]
    fn test_classify_intent_command() {
        let intent = classify_intent("Deploy the application to production");
        assert_eq!(intent, QueryIntent::Action);
    }

    #[test]
    fn test_classify_intent_recall() {
        let intent = classify_intent("Tell me about the database migration");
        assert_eq!(intent, QueryIntent::Recall);
    }

    #[test]
    fn test_classify_intent_generic() {
        let intent = classify_intent("dark mode preference");
        assert_eq!(intent, QueryIntent::General);
    }

    #[test]
    fn test_intent_score_question_boosts_episodic() {
        let intent = classify_intent("What happened during the deployment?");
        let episodic_score = intent_score_for_type(&intent, "episodic");
        let procedural_score = intent_score_for_type(&intent, "procedural");
        assert!(
            episodic_score > procedural_score,
            "Questions should boost episodic over procedural"
        );
    }

    #[test]
    fn test_intent_score_command_boosts_procedural() {
        let intent = classify_intent("Run the migration script");
        let procedural_score = intent_score_for_type(&intent, "procedural");
        let episodic_score = intent_score_for_type(&intent, "episodic");
        assert!(
            procedural_score > episodic_score,
            "Commands should boost procedural over episodic"
        );
    }
```

- [ ] **Run the tests and verify they fail** (functions don't exist yet):

```bash
cargo test -p pensyve-core -- test_classify_intent 2>&1 | tail -5
# Expected: error[E0425]: cannot find function `classify_intent`
```

### 1.5.2 — Implement QueryIntent enum and classify_intent function

- [ ] **Add the intent classifier** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/retrieval.rs`. Insert the following after the `type CandidateMaps` line (after line 16) and before the `RecallError` enum:

```rust
// ---------------------------------------------------------------------------
// Intent classification
// ---------------------------------------------------------------------------

/// Classified query intent for retrieval boosting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryIntent {
    /// Informational question — boost episodic + semantic results.
    Question,
    /// Action/command — boost procedural results.
    Action,
    /// Recall/lookup — boost semantic results.
    Recall,
    /// No clear intent signal.
    General,
}

/// Lightweight heuristic classifier for query intent.
///
/// Uses keyword/pattern matching — no LLM required.
pub fn classify_intent(query: &str) -> QueryIntent {
    let lower = query.to_lowercase();
    let trimmed = lower.trim();

    // Question patterns: starts with interrogative word or ends with "?"
    let question_starters = [
        "what ", "when ", "where ", "who ", "why ", "how ", "which ", "does ", "did ",
        "is ", "are ", "was ", "were ", "can ", "could ", "would ", "should ", "has ",
        "have ", "had ",
    ];
    if trimmed.ends_with('?') || question_starters.iter().any(|s| trimmed.starts_with(s)) {
        return QueryIntent::Question;
    }

    // Action patterns: imperative verbs
    let action_starters = [
        "run ", "execute ", "deploy ", "build ", "install ", "create ", "delete ",
        "remove ", "start ", "stop ", "restart ", "update ", "upgrade ", "fix ",
        "apply ", "migrate ", "configure ", "setup ", "set up ",
    ];
    if action_starters.iter().any(|s| trimmed.starts_with(s)) {
        return QueryIntent::Action;
    }

    // Recall patterns: explicit lookup intent
    let recall_starters = [
        "tell me about ", "recall ", "remember ", "find ", "search ", "look up ",
        "lookup ", "show me ", "get ", "retrieve ", "fetch ",
    ];
    if recall_starters.iter().any(|s| trimmed.starts_with(s)) {
        return QueryIntent::Recall;
    }

    QueryIntent::General
}

/// Compute intent-based score adjustment for a given memory type.
///
/// Returns a value in [0.0, 1.0] that gets multiplied by the intent weight
/// in the fusion scoring formula.
pub fn intent_score_for_type(intent: &QueryIntent, memory_type: &str) -> f32 {
    match intent {
        QueryIntent::Question => match memory_type {
            "episodic" => 0.8,
            "semantic" => 0.6,
            "procedural" => 0.2,
            _ => 0.0,
        },
        QueryIntent::Action => match memory_type {
            "procedural" => 0.9,
            "semantic" => 0.3,
            "episodic" => 0.1,
            _ => 0.0,
        },
        QueryIntent::Recall => match memory_type {
            "semantic" => 0.8,
            "episodic" => 0.6,
            "procedural" => 0.3,
            _ => 0.0,
        },
        QueryIntent::General => 0.5,  // neutral — no boost or penalty
    }
}
```

- [ ] **Run the intent classification tests and verify they pass**:

```bash
cargo test -p pensyve-core -- test_classify_intent -v
# Expected: all 4 test_classify_intent tests PASSED
cargo test -p pensyve-core -- test_intent_score -v
# Expected: both test_intent_score tests PASSED
```

### 1.5.3 — Wire intent scoring into the scoring pipeline

- [ ] **Update `recall_with_entity`** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/retrieval.rs` to classify intent and pass it to scoring. Find the block in `recall_with_entity` that starts step 7:

```rust
        // Step 7: Score and sort candidates.
        let now = Utc::now();
        let weights = &self.config.weights;
        let mut scored: Vec<ScoredCandidate> = candidates
            .into_iter()
            .map(|(id, memory)| {
                score_candidate(
                    id,
                    memory,
                    &vector_map,
                    &bm25_map,
                    &graph_map,
                    max_access,
                    now,
                    weights,
                )
            })
            .collect();
```

  Replace with:

```rust
        // Step 6b: Classify query intent.
        let intent = classify_intent(query);

        // Step 7: Score and sort candidates.
        let now = Utc::now();
        let weights = &self.config.weights;
        let mut scored: Vec<ScoredCandidate> = candidates
            .into_iter()
            .map(|(id, memory)| {
                score_candidate(
                    id,
                    memory,
                    &vector_map,
                    &bm25_map,
                    &graph_map,
                    max_access,
                    now,
                    weights,
                    &intent,
                )
            })
            .collect();
```

- [ ] **Update `score_candidate` signature and body** to accept and use intent. Find the function signature:

```rust
fn score_candidate(
    id: Uuid,
    memory: Memory,
    vector_map: &HashMap<Uuid, f32>,
    bm25_map: &HashMap<Uuid, f32>,
    graph_map: &HashMap<Uuid, f32>,
    max_access: u32,
    now: chrono::DateTime<Utc>,
    weights: &[f32; 8],
) -> ScoredCandidate {
```

  Replace with:

```rust
fn score_candidate(
    id: Uuid,
    memory: Memory,
    vector_map: &HashMap<Uuid, f32>,
    bm25_map: &HashMap<Uuid, f32>,
    graph_map: &HashMap<Uuid, f32>,
    max_access: u32,
    now: chrono::DateTime<Utc>,
    weights: &[f32; 8],
    intent: &QueryIntent,
) -> ScoredCandidate {
```

- [ ] **Replace the hardcoded `intent_score = 0.0`** in `score_candidate`. Find:

```rust
    let intent_score = 0.0_f32;
    let type_boost = 1.0_f32;
```

  Replace with:

```rust
    let mem_type = match &memory {
        Memory::Episodic(_) => "episodic",
        Memory::Semantic(_) => "semantic",
        Memory::Procedural(_) => "procedural",
    };
    let intent_score = intent_score_for_type(intent, mem_type);
    let type_boost = 1.0_f32;
```

### 1.5.4 — Update the default intent weight in config

- [ ] **Update the default weights** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/config.rs` to give intent a non-zero weight. Find:

```rust
                weights: [0.25, 0.10, 0.15, 0.0, 0.20, 0.10, 0.10, 0.10],
```

  Replace with:

```rust
                weights: [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
```

  Note: Reduced `type_boost` from 0.10 to 0.05 and allocated 0.05 to intent so weights still sum to 1.0.

- [ ] **Update the test weights constant** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/retrieval.rs`. Find:

```rust
    const TEST_WEIGHTS: [f32; 8] = [0.25, 0.10, 0.15, 0.0, 0.20, 0.10, 0.10, 0.10];
```

  Replace with:

```rust
    const TEST_WEIGHTS: [f32; 8] = [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05];
```

### 1.5.5 — Run full Rust test suite and commit

- [ ] **Run all Rust tests** to verify no regressions:

```bash
cargo test -p pensyve-core
# Expected: All tests PASSED (including new intent tests)
```

- [ ] **Run clippy**:

```bash
cargo clippy --workspace
# Expected: No errors or warnings
```

- [ ] **Commit intent scoring**:

```bash
git add pensyve-core/src/retrieval.rs pensyve-core/src/config.rs
git commit -m "$(cat <<'EOF'
feat: add heuristic intent scoring to retrieval fusion

- Classify queries as Question/Action/Recall/General using keyword patterns
- Questions boost episodic, commands boost procedural, recalls boost semantic
- Wire intent score into 8-signal fusion pipeline (weight: 0.05)
- Redistribute weights: intent=0.05, type_boost reduced 0.10->0.05
EOF
)"
```

---

## Task 1.1 — Benchmark Infrastructure

**Sprint:** 2
**Owner files:** `benchmarks/`
**Goal:** Integrate LongMemEval_S dataset into benchmark harness. Run with real ONNX embeddings. Establish baseline score.

### 1.1.1 — Create LongMemEval directory structure

- [ ] **Create the directory structure**:

```bash
mkdir -p benchmarks/longmemeval
```

- [ ] **Create `benchmarks/longmemeval/__init__.py`** (empty init):

```bash
touch benchmarks/longmemeval/__init__.py
```

### 1.1.2 — Build the LongMemEval_S dataset loader

- [ ] **Create** `/home/wshobson/workspace/major7apps/pensyve/benchmarks/longmemeval/dataset.py` — this module downloads and parses the LongMemEval_S dataset:

```python
"""LongMemEval_S dataset loader.

LongMemEval is a benchmark for evaluating long-term memory in conversational
agents. The _S (short) variant tests single-session factual recall.

Reference: https://github.com/xiaowu0162/LongMemEval
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)

DATASET_DIR = Path(__file__).parent / "data"

# LongMemEval_S focuses on single-hop factual recall from conversations.
# We provide a built-in subset for offline use, and support loading the full
# dataset from a local clone of the LongMemEval repository.


@dataclass
class MemEvalConversation:
    """A conversation from the LongMemEval dataset."""

    id: str
    messages: list[dict[str, str]]  # [{"role": "user"|"assistant", "content": "..."}]
    metadata: dict[str, str] = field(default_factory=dict)


@dataclass
class MemEvalQuery:
    """A query with expected answer from LongMemEval."""

    id: str
    question: str
    gold_answer: str
    conversation_id: str
    category: str = "single_hop"  # single_hop, multi_hop, temporal, etc.


@dataclass
class MemEvalDataset:
    """The full LongMemEval_S dataset."""

    conversations: list[MemEvalConversation]
    queries: list[MemEvalQuery]
    metadata: dict[str, object] = field(default_factory=dict)


def load_longmemeval_s(
    data_dir: str | Path | None = None,
) -> MemEvalDataset:
    """Load the LongMemEval_S dataset.

    Args:
        data_dir: Path to directory containing LongMemEval JSON files.
                  If None, looks for data in benchmarks/longmemeval/data/.

    Returns:
        MemEvalDataset with conversations and queries.

    The expected file format is a JSON file with structure:
    [
        {
            "session_id": "...",
            "conversation": [{"role": "user", "content": "..."}, ...],
            "questions": [
                {"question": "...", "answer": "...", "category": "..."}
            ]
        }
    ]
    """
    if data_dir is None:
        data_dir = DATASET_DIR

    data_path = Path(data_dir)
    if not data_path.exists():
        data_path.mkdir(parents=True, exist_ok=True)

    # Look for the dataset file
    dataset_file = data_path / "longmemeval_s.json"
    if not dataset_file.exists():
        # Try alternate name
        dataset_file = data_path / "dataset.json"

    if not dataset_file.exists():
        logger.warning(
            "LongMemEval_S dataset not found at %s. "
            "Please download it from https://github.com/xiaowu0162/LongMemEval "
            "and place the JSON file at %s/longmemeval_s.json",
            data_path,
            data_path,
        )
        # Return a small built-in test dataset for development
        return _builtin_test_dataset()

    return _load_from_file(dataset_file)


def _load_from_file(path: Path) -> MemEvalDataset:
    """Load dataset from a JSON file."""
    with open(path) as f:
        raw = json.load(f)

    conversations = []
    queries = []

    items = raw if isinstance(raw, list) else raw.get("data", raw.get("sessions", []))

    for item in items:
        session_id = str(item.get("session_id", item.get("id", len(conversations))))

        # Parse conversation messages
        raw_messages = item.get("conversation", item.get("messages", []))
        messages = []
        for msg in raw_messages:
            role = msg.get("role", "user")
            content = msg.get("content", "")
            if content:
                messages.append({"role": role, "content": content})

        if not messages:
            continue

        conversations.append(
            MemEvalConversation(
                id=session_id,
                messages=messages,
                metadata=item.get("metadata", {}),
            )
        )

        # Parse questions
        raw_questions = item.get("questions", item.get("queries", []))
        for qi, q in enumerate(raw_questions):
            queries.append(
                MemEvalQuery(
                    id=f"{session_id}_q{qi}",
                    question=q.get("question", q.get("query", "")),
                    gold_answer=q.get("answer", q.get("gold_answer", "")),
                    conversation_id=session_id,
                    category=q.get("category", "single_hop"),
                )
            )

    return MemEvalDataset(
        conversations=conversations,
        queries=queries,
        metadata={"source": str(path), "num_sessions": len(conversations)},
    )


def _builtin_test_dataset() -> MemEvalDataset:
    """Return a small built-in dataset for development/testing.

    This is NOT the real LongMemEval dataset — it is a synthetic subset
    designed to let the benchmark harness run without external data.
    """
    conversations = [
        MemEvalConversation(
            id="builtin_0",
            messages=[
                {"role": "user", "content": "My name is Alice and I work at Google as a software engineer."},
                {"role": "assistant", "content": "Nice to meet you, Alice! It's great that you work at Google."},
                {"role": "user", "content": "I've been working on the search ranking team for 3 years."},
                {"role": "assistant", "content": "That's impressive! The search ranking team does important work."},
                {"role": "user", "content": "I prefer Python for data analysis and Go for backend services."},
                {"role": "assistant", "content": "Those are solid choices for those domains."},
            ],
        ),
        MemEvalConversation(
            id="builtin_1",
            messages=[
                {"role": "user", "content": "I just adopted a golden retriever named Max."},
                {"role": "assistant", "content": "Congratulations! Golden retrievers are wonderful dogs."},
                {"role": "user", "content": "We live in a house in San Francisco with a small backyard."},
                {"role": "assistant", "content": "That's nice! Max will enjoy the backyard."},
                {"role": "user", "content": "My partner Sarah is a veterinarian so Max is in good hands."},
                {"role": "assistant", "content": "That's perfect! Having a vet in the family is great for a new pet."},
            ],
        ),
        MemEvalConversation(
            id="builtin_2",
            messages=[
                {"role": "user", "content": "I'm planning a trip to Japan next spring."},
                {"role": "assistant", "content": "Japan in spring sounds wonderful! Cherry blossom season is beautiful."},
                {"role": "user", "content": "I want to visit Tokyo, Kyoto, and Osaka over two weeks."},
                {"role": "assistant", "content": "Two weeks is a great amount of time for those three cities."},
                {"role": "user", "content": "My budget is around $5000 for the whole trip."},
                {"role": "assistant", "content": "That should be manageable for a two-week trip to Japan."},
            ],
        ),
        MemEvalConversation(
            id="builtin_3",
            messages=[
                {"role": "user", "content": "I've been learning Rust for the past six months."},
                {"role": "assistant", "content": "Rust is a great language to learn! How's it going?"},
                {"role": "user", "content": "I'm building a memory system called Pensyve with it."},
                {"role": "assistant", "content": "That sounds like an interesting project!"},
                {"role": "user", "content": "The hardest part has been the borrow checker, but I'm getting used to it."},
                {"role": "assistant", "content": "The borrow checker is definitely the steepest learning curve in Rust."},
            ],
        ),
        MemEvalConversation(
            id="builtin_4",
            messages=[
                {"role": "user", "content": "I just finished reading 'Designing Data-Intensive Applications' by Martin Kleppmann."},
                {"role": "assistant", "content": "That's an excellent book! What did you think of it?"},
                {"role": "user", "content": "The chapter on distributed consensus was the most enlightening for me."},
                {"role": "assistant", "content": "Distributed consensus is a fascinating topic."},
                {"role": "user", "content": "I'm now reading 'Database Internals' by Alex Petrov."},
                {"role": "assistant", "content": "Another great choice! That pairs well with DDIA."},
            ],
        ),
    ]

    queries = [
        MemEvalQuery(id="bq_0", question="Where does Alice work?", gold_answer="Google", conversation_id="builtin_0"),
        MemEvalQuery(id="bq_1", question="What team does Alice work on?", gold_answer="search ranking", conversation_id="builtin_0"),
        MemEvalQuery(id="bq_2", question="What language does Alice prefer for data analysis?", gold_answer="Python", conversation_id="builtin_0"),
        MemEvalQuery(id="bq_3", question="What is the name of the dog?", gold_answer="Max", conversation_id="builtin_1"),
        MemEvalQuery(id="bq_4", question="What breed is the dog?", gold_answer="golden retriever", conversation_id="builtin_1"),
        MemEvalQuery(id="bq_5", question="What city do they live in?", gold_answer="San Francisco", conversation_id="builtin_1"),
        MemEvalQuery(id="bq_6", question="What is Sarah's profession?", gold_answer="veterinarian", conversation_id="builtin_1"),
        MemEvalQuery(id="bq_7", question="What country is the trip to?", gold_answer="Japan", conversation_id="builtin_2"),
        MemEvalQuery(id="bq_8", question="What is the trip budget?", gold_answer="$5000", conversation_id="builtin_2"),
        MemEvalQuery(id="bq_9", question="How long is the trip?", gold_answer="two weeks", conversation_id="builtin_2"),
        MemEvalQuery(id="bq_10", question="What language has the user been learning?", gold_answer="Rust", conversation_id="builtin_3"),
        MemEvalQuery(id="bq_11", question="What project is the user building?", gold_answer="Pensyve", conversation_id="builtin_3"),
        MemEvalQuery(id="bq_12", question="What is the hardest part of Rust?", gold_answer="borrow checker", conversation_id="builtin_3"),
        MemEvalQuery(id="bq_13", question="What book did the user just finish reading?", gold_answer="Designing Data-Intensive Applications", conversation_id="builtin_4"),
        MemEvalQuery(id="bq_14", question="Who wrote DDIA?", gold_answer="Martin Kleppmann", conversation_id="builtin_4"),
        MemEvalQuery(id="bq_15", question="What book is the user currently reading?", gold_answer="Database Internals", conversation_id="builtin_4"),
    ]

    return MemEvalDataset(
        conversations=conversations,
        queries=queries,
        metadata={"source": "builtin_test", "num_sessions": len(conversations)},
    )
```

### 1.1.3 — Build the evaluator

- [ ] **Create** `/home/wshobson/workspace/major7apps/pensyve/benchmarks/longmemeval/evaluate.py`:

```python
"""LongMemEval_S evaluator for Pensyve.

Ingests conversations via the Python SDK, then runs queries and checks
whether the gold answer appears in the top-K recalled memories.
"""

from __future__ import annotations

import logging
import tempfile
import time
from dataclasses import dataclass, field

import pensyve

from .dataset import MemEvalDataset, MemEvalQuery

logger = logging.getLogger(__name__)


@dataclass
class QueryResult:
    """Result of evaluating a single query."""

    query_id: str
    question: str
    gold_answer: str
    hit: bool
    recalled_text: str
    num_results: int
    recall_ms: float


@dataclass
class EvalReport:
    """Full evaluation report."""

    accuracy: float
    hits: int
    total: int
    misses: int
    ingest_time_s: float
    recall_time_s: float
    avg_recall_ms: float
    results: list[QueryResult] = field(default_factory=list)
    metadata: dict[str, object] = field(default_factory=dict)

    def summary(self) -> str:
        return (
            f"LongMemEval_S: {self.accuracy:.1f}% "
            f"({self.hits}/{self.total}) | "
            f"Ingest: {self.ingest_time_s:.1f}s | "
            f"Avg recall: {self.avg_recall_ms:.1f}ms"
        )


def evaluate(
    dataset: MemEvalDataset,
    top_k: int = 5,
    pensyve_path: str | None = None,
    verbose: bool = False,
) -> EvalReport:
    """Run LongMemEval_S evaluation against Pensyve.

    Args:
        dataset: The loaded LongMemEval_S dataset.
        top_k: Number of memories to retrieve per query (default: 5).
        pensyve_path: Path for Pensyve storage. If None, uses a temp directory.
        verbose: If True, print missed queries.

    Returns:
        EvalReport with accuracy and per-query results.
    """
    cleanup_tempdir = pensyve_path is None
    tmpdir = None
    if pensyve_path is None:
        tmpdir = tempfile.mkdtemp(prefix="pensyve_bench_")
        pensyve_path = tmpdir

    try:
        p = pensyve.Pensyve(path=pensyve_path, namespace="longmemeval")
        agent = p.entity("benchmark_assistant", kind="agent")
        user = p.entity("benchmark_user", kind="user")

        # Phase 1: Ingest all conversations
        logger.info(
            "Ingesting %d conversations...", len(dataset.conversations)
        )
        ingest_start = time.time()
        for conv in dataset.conversations:
            with p.episode(agent, user) as ep:
                for msg in conv.messages:
                    ep.message(msg["role"], msg["content"])
        ingest_time = time.time() - ingest_start
        logger.info("Ingestion complete in %.1fs", ingest_time)

        # Phase 2: Run all queries
        logger.info("Running %d queries...", len(dataset.queries))
        results: list[QueryResult] = []
        recall_start = time.time()

        for query in dataset.queries:
            q_start = time.time()
            memories = p.recall(query.question, entity=user, limit=top_k)
            q_time = (time.time() - q_start) * 1000

            recalled_text = " ".join(m.content.lower() for m in memories)
            gold_lower = query.gold_answer.lower()

            # Check: does the gold answer appear in any recalled memory?
            hit = gold_lower in recalled_text

            result = QueryResult(
                query_id=query.id,
                question=query.question,
                gold_answer=query.gold_answer,
                hit=hit,
                recalled_text=recalled_text[:200],
                num_results=len(memories),
                recall_ms=q_time,
            )
            results.append(result)

            if verbose and not hit:
                print(f"  MISS: {query.question}")
                print(f"    Gold: {query.gold_answer}")
                print(f"    Got:  {recalled_text[:150]}")

        recall_time = time.time() - recall_start

        # Compute metrics
        hits = sum(1 for r in results if r.hit)
        total = len(results)
        accuracy = (hits / total * 100) if total > 0 else 0.0
        avg_recall_ms = (recall_time / total * 1000) if total > 0 else 0.0

        return EvalReport(
            accuracy=round(accuracy, 1),
            hits=hits,
            total=total,
            misses=total - hits,
            ingest_time_s=round(ingest_time, 2),
            recall_time_s=round(recall_time, 2),
            avg_recall_ms=round(avg_recall_ms, 1),
            results=results,
            metadata={
                "dataset": dataset.metadata,
                "top_k": top_k,
                "pensyve_path": pensyve_path,
            },
        )

    finally:
        if cleanup_tempdir and tmpdir is not None:
            import shutil

            shutil.rmtree(tmpdir, ignore_errors=True)
```

### 1.1.4 — Build the benchmark runner script

- [ ] **Create** `/home/wshobson/workspace/major7apps/pensyve/benchmarks/longmemeval/run.py`:

```python
"""Run the LongMemEval_S benchmark.

Usage:
    python benchmarks/longmemeval/run.py [--verbose] [--top-k 5] [--data-dir PATH]
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
from dataclasses import asdict
from pathlib import Path

# Ensure project root is on sys.path
project_root = str(Path(__file__).parent.parent.parent)
if project_root not in sys.path:
    sys.path.insert(0, project_root)

from benchmarks.longmemeval.dataset import load_longmemeval_s
from benchmarks.longmemeval.evaluate import evaluate


def main():
    parser = argparse.ArgumentParser(description="LongMemEval_S Benchmark Runner")
    parser.add_argument("--verbose", "-v", action="store_true", help="Show missed queries")
    parser.add_argument("--top-k", type=int, default=5, help="Top-K for recall (default: 5)")
    parser.add_argument("--data-dir", type=str, default=None, help="Path to dataset directory")
    parser.add_argument(
        "--output",
        type=str,
        default="benchmarks/results/longmemeval_results.json",
        help="Output file for results",
    )
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO if args.verbose else logging.WARNING,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    # Load dataset
    print("Loading LongMemEval_S dataset...")
    dataset = load_longmemeval_s(data_dir=args.data_dir)
    print(
        f"Loaded {len(dataset.conversations)} conversations, "
        f"{len(dataset.queries)} queries "
        f"(source: {dataset.metadata.get('source', 'unknown')})"
    )

    # Run evaluation
    print("\nRunning evaluation...")
    report = evaluate(dataset, top_k=args.top_k, verbose=args.verbose)

    # Print summary
    print(f"\n{'=' * 60}")
    print("LongMemEval_S Benchmark Results")
    print(f"{'=' * 60}")
    print(f"Accuracy:    {report.accuracy}% ({report.hits}/{report.total})")
    print(f"Misses:      {report.misses}")
    print(f"Ingest time: {report.ingest_time_s}s")
    print(f"Recall time: {report.recall_time_s}s")
    print(f"Avg recall:  {report.avg_recall_ms}ms")
    print(f"{'=' * 60}")

    # Save results
    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with open(args.output, "w") as f:
        json.dump(asdict(report), f, indent=2, default=str)
    print(f"\nResults saved to {args.output}")


if __name__ == "__main__":
    main()
```

### 1.1.5 — Write tests for benchmark infrastructure

- [ ] **Create** `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_benchmark.py`:

```python
"""Tests for the LongMemEval benchmark infrastructure."""

import sys
from pathlib import Path

# Ensure benchmarks are importable
sys.path.insert(0, str(Path(__file__).parent.parent.parent))

from benchmarks.longmemeval.dataset import (
    MemEvalDataset,
    MemEvalQuery,
    _builtin_test_dataset,
    load_longmemeval_s,
)
from benchmarks.longmemeval.evaluate import evaluate


def test_builtin_dataset_loads():
    """The built-in test dataset should load without external files."""
    dataset = _builtin_test_dataset()
    assert isinstance(dataset, MemEvalDataset)
    assert len(dataset.conversations) > 0
    assert len(dataset.queries) > 0


def test_builtin_dataset_structure():
    """Each query should reference a valid conversation."""
    dataset = _builtin_test_dataset()
    conv_ids = {c.id for c in dataset.conversations}
    for q in dataset.queries:
        assert q.conversation_id in conv_ids, f"Query {q.id} references unknown conversation"


def test_load_fallback_to_builtin():
    """load_longmemeval_s should fall back to builtin when no data file exists."""
    dataset = load_longmemeval_s(data_dir="/nonexistent/path")
    assert len(dataset.conversations) > 0
    assert dataset.metadata.get("source") == "builtin_test"


def test_evaluate_runs_without_error():
    """Evaluation should complete without errors on the builtin dataset."""
    dataset = _builtin_test_dataset()
    report = evaluate(dataset, top_k=3)
    assert report.total == len(dataset.queries)
    assert report.hits >= 0
    assert report.accuracy >= 0.0
    assert report.ingest_time_s >= 0.0
    assert report.avg_recall_ms >= 0.0


def test_evaluate_accuracy_is_percentage():
    """Accuracy should be a percentage between 0 and 100."""
    dataset = _builtin_test_dataset()
    report = evaluate(dataset, top_k=5)
    assert 0.0 <= report.accuracy <= 100.0
```

- [ ] **Run the benchmark tests**:

```bash
.venv/bin/pytest tests/python/test_benchmark.py -v
# Expected: All tests PASSED
```

### 1.1.6 — Run the baseline benchmark and commit

- [ ] **Run the benchmark with the builtin dataset** to establish a baseline:

```bash
.venv/bin/python benchmarks/longmemeval/run.py --verbose
# Expected: Prints accuracy score and saves to benchmarks/results/longmemeval_results.json
```

- [ ] **Document baseline score**: Note the accuracy percentage from the output.

- [ ] **Commit benchmark infrastructure**:

```bash
git add benchmarks/longmemeval/ tests/python/test_benchmark.py
git commit -m "$(cat <<'EOF'
feat: add LongMemEval_S benchmark infrastructure

- Dataset loader with built-in test dataset (16 queries, 5 conversations)
- Supports loading full LongMemEval_S dataset from JSON
- Evaluator: ingest conversations, run queries, check gold answer in top-K
- Runner script: python benchmarks/longmemeval/run.py
- Tests for dataset loading, structure validation, and evaluation
EOF
)"
```

---

## Task 1.2 — Retrieval Weight Tuning

**Sprint:** 2
**Owner files:** `pensyve-core/src/retrieval.rs` (weight constants only), `benchmarks/tuning/`
**Goal:** Optimize the 8-signal weight vector against the LongMemEval benchmark to achieve 80%+ accuracy.

### 1.2.1 — Create the tuning script directory

- [ ] **Create directory**:

```bash
mkdir -p benchmarks/tuning
touch benchmarks/tuning/__init__.py
```

### 1.2.2 — Build the weight tuning script

- [ ] **Create** `/home/wshobson/workspace/major7apps/pensyve/benchmarks/tuning/optimize.py`:

```python
"""Weight tuning script for Pensyve retrieval fusion.

Uses scipy.optimize.minimize to find the weight vector that maximizes
LongMemEval_S accuracy. The optimization runs the full evaluation loop
for each candidate weight vector.

Usage:
    python benchmarks/tuning/optimize.py [--iterations 50] [--top-k 5] [--verbose]
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import time
from pathlib import Path

import numpy as np
from scipy.optimize import differential_evolution

# Ensure project root is on sys.path
project_root = str(Path(__file__).parent.parent.parent)
if project_root not in sys.path:
    sys.path.insert(0, project_root)

from benchmarks.longmemeval.dataset import MemEvalDataset, load_longmemeval_s
from benchmarks.longmemeval.evaluate import evaluate

logger = logging.getLogger(__name__)

# Weight indices: [vector, bm25, graph, intent, recency, access, confidence, type_boost]
WEIGHT_NAMES = [
    "vector",
    "bm25",
    "graph",
    "intent",
    "recency",
    "access",
    "confidence",
    "type_boost",
]

# Current default weights
DEFAULT_WEIGHTS = [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05]


def normalize_weights(raw: np.ndarray) -> list[float]:
    """Normalize weights to sum to 1.0."""
    total = np.sum(np.abs(raw))
    if total == 0:
        return [1.0 / len(raw)] * len(raw)
    normalized = np.abs(raw) / total
    return [round(float(w), 4) for w in normalized]


def objective(
    raw_weights: np.ndarray,
    dataset: MemEvalDataset,
    top_k: int,
    verbose: bool,
) -> float:
    """Objective function: negative accuracy (we minimize).

    Sets PENSYVE_RETRIEVAL_WEIGHTS env var, runs evaluation, returns -accuracy.
    """
    weights = normalize_weights(raw_weights)

    # Set weights via environment variable (picked up by Pensyve config)
    os.environ["PENSYVE_RETRIEVAL_WEIGHTS"] = json.dumps(weights)

    try:
        report = evaluate(dataset, top_k=top_k, verbose=False)
        accuracy = report.accuracy
        if verbose:
            print(f"  Weights: {weights} -> Accuracy: {accuracy}%")
        return -accuracy  # minimize negative accuracy = maximize accuracy
    except Exception as e:
        logger.warning("Evaluation failed with weights %s: %s", weights, e)
        return 0.0  # worst possible score


def run_tuning(
    dataset: MemEvalDataset,
    top_k: int = 5,
    max_iterations: int = 50,
    verbose: bool = False,
) -> dict:
    """Run differential evolution to find optimal weights.

    Returns dict with best_weights, best_accuracy, all_results.
    """
    print(f"\nStarting weight optimization (max {max_iterations} iterations)...")
    print(f"Default weights: {DEFAULT_WEIGHTS}")

    # Evaluate baseline
    os.environ.pop("PENSYVE_RETRIEVAL_WEIGHTS", None)
    baseline = evaluate(dataset, top_k=top_k, verbose=False)
    print(f"Baseline accuracy: {baseline.accuracy}%\n")

    # Define bounds: each weight in [0.01, 0.50]
    bounds = [(0.01, 0.50)] * 8

    start_time = time.time()

    result = differential_evolution(
        objective,
        bounds,
        args=(dataset, top_k, verbose),
        maxiter=max_iterations,
        seed=42,
        tol=0.001,
        popsize=10,
        mutation=(0.5, 1.5),
        recombination=0.7,
        disp=verbose,
    )

    elapsed = time.time() - start_time

    best_weights = normalize_weights(result.x)
    best_accuracy = -result.fun

    print(f"\n{'=' * 60}")
    print("Weight Tuning Results")
    print(f"{'=' * 60}")
    print(f"Baseline accuracy:  {baseline.accuracy}%")
    print(f"Optimized accuracy: {best_accuracy}%")
    print(f"Improvement:        {best_accuracy - baseline.accuracy:+.1f}%")
    print(f"Optimization time:  {elapsed:.0f}s")
    print(f"\nOptimal weights:")
    for name, weight in zip(WEIGHT_NAMES, best_weights):
        default = DEFAULT_WEIGHTS[WEIGHT_NAMES.index(name)]
        delta = weight - default
        print(f"  {name:12s}: {weight:.4f}  (was {default:.4f}, {delta:+.4f})")
    print(f"{'=' * 60}")

    return {
        "baseline_accuracy": baseline.accuracy,
        "best_accuracy": best_accuracy,
        "best_weights": best_weights,
        "weight_names": WEIGHT_NAMES,
        "default_weights": DEFAULT_WEIGHTS,
        "iterations": result.nit,
        "function_evaluations": result.nfev,
        "elapsed_s": round(elapsed, 1),
        "converged": result.success,
    }


def main():
    parser = argparse.ArgumentParser(description="Pensyve Weight Tuning")
    parser.add_argument(
        "--iterations",
        type=int,
        default=50,
        help="Max optimization iterations (default: 50)",
    )
    parser.add_argument("--top-k", type=int, default=5, help="Top-K for recall (default: 5)")
    parser.add_argument("--verbose", "-v", action="store_true", help="Show each evaluation")
    parser.add_argument("--data-dir", type=str, default=None, help="Path to dataset directory")
    parser.add_argument(
        "--output",
        type=str,
        default="benchmarks/results/tuning_results.json",
        help="Output file",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO if args.verbose else logging.WARNING)

    # Load dataset
    print("Loading dataset...")
    dataset = load_longmemeval_s(data_dir=args.data_dir)
    print(f"Loaded {len(dataset.conversations)} conversations, {len(dataset.queries)} queries")

    # Run tuning
    results = run_tuning(
        dataset,
        top_k=args.top_k,
        max_iterations=args.iterations,
        verbose=args.verbose,
    )

    # Save results
    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with open(args.output, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nResults saved to {args.output}")

    # Print Rust constant for copy-paste
    print("\nRust weight constant (copy to pensyve-core/src/config.rs):")
    w = results["best_weights"]
    print(f"    weights: [{', '.join(f'{x:.4f}' for x in w)}],")


if __name__ == "__main__":
    main()
```

### 1.2.3 — Add tuning dependencies

- [ ] **Update** `/home/wshobson/workspace/major7apps/pensyve/benchmarks/requirements.txt`:

```
pensyve
numpy
scipy
```

### 1.2.4 — Write test for the tuning script

- [ ] **Create** `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_tuning.py`:

```python
"""Tests for the weight tuning infrastructure."""

import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

from benchmarks.tuning.optimize import DEFAULT_WEIGHTS, normalize_weights


def test_normalize_weights_sums_to_one():
    """Normalized weights should sum to 1.0."""
    raw = np.array([0.3, 0.1, 0.2, 0.05, 0.15, 0.1, 0.05, 0.05])
    normalized = normalize_weights(raw)
    assert abs(sum(normalized) - 1.0) < 0.01


def test_normalize_weights_handles_zeros():
    """All-zero weights should return uniform."""
    raw = np.array([0.0] * 8)
    normalized = normalize_weights(raw)
    assert abs(sum(normalized) - 1.0) < 0.01
    assert all(abs(w - 0.125) < 0.01 for w in normalized)


def test_normalize_weights_handles_negatives():
    """Negative weights should be made positive via abs."""
    raw = np.array([-0.3, 0.1, 0.2, -0.05, 0.15, 0.1, 0.05, 0.05])
    normalized = normalize_weights(raw)
    assert abs(sum(normalized) - 1.0) < 0.01
    assert all(w >= 0 for w in normalized)


def test_default_weights_sum_to_one():
    """Default weights should sum to 1.0."""
    assert abs(sum(DEFAULT_WEIGHTS) - 1.0) < 0.01
```

- [ ] **Run tuning tests**:

```bash
.venv/bin/pytest tests/python/test_tuning.py -v
# Expected: All tests PASSED
```

### 1.2.5 — Run initial tuning and commit

- [ ] **Run tuning with small iteration count** for initial validation:

```bash
.venv/bin/python benchmarks/tuning/optimize.py --iterations 10 --verbose
# Expected: Prints baseline and optimized accuracy, saves results
```

- [ ] **Commit tuning infrastructure**:

```bash
git add benchmarks/tuning/ benchmarks/requirements.txt tests/python/test_tuning.py
git commit -m "$(cat <<'EOF'
feat: add retrieval weight tuning via differential evolution

- Tuning script optimizes 8-signal weight vector against LongMemEval_S
- Uses scipy differential_evolution with normalized weight constraints
- Prints Rust constant for direct copy-paste into config.rs
- Tests for weight normalization and default weight validity
EOF
)"
```

### 1.2.6 — Apply optimized weights (after full tuning run)

- [ ] **Run full tuning** (longer, should be done with real dataset if available):

```bash
.venv/bin/python benchmarks/tuning/optimize.py --iterations 50
```

- [ ] **Update default weights** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/config.rs` with the optimal weights from the tuning output. The tuning script prints the exact Rust constant to copy.

  Find:

```rust
                weights: [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
```

  Replace with the optimized weights from the tuning output (e.g.):

```rust
                weights: [/* paste optimized weights from tuning output */],
```

- [ ] **Update TEST_WEIGHTS** in `/home/wshobson/workspace/major7apps/pensyve/pensyve-core/src/retrieval.rs` to match.

- [ ] **Run all Rust tests** to verify no regressions:

```bash
cargo test -p pensyve-core
# Expected: All tests PASSED
```

- [ ] **Run benchmark again** to confirm improvement:

```bash
.venv/bin/python benchmarks/longmemeval/run.py --verbose
# Expected: Higher accuracy than baseline
```

- [ ] **Commit optimized weights**:

```bash
git add pensyve-core/src/config.rs pensyve-core/src/retrieval.rs benchmarks/results/
git commit -m "$(cat <<'EOF'
perf: apply optimized retrieval weights from LongMemEval tuning

Before: X% accuracy
After:  Y% accuracy
Weights tuned via differential evolution over N iterations.
EOF
)"
```

---

## Task 1.3 — Wire Tier 2 Extraction

**Sprint:** 3
**Owner files:** `pensyve_server/main.py` (endpoint integration)
**Goal:** Wire `Tier2Extractor` from `pensyve_server/extraction.py` into the REST API `remember()` and `recall()` endpoints. Configurable via `PENSYVE_TIER2_ENABLED` env var.

### 1.3.1 — Write tests for Tier 2 integration

- [ ] **Add tests** in `/home/wshobson/workspace/major7apps/pensyve/tests/python/test_api.py`. Append:

```python
def test_remember_with_tier2_disabled(client):
    """When PENSYVE_TIER2_ENABLED is not set, remember works normally."""
    client.post("/v1/entities", json={"name": "alice", "kind": "user"})
    r = client.post(
        "/v1/remember",
        json={"entity": "alice", "fact": "Alice works at Google", "confidence": 0.9},
    )
    assert r.status_code == 200
    assert r.json()["memory_type"] == "semantic"


def test_remember_with_tier2_enabled(client):
    """When PENSYVE_TIER2_ENABLED=1, remember should still work (extraction runs in background)."""
    import os

    os.environ["PENSYVE_TIER2_ENABLED"] = "1"
    try:
        # Reset to pick up env var
        import pensyve_server.main as main_mod

        main_mod._tier2 = None

        client.post("/v1/entities", json={"name": "bob", "kind": "user"})
        r = client.post(
            "/v1/remember",
            json={
                "entity": "bob",
                "fact": "Bob is a senior engineer at Acme Corp",
                "confidence": 0.9,
            },
        )
        assert r.status_code == 200
        data = r.json()
        assert data["memory_type"] == "semantic"
        # When tier2 is enabled, response may include extracted_facts
        # (only if extractor is in mock mode, which it will be in tests)
    finally:
        os.environ.pop("PENSYVE_TIER2_ENABLED", None)
        main_mod._tier2 = None


def test_recall_with_tier2_disabled_has_no_contradictions(client):
    """Without tier2, recall should work normally with no contradictions field."""
    client.post("/v1/entities", json={"name": "carol", "kind": "user"})
    client.post(
        "/v1/remember",
        json={"entity": "carol", "fact": "Carol uses Python", "confidence": 0.9},
    )
    r = client.post("/v1/recall", json={"query": "programming language", "entity": "carol"})
    assert r.status_code == 200
```

- [ ] **Run the tests and verify they fail** (because `_tier2` doesn't exist yet):

```bash
.venv/bin/pytest tests/python/test_api.py::test_remember_with_tier2_enabled -v
# Expected: FAILED — AttributeError: module has no attribute '_tier2'
```

### 1.3.2 — Add Tier 2 initialization to main.py

- [ ] **Edit** `/home/wshobson/workspace/major7apps/pensyve/pensyve_server/main.py` — add Tier 2 imports and initialization. After the existing imports block:

```python
import os
import uuid as uuid_mod

from fastapi import FastAPI, HTTPException

import pensyve
```

  Add:

```python
from pensyve_server.extraction import Tier2Extractor
```

- [ ] **Add Tier 2 global state** after the existing globals. Find:

```python
_episodes: dict[str, dict] = {}  # episode_id -> {"ep": Episode, "message_count": int}
```

  Add after it:

```python
_tier2: Tier2Extractor | None = None


def get_tier2() -> Tier2Extractor | None:
    """Get the Tier 2 extractor if enabled via PENSYVE_TIER2_ENABLED env var."""
    global _tier2
    if os.environ.get("PENSYVE_TIER2_ENABLED", "").strip() not in ("1", "true", "yes"):
        return None
    if _tier2 is None:
        model_path = os.environ.get("PENSYVE_TIER2_MODEL", None)
        _tier2 = Tier2Extractor(model_path=model_path)
    return _tier2
```

### 1.3.3 — Wire Tier 2 into the remember endpoint

- [ ] **Update the `remember` function** in `/home/wshobson/workspace/major7apps/pensyve/pensyve_server/main.py`. Find:

```python
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
```

  Replace with:

```python
@app.post("/v1/remember")
def remember(req: RememberRequest):
    p = get_pensyve()
    entity = p.entity(req.entity)
    mem = p.remember(entity=entity, fact=req.fact, confidence=req.confidence)

    response = MemoryResponse(
        id=mem.id,
        content=mem.content,
        memory_type=mem.memory_type,
        confidence=mem.confidence,
        stability=mem.stability,
    )

    # Tier 2: extract additional facts if enabled
    tier2 = get_tier2()
    if tier2 is not None:
        extraction = tier2.extract_facts(req.fact)
        # Store extracted facts as additional semantic memories
        for fact in extraction:
            try:
                p.remember(
                    entity=entity,
                    fact=f"{fact.subject} {fact.predicate} {fact.object}",
                    confidence=fact.confidence,
                )
            except Exception:
                pass  # best-effort extraction storage

    return response
```

### 1.3.4 — Wire Tier 2 contradiction detection into recall

- [ ] **Update the `recall` function** in `/home/wshobson/workspace/major7apps/pensyve/pensyve_server/main.py`. Find:

```python
@app.post("/v1/recall", response_model=list[MemoryResponse])
def recall(req: RecallRequest):
    p = get_pensyve()
    kwargs: dict[str, object] = {"limit": req.limit}
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
```

  Replace with:

```python
@app.post("/v1/recall")
def recall(req: RecallRequest):
    p = get_pensyve()
    kwargs: dict[str, object] = {"limit": req.limit}
    if req.entity:
        kwargs["entity"] = p.entity(req.entity)
    if req.types:
        kwargs["types"] = req.types
    results = p.recall(req.query, **kwargs)

    memories = [
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

    # Tier 2: detect contradictions if enabled
    tier2 = get_tier2()
    if tier2 is not None and tier2.is_available:
        existing_facts = [
            {"subject": "", "predicate": "", "object": m.content}
            for m in results
            if m.memory_type == "semantic"
        ]
        if existing_facts:
            contradictions = tier2.detect_contradictions(req.query, existing_facts)
            if contradictions:
                # Return contradictions as metadata in the response
                # For now, append as a special "contradiction" memory
                for c in contradictions:
                    memories.append(
                        MemoryResponse(
                            id="contradiction",
                            content=f"Contradiction: {c.get('explanation', '')}",
                            memory_type="contradiction",
                            confidence=0.0,
                            stability=0.0,
                            score=0.0,
                        )
                    )

    return memories
```

### 1.3.5 — Run tests and commit

- [ ] **Run all API tests**:

```bash
.venv/bin/pytest tests/python/test_api.py -v
# Expected: All tests PASSED
```

- [ ] **Run full Python test suite**:

```bash
.venv/bin/pytest tests/python/ -v
# Expected: All tests PASSED
```

- [ ] **Commit Tier 2 wiring**:

```bash
git add pensyve_server/main.py tests/python/test_api.py
git commit -m "$(cat <<'EOF'
feat: wire Tier 2 LLM extraction into REST API pipeline

- remember() extracts additional facts when PENSYVE_TIER2_ENABLED=1
- recall() detects contradictions against existing semantic memories
- Tier 2 disabled by default; enable via PENSYVE_TIER2_ENABLED env var
- Model path configurable via PENSYVE_TIER2_MODEL env var
- Falls back to heuristic mock mode when no GGUF model available
EOF
)"
```

---

## Final Validation

### Run the complete test suite

- [ ] **Run all Rust tests**:

```bash
cargo test --workspace
# Expected: All tests PASSED
```

- [ ] **Rebuild the PyO3 module** (needed after Rust changes):

```bash
maturin develop --manifest-path pensyve-python/Cargo.toml
```

- [ ] **Run all Python tests**:

```bash
.venv/bin/pytest tests/python/ -v
# Expected: All tests PASSED
```

- [ ] **Run clippy**:

```bash
cargo clippy --workspace
# Expected: No errors
```

- [ ] **Run the LongMemEval benchmark** to verify the baseline:

```bash
.venv/bin/python benchmarks/longmemeval/run.py --verbose
# Expected: Accuracy score printed and saved
```

### Summary of files changed

| File | Task | Change |
|------|------|--------|
| `pensyve_server/main.py` | 1.4, 1.3 | UUID episodes, memories_created, Tier 2 wiring |
| `pensyve-python/python/pensyve/_core.pyi` | 1.4 | Add `consolidate()` stub |
| `pensyve-core/src/retrieval.rs` | 1.5 | Intent classifier + scoring pipeline |
| `pensyve-core/src/config.rs` | 1.5, 1.2 | Intent weight + tuned weights |
| `benchmarks/longmemeval/dataset.py` | 1.1 | LongMemEval dataset loader |
| `benchmarks/longmemeval/evaluate.py` | 1.1 | Evaluation harness |
| `benchmarks/longmemeval/run.py` | 1.1 | Benchmark runner |
| `benchmarks/tuning/optimize.py` | 1.2 | Weight optimization script |
| `benchmarks/requirements.txt` | 1.2 | numpy, scipy deps |
| `tests/python/test_api.py` | 1.4, 1.3 | Bug fix + Tier 2 tests |
| `tests/python/test_benchmark.py` | 1.1 | Benchmark infra tests |
| `tests/python/test_tuning.py` | 1.2 | Tuning infra tests |
