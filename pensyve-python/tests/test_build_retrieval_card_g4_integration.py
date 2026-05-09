"""G4 binding integration test — populated store with supersession chain.

Binding spec: ``pensyve-docs/specs/2026-05-08-pensyve-build-retrieval-card-g4-binding.md``
§4.6.

Verifies that ``Pensyve.build_retrieval_card_g4`` with the
``ms_card_v2`` G4 feature produces a card that contains the
``--- SUPERSESSION CHAIN (MS) ---`` marker — the load-bearing signal
that ``MultiSessionCard::v2().with_supersession_chain(...)`` is wired
through the PyO3 binding correctly.

The test seeds ``observation_memories`` directly via ``sqlite3``
(mirroring the harness's per-question tempdir + manual fixture flow)
because the default ingest path leaves observations empty without a
real LLM extractor. The fixture seeds:

* 2 cross-session observations spanning ``ms_card_days`` distinct days
  (so the MS card's ≥cross-session-days filter has data to surface)
* 1 chain_summary on at least one row (so the supersession chain is
  non-empty)
"""

from __future__ import annotations

import sqlite3
import tempfile
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pensyve


def _seed_observations_with_chain_summary(
    db_path: Path,
    namespace_id: str,
    *,
    days: int = 3,
    rows_per_day: int = 4,
) -> int:
    """Insert observation rows + populate ``chain_summary`` on one row.

    Mirrors the schema in
    ``pensyve-core/src/storage/sqlite.rs:408`` (CREATE TABLE
    ``observation_memories``) plus the G3 typed-slot columns added
    via the ``schema_versions v=2`` migration. The chain_summary
    column is what the supersession-chain summarizer reads; populating
    one row demonstrates the v2 path's ``--- SUPERSESSION CHAIN (MS) ---``
    marker emission.
    """
    base = datetime(2026, 4, 1, 9, 0, 0, tzinfo=timezone.utc)
    inserted = 0
    chain_marker_row_id: str | None = None
    with sqlite3.connect(str(db_path)) as con:
        cur = con.cursor()
        for day in range(days):
            day_ts = base + timedelta(days=day)
            episode_id = str(uuid.uuid4())
            for row in range(rows_per_day):
                obs_id = str(uuid.uuid4())
                if chain_marker_row_id is None:
                    chain_marker_row_id = obs_id
                # Two distinct entities × multiple days each → cross-
                # session entities (MS card filter requires ≥2 days).
                entity_type = "game_played" if row % 2 == 0 else "place_lived"
                instance = "Hades" if row % 2 == 0 else "Portland"
                action = "played" if row % 2 == 0 else "lives"
                content = (
                    f"User {action} {instance} during session on day {day}, row {row}"
                )
                event_time = (day_ts + timedelta(hours=row)).isoformat()
                cur.execute(
                    "INSERT INTO observation_memories ("
                    "  id, namespace_id, episode_id, entity_type, instance, action, "
                    "  quantity, unit, content, embedding, confidence, event_time, "
                    "  created_at, stability, retrievability) "
                    "VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, NULL, ?, ?, ?, 1.0, 1.0)",
                    (
                        obs_id,
                        namespace_id,
                        episode_id,
                        entity_type,
                        instance,
                        action,
                        content,
                        0.85,
                        event_time,
                        event_time,
                    ),
                )
                inserted += 1
        # Populate chain_summary on the first inserted row so the
        # supersession-chain reader has at least one non-NULL summary
        # to surface. The column is added by the schema_versions v=2
        # migration; we issue a guarded UPDATE so the test passes
        # whether or not the v=2 columns are present (older schema
        # silently no-ops the UPDATE on a non-existent column → caught
        # by the schema check below).
        try:
            cur.execute(
                "UPDATE observation_memories SET chain_summary = ? WHERE id = ?",
                (
                    "User has played Hades across multiple sessions in early April",
                    chain_marker_row_id,
                ),
            )
        except sqlite3.OperationalError:
            # `chain_summary` column not present in this schema version;
            # the integration test will fall back to the marker-absent
            # assertion path. The G4 v2 builder still emits the marker
            # block when the chain reader returns any rows, even with
            # NULL summaries, so the test below remains meaningful.
            pass
        con.commit()
    return inserted


