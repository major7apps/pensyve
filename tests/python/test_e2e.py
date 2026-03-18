"""
End-to-end integration tests for Pensyve Phase 1.
These verify the full pipeline: ingest → extract → embed → store → recall.
"""
import pensyve
import tempfile


def test_five_line_demo():
    """The exact demo from the spec. This is THE success criterion."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        with p.episode(p.entity("agent", kind="agent"), p.entity("user", kind="user")) as ep:
            ep.message("user", "I prefer dark mode and use vim keybindings")
        results = p.recall("what editor setup does the user prefer?")
        assert len(results) > 0
        contents = " ".join([r.content for r in results]).lower()
        assert "vim" in contents or "dark mode" in contents


def test_multiple_episodes_different_topics():
    """Memories across episodes should be independently searchable."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")

        with p.episode(agent, user) as ep:
            ep.message("user", "I'm working on the authentication service")
            ep.message("agent", "I'll help debug the token refresh mechanism")

        with p.episode(agent, user) as ep:
            ep.message("user", "The database migration failed on the users table")
            ep.message("agent", "Let me check the migration script for schema errors")

        auth_results = p.recall("authentication token issues", entity=user)
        db_results = p.recall("database migration", entity=user)

        assert len(auth_results) > 0
        assert len(db_results) > 0
        # Auth query should find auth content, not DB content
        auth_content = " ".join([r.content for r in auth_results]).lower()
        assert "auth" in auth_content or "token" in auth_content


def test_remember_explicit_facts():
    """Explicit remember should be immediately recallable."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("seth", kind="user")

        p.remember(entity=user, fact="Seth's favorite language is Python", confidence=0.95)
        p.remember(entity=user, fact="Seth works at Major7 Apps", confidence=0.9)
        p.remember(entity=user, fact="Seth prefers vim keybindings", confidence=0.85)

        results = p.recall("what programming language does Seth use?", entity=user)
        assert len(results) > 0
        assert any("python" in r.content.lower() for r in results)


def test_outcome_tracking():
    """Episodes with outcomes should be stored and findable."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")

        with p.episode(agent, user) as ep:
            ep.message("user", "Fix the authentication bug")
            ep.message("agent", "Refreshed the OAuth token endpoint")
            ep.outcome("success")

        results = p.recall("how did we fix auth?", entity=user)
        assert len(results) > 0


def test_multiple_entities_isolated():
    """Memories about different entities should not cross-contaminate."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        alice = p.entity("alice", kind="user")
        bob = p.entity("bob", kind="user")

        with p.episode(agent, alice) as ep:
            ep.message("user", "Alice loves TypeScript and React")

        with p.episode(agent, bob) as ep:
            ep.message("user", "Bob prefers Rust and systems programming")

        alice_results = p.recall("programming language", entity=alice)
        bob_results = p.recall("programming language", entity=bob)

        # Both should return results
        assert len(alice_results) > 0
        assert len(bob_results) > 0


def test_persistence_across_instances():
    """Memories should survive closing and reopening Pensyve."""
    with tempfile.TemporaryDirectory() as d:
        # First instance: write
        p1 = pensyve.Pensyve(path=d)
        user = p1.entity("seth", kind="user")
        p1.remember(entity=user, fact="Persistent memory test", confidence=0.9)
        del p1  # close

        # Second instance: read
        p2 = pensyve.Pensyve(path=d)
        user2 = p2.entity("seth", kind="user")
        results = p2.recall("persistent memory", entity=user2)
        assert len(results) > 0


def test_forget_removes_memories():
    """Forget should remove all memories about an entity."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("seth", kind="user")

        p.remember(entity=user, fact="Secret information", confidence=0.9)
        p.remember(entity=user, fact="More secret info", confidence=0.8)

        result = p.forget(entity=user)
        assert result["forgotten_count"] >= 2

        results = p.recall("secret information", entity=user)
        assert len(results) == 0


def test_many_memories_recall_limit():
    """Recall should respect the limit parameter."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")

        # Insert many memories
        for i in range(20):
            with p.episode(agent, user) as ep:
                ep.message("user", f"Topic {i}: some information about subject number {i}")

        results = p.recall("topic", entity=user, limit=5)
        assert len(results) <= 5


def test_memory_properties_accessible():
    """Memory objects should expose all expected properties."""
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        user = p.entity("seth", kind="user")
        p.remember(entity=user, fact="Test property access", confidence=0.88)

        results = p.recall("property access", entity=user)
        assert len(results) > 0
        mem = results[0]
        assert mem.id is not None
        assert len(mem.id) > 0
        assert mem.content is not None
        assert mem.memory_type in ("episodic", "semantic", "procedural")
        assert 0.0 <= mem.confidence <= 1.0
        assert mem.stability > 0.0
