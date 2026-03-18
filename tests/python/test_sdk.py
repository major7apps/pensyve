"""Tests for the Pensyve Python SDK."""

import tempfile

import pensyve


def test_version():
    assert pensyve.__version__ == "0.1.0"


def test_create_instance():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        assert p is not None


def test_create_instance_with_namespace():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d, namespace="test-ns")
        assert p is not None


def test_entity_creation():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        assert agent.name == "bot"
        assert agent.kind == "agent"
        assert agent.id  # non-empty UUID string


def test_entity_kinds():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        for kind in ["agent", "user", "team", "tool"]:
            e = p.entity(f"test-{kind}", kind=kind)
            assert e.kind == kind


def test_entity_default_kind():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        e = p.entity("alice")
        assert e.kind == "user"


def test_entity_idempotent():
    """Getting the same entity twice should return the same id."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        e1 = p.entity("bob", kind="user")
        e2 = p.entity("bob", kind="user")
        assert e1.id == e2.id
        assert e1.name == e2.name


def test_entity_repr():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        r = repr(agent)
        assert "bot" in r
        assert "agent" in r


def test_episode_and_recall():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")
        with p.episode(agent, user) as ep:
            ep.message("user", "I prefer dark mode and use vim")
        results = p.recall("dark mode preference", entity=user)
        assert len(results) > 0
        assert all(isinstance(m, pensyve.Memory) for m in results)


def test_episode_multiple_messages():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")
        with p.episode(agent, user) as ep:
            ep.message("user", "I love Python programming")
            ep.message("assistant", "That's great! Python is very popular.")
            ep.message("user", "I also enjoy Rust")
        results = p.recall("Python programming", entity=user)
        assert len(results) > 0


def test_episode_with_outcome():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("alice", kind="user")
        with p.episode(agent, user) as ep:
            ep.message("user", "Help me deploy my app")
            ep.outcome("success")
        results = p.recall("deploy app")
        assert len(results) > 0


def test_remember_and_recall():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("seth", kind="user")
        mem = p.remember(entity=user, fact="Seth prefers Python", confidence=0.95)
        assert mem.memory_type == "semantic"
        assert abs(mem.confidence - 0.95) < 0.01
        results = p.recall("what language", entity=user)
        assert len(results) > 0


def test_remember_default_confidence():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("alice", kind="user")
        mem = p.remember(entity=user, fact="Alice likes tea")
        assert abs(mem.confidence - 0.8) < 0.01


def test_forget():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("seth", kind="user")
        p.remember(entity=user, fact="test fact", confidence=0.9)
        result = p.forget(entity=user)
        assert result["forgotten_count"] >= 1


def test_forget_no_memories():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("empty-user", kind="user")
        result = p.forget(entity=user)
        assert result["forgotten_count"] == 0


def test_memory_properties():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("alice", kind="user")
        mem = p.remember(entity=user, fact="Alice uses VS Code", confidence=0.85)
        assert mem.id  # non-empty
        assert mem.content  # non-empty
        assert mem.memory_type == "semantic"
        assert abs(mem.confidence - 0.85) < 0.01
        assert mem.stability >= 0.0


def test_memory_repr():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("alice", kind="user")
        mem = p.remember(entity=user, fact="Alice uses VS Code")
        r = repr(mem)
        assert "semantic" in r


def test_recall_with_type_filter():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("alice", kind="user")

        # Create episodic memory.
        with p.episode(agent, user) as ep:
            ep.message("user", "I love hiking in the mountains")

        # Create semantic memory.
        p.remember(entity=user, fact="Alice prefers hiking")

        # Filter by type.
        episodic_only = p.recall("hiking", types=["episodic"])
        for m in episodic_only:
            assert m.memory_type == "episodic"

        semantic_only = p.recall("hiking", types=["semantic"])
        for m in semantic_only:
            assert m.memory_type == "semantic"


def test_recall_limit():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("alice", kind="user")
        for i in range(10):
            p.remember(entity=user, fact=f"Fact number {i} about alice")
        results = p.recall("fact about alice", limit=3)
        assert len(results) <= 3


def test_consolidate_returns_dict():
    """consolidate() should return a dict with promoted, decayed, archived keys."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        result = p.consolidate()
        assert isinstance(result, dict)
        assert "promoted" in result
        assert "decayed" in result
        assert "archived" in result
        assert result["promoted"] >= 0
        assert result["decayed"] >= 0
        assert result["archived"] >= 0


def test_consolidate_promotes_repeated_facts():
    """Repeated episodic memories about the same entity should be promoted."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("carol", kind="user")

        # Record the same message in 3 separate episodes.
        for _ in range(3):
            with p.episode(agent, user) as ep:
                ep.message("user", "I prefer dark mode")

        result = p.consolidate()
        # The consolidation engine should have promoted at least one semantic memory.
        assert result["promoted"] >= 1


def test_consolidate_no_op_on_empty_namespace():
    """Consolidation on an empty namespace should return all zeros."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d, namespace="empty-ns")
        result = p.consolidate()
        assert result["promoted"] == 0
        assert result["decayed"] == 0
        assert result["archived"] == 0