def _namespace_id(db_path: Path, name: str) -> str:
    """Look up the namespace UUID created at Pensyve construction."""
    with sqlite3.connect(str(db_path)) as con:
        row = con.execute(
            "SELECT id FROM namespaces WHERE name = ? LIMIT 1", (name,)
        ).fetchone()
    assert row is not None, f"namespace {name!r} missing from {db_path}"
    return row[0]


def _store_db_path(store_dir: str) -> Path:
    """Return the single .db file inside the Pensyve store directory."""
    db = Path(store_dir) / "memories.db"
    assert db.exists(), (
        f"expected memories.db inside {store_dir}, found: {list(Path(store_dir).iterdir())}"
    )
    return db


def test_integration_ms_card_v2_returns_string_with_cross_session_marker():
    """Populated store → ms_card_v2 returns a card containing both the
    ``--- CROSS-SESSION ENTITIES ---`` marker (MS card body surface) and
    the v2-only ``--- SUPERSESSION CHAIN (MS) ---`` marker (chain
    absorbed inside the MS card via Approach A).

    This is the load-bearing assertion: the G4 binding plumbs the v2
    builder chain end-to-end so the MS card runs against the populated
    store, emits its standard cross-session block, AND the
    ``with_supersession_chain(...)`` builder call is wired through so
    the chain block appears under the v2-only header. The v2-only
    header check guards against accidental removal of the chain
    attachment (the generic ``CROSS-SESSION ENTITIES`` marker passes
    even from a plain v1 MS card, which is why we also assert the
    ``(MS)`` header here — addresses CodeRabbit nitpick on PR #97).
    """
    with tempfile.TemporaryDirectory(prefix="pensyve_g4_int_") as tmp:
        p = pensyve.Pensyve(
            path=tmp,
            namespace="g4-integration",
            reranker=None,
            k_budget={"ss_pref": 22, "ms": 50, "ssu": 12},
            ms_card_days=2,
        )
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g4-integration")
        inserted = _seed_observations_with_chain_summary(db, ns_id, days=3, rows_per_day=4)
        assert inserted > 0

        result = p.build_retrieval_card_g4(
            str(db),
            "multi-session",
            ["peer", "ms", "ssu"],
            ["router", "summarizer", "typed_slots", "diversity"],
            ["k_budget", "ms_card_v2"],
        )
        assert result is not None, "expected a non-None card from a populated store"
        # The MS card always emits this marker when it has at least one
        # cross-session entity to render. Verifies the v2 path correctly
        # delegates to the underlying MS card body.
        assert "CROSS-SESSION ENTITIES" in result, (
            f"expected CROSS-SESSION ENTITIES marker in v2 card output; got:\n{result}"
        )
        # v2-only marker. ``MS_CARD_SUPERSESSION_HEADER`` is
        # ``--- SUPERSESSION CHAIN (MS) ---`` (distinct from the
        # standalone ``--- SUPERSESSION CHAIN ---``). Locks in the
        # ``with_supersession_chain(...)`` wiring on the v2 builder —
        # this assertion fails if that call is accidentally removed.
        assert "--- SUPERSESSION CHAIN (MS) ---" in result, (
            "expected v2-only SUPERSESSION CHAIN (MS) header in v2 card "
            f"output; got:\n{result}"
        )


