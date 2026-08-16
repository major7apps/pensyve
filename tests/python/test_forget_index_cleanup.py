"""`Pensyve.forget` must clear every row shape the entity-wide delete removes (#261).

The delete matches episodic rows on `about_entity OR source_entity` and
semantic rows on `subject OR object_entity`, superseded rows included. The
binding used to hand its vector-index cleanup nothing at all, and the gateway
handlers assembled it from `list_episodic_by_entity` / `list_semantic_by_entity`,
which see only the about-side, the subject-side and live rows. Both now collect
through `list_memories_by_entity_including_superseded`, whose contract is that
its result equals the delete's.

Scope note for reviewers: the binding exposes no vector-index introspection,
and stale index entries are invisible through `recall` — the retrieval engine
hydrates every vector hit from storage and silently drops the misses. So this
test pins the half Python can observe: every deletable row shape leaves storage
and stops coming back from recall, and unrelated rows survive. The index half of
#261 is pinned in Rust, by
`pensyve-core/src/storage/sqlite.rs::test_list_memories_by_entity_including_superseded_matches_the_delete_scope`
and `pensyve-mcp-gateway/tests/test_rest_forget_index_cleanup.rs`.

Object-side semantic and superseded rows are not reachable through the Python
API (no `object_entity` argument, no supersede call), so the shapes seeded here
are the ones the binding can actually produce.
"""

import tempfile

import pensyve

ALICE_EPISODIC_SOURCE = "the marmalade recipe uses seville oranges"
ALICE_EPISODIC_ABOUT = "the kayak was stored in the garage rafters"
ALICE_SEMANTIC = "prefers oat milk in coffee"
BOB_EPISODIC = "the telescope needs recollimating before winter"
BOB_SEMANTIC = "runs the observatory on tuesdays"

ALICE_MARKERS = (ALICE_EPISODIC_SOURCE, ALICE_EPISODIC_ABOUT, ALICE_SEMANTIC)
BOB_MARKERS = (BOB_EPISODIC, BOB_SEMANTIC)


def _contents(p, query):
    """Every memory content string recall surfaces for `query`."""
    return [m.content for m in p.recall(query, limit=20)]


def _seen(p, marker):
    """True when `marker` still comes back from recall."""
    # Query with the marker itself so both the lexical and the vector list
    # rank it first if it is still there.
    return any(marker in content for content in _contents(p, marker))


def test_forget_clears_every_reachable_row_shape():
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        alice = p.entity("alice", kind="user")
        bob = p.entity("bob", kind="agent")
        carol = p.entity("carol", kind="user")

        # Source-side episodic: the first participant is the source, so this
        # row names alice as `source_entity` and bob as `about_entity`.
        with p.episode(alice, bob) as ep:
            ep.message("user", ALICE_EPISODIC_SOURCE)

        # About-side episodic: participants reversed, so alice is
        # `about_entity` here.
        with p.episode(bob, alice) as ep:
            ep.message("agent", ALICE_EPISODIC_ABOUT)

        # Subject-side semantic.
        p.remember(alice, ALICE_SEMANTIC)

        # Controls: nothing to do with alice, so nothing here may be touched.
        with p.episode(bob, carol) as ep:
            ep.message("agent", BOB_EPISODIC)
        p.remember(bob, BOB_SEMANTIC)

        for marker in ALICE_MARKERS + BOB_MARKERS:
            assert _seen(p, marker), (
                f"{marker!r} must be recallable before the forget, "
                "or its absence afterwards proves nothing"
            )

        result = p.forget(alice)

        assert result["forgotten_count"] == len(ALICE_MARKERS), (
            "the forget must report every row shape it deleted, source-side "
            f"episodic included; got {result['forgotten_count']}"
        )

        for marker in ALICE_MARKERS:
            assert not _seen(p, marker), (
                f"{marker!r} was reported deleted but recall still returns it"
            )
        for marker in BOB_MARKERS:
            assert _seen(p, marker), f"{marker!r} is unrelated to alice and must survive the forget"

        stats = p.stats()
        assert stats["episodic"] == 1, (
            f"only bob's episodic row may remain; got {stats['episodic']}"
        )
        assert stats["semantic"] == 1, (
            f"only bob's semantic row may remain; got {stats['semantic']}"
        )