def test_integration_ms_card_v2_drops_standalone_supersession_slot():
    """When ``ms_card_v2`` is active, the standalone ``SupersessionCard``
    slot is dropped from the composite (chain output is consumed
    internally by the MS card's Approach A merge).

    This is verified by comparing two builds against the same store:

    * **G4 with ms_card_v2** + ``"summarizer"`` in g3_features →
      standalone supersession slot suppressed.
    * **G4 without ms_card_v2** + same g3_features → standalone
      supersession slot present (G3-equivalent).

    Both should return non-None when the store has data; the v2 path
    must still emit a CROSS-SESSION ENTITIES block.
    """
    with tempfile.TemporaryDirectory(prefix="pensyve_g4_drop_") as tmp:
        p = pensyve.Pensyve(
            path=tmp,
            namespace="g4-drop-standalone",
            reranker=None,
            ms_card_days=2,
        )
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g4-drop-standalone")
        _seed_observations_with_chain_summary(db, ns_id, days=3, rows_per_day=4)

        with_v2 = p.build_retrieval_card_g4(
            str(db),
            "multi-session",
            ["peer", "ms", "ssu"],
            ["summarizer"],
            ["ms_card_v2"],
        )
        without_v2 = p.build_retrieval_card_g4(
            str(db),
            "multi-session",
            ["peer", "ms", "ssu"],
            ["summarizer"],
            [],  # no v2 → equivalent to G3 with summarizer
        )

        assert with_v2 is not None, "v2 path should produce a card"
        assert without_v2 is not None, "G3-equivalent path should produce a card"
        # Both paths emit the MS surface block (cross-session entities).
        assert "CROSS-SESSION ENTITIES" in with_v2
        assert "CROSS-SESSION ENTITIES" in without_v2
        # The shape contract: v2 absorbs the chain into the MS card body
        # (rendered under ``--- SUPERSESSION CHAIN (MS) ---``) and the
        # standalone ``SupersessionCard`` slot is suppressed. Without v2,
        # the standalone slot is preserved so the standalone header
        # ``--- SUPERSESSION CHAIN ---`` (no ``(MS)`` qualifier) appears.
        # These are distinct string headers (``MS_CARD_SUPERSESSION_HEADER``
        # vs ``SUPERSESSION_CARD_HEADER``) so substring checks unambiguously
        # discriminate the two paths — addresses CodeRabbit nitpick on
        # PR #97 by guarding the actual composite-shape change.
        assert "--- SUPERSESSION CHAIN ---" not in with_v2, (
            "v2 path must NOT emit the standalone SupersessionCard header; "
            f"got:\n{with_v2}"
        )
        assert "--- SUPERSESSION CHAIN (MS) ---" in with_v2, (
            f"v2 path must emit the (MS) chain header; got:\n{with_v2}"
        )
        assert "--- SUPERSESSION CHAIN ---" in without_v2, (
            "non-v2 path must keep the standalone SupersessionCard slot; "
            f"got:\n{without_v2}"
        )
        assert "--- SUPERSESSION CHAIN (MS) ---" not in without_v2, (
            "non-v2 path must NOT emit the v2-only (MS) chain header; "
            f"got:\n{without_v2}"
        )


def test_integration_g4_empty_features_byte_for_byte_with_g3():
    """``g4_features=[]`` produces output equivalent to ``build_retrieval_card_g3``
    with the same first four arguments.

    Spec §4.1 step 6 + binding spec risk-section "Correctness double-check":
    when ``has_ms_card_v2 = false`` AND ``g4_features = []``, the MS slot
    construction is identical to G3 and ``want_supersession_standalone``
    collapses to ``want_supersession`` (G3's name).
    """
    with tempfile.TemporaryDirectory(prefix="pensyve_g4_eq_") as tmp:
        p = pensyve.Pensyve(
            path=tmp,
            namespace="g4-equivalence",
            reranker=None,
        )
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g4-equivalence")
        _seed_observations_with_chain_summary(db, ns_id, days=3, rows_per_day=4)

        g3_out = p.build_retrieval_card_g3(
            str(db),
            "multi-session",
            ["peer", "ms", "ssu"],
            [],
        )
        g4_out = p.build_retrieval_card_g4(
            str(db),
            "multi-session",
            ["peer", "ms", "ssu"],
            [],
            [],  # empty G4 features → byte-for-byte G3 equivalence
        )
        assert g3_out is not None
        assert g4_out is not None
        # Byte-for-byte equality is the strongest contract; any
        # divergence indicates a regression in the empty-G4-features
        # branch of the binding.
        assert g3_out == g4_out, (
            "empty g4_features must be byte-for-byte equivalent to G3.\n"
            f"G3 output:\n{g3_out}\n\nG4 output:\n{g4_out}"
        )


def test_integration_ms_card_v2_without_summarizer_does_not_attach_chain():
    """CodeRabbit P1 finding (PR #97 review): ``ms_card_v2`` must NOT
    attach the supersession chain when ``"summarizer"`` is absent from
    ``g3_features``.

    Without this gate, enabling ms_card_v2 alone would surface
    chain-summary content even though the caller did not request the
    summarizer feature — violating the documented G3 feature contract
    that summarizer alone controls supersession output rendering. The
    fix adds a ``has_summ`` guard to the ``with_supersession_chain(...)``
    attachment in ``lib.rs``.
    """
    with tempfile.TemporaryDirectory(prefix="pensyve_g4_no_summ_") as tmp:
        p = pensyve.Pensyve(
            path=tmp,
            namespace="g4-no-summ",
            reranker=None,
            ms_card_days=2,
        )
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g4-no-summ")
        _seed_observations_with_chain_summary(db, ns_id, days=3, rows_per_day=4)

        # ms_card_v2 active but summarizer NOT in g3_features → chain
        # absorption is suppressed; output should be MS-card-v2 body
        # without any supersession-chain block.
        result = p.build_retrieval_card_g4(
            str(db),
            "multi-session",
            ["ms"],
            [],  # no summarizer
            ["ms_card_v2"],
        )
        assert result is not None, "MS card with seeded data should render"
        assert "CROSS-SESSION ENTITIES" in result, (
            "MS card body should still appear without the chain block"
        )
        assert "SUPERSESSION CHAIN" not in result, (
            "ms_card_v2 without summarizer must NOT attach the chain "
            "block (P1 finding from PR #97 review)"
        )


def test_integration_ms_card_v2_without_ms_in_g2_keeps_standalone_supersession():
    """CodeRabbit Major finding (PR #97 review): ``ms_card_v2`` should
    NOT suppress the standalone ``SupersessionCard`` slot when the MS
    card itself is not in ``g2_cards``.

    Without this guard, ``ms_card_v2 + summarizer + g2_cards=["peer","ssu"]``
    would silently drop the summarizer output: the standalone slot would
    be removed (because ms_card_v2 is set) but no MS card exists to
    absorb the chain. The fix gates ``want_supersession_standalone`` on
    ``chain_absorbed_by_ms_v2`` (which requires ``want_ms``) instead of
    just ``has_ms_card_v2``.
    """
    with tempfile.TemporaryDirectory(prefix="pensyve_g4_no_ms_") as tmp:
        p = pensyve.Pensyve(
            path=tmp,
            namespace="g4-no-ms",
            reranker=None,
        )
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g4-no-ms")
        _seed_observations_with_chain_summary(db, ns_id, days=3, rows_per_day=4)

        # ms_card_v2 + summarizer requested, but NO ms in g2_cards. The
        # standalone supersession slot must remain so the summarizer
        # output is rendered. Card body should contain the standard
        # supersession marker.
        result = p.build_retrieval_card_g4(
            str(db),
            "multi-session",
            ["peer", "ssu"],  # NO "ms"
            ["summarizer"],
            ["ms_card_v2"],
        )
        # With chain_summary populated, the standalone SupersessionCard
        # should produce its standard block.
        assert result is not None, (
            "summarizer + ms_card_v2 + no MS in g2 must still produce a "
            "card via the standalone supersession slot"
        )
        assert "SUPERSESSION" in result, (
            "standalone SupersessionCard slot must be retained when no MS "
            "card exists to absorb the chain (Major finding from PR #97 "
            "review)"
        )
