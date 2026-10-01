"""`Pensyve.forget` must write a pre-delete snapshot and fail closed (#284).

The binding used to call the storage delete directly with a persist step that
did nothing, so it was the one surface that destroyed memories and left no
recovery artifact. It now goes through the same snapshot path as the MCP
server and the REST gateway: the rows are written to a file before they are
deleted, in the delete's transaction, and nothing is deleted when that write
fails.

The root is resolved the way the serving surfaces resolve theirs:
`<storage path>/snapshots`, or `PENSYVE_SNAPSHOT_DIR`.
"""

import os
import tempfile

import pytest

import pensyve

FACT = "prefers oat milk in coffee"


def _recalled(p, entity):
    return [m.content for m in p.recall(FACT, entity=entity, limit=20)]


def test_forget_writes_a_snapshot_under_the_storage_root(monkeypatch):
    monkeypatch.delenv("PENSYVE_SNAPSHOT_DIR", raising=False)
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        alice = p.entity("alice", kind="user")
        p.remember(entity=alice, fact=FACT)

        result = p.forget(alice)

        assert result["forgotten_count"] == 1
        snapshot_path = result["snapshot_path"]
        assert os.path.isfile(snapshot_path)
        assert os.path.getsize(snapshot_path) > 0
        assert os.path.commonpath([snapshot_path, os.path.join(d, "snapshots")]) == os.path.join(
            d, "snapshots"
        ), "the snapshot must land under <storage path>/snapshots"
        with open(snapshot_path, encoding="utf-8") as snapshot:
            assert FACT in snapshot.read(), "the snapshot must hold what was deleted"
        assert _recalled(p, alice) == []


def test_forget_honours_pensyve_snapshot_dir(monkeypatch):
    with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as elsewhere:
        monkeypatch.setenv("PENSYVE_SNAPSHOT_DIR", elsewhere)
        p = pensyve.Pensyve(path=d)
        alice = p.entity("alice", kind="user")
        p.remember(entity=alice, fact=FACT)

        result = p.forget(alice)

        assert os.path.commonpath([result["snapshot_path"], elsewhere]) == elsewhere
        assert not os.path.exists(os.path.join(d, "snapshots"))


def test_forget_deletes_nothing_when_the_snapshot_cannot_be_written(monkeypatch):
    with tempfile.TemporaryDirectory() as d:
        # A regular file where the snapshot root's parent would have to be a
        # directory: nothing can be created beneath it, whoever runs the test.
        blocker = os.path.join(d, "blocker")
        with open(blocker, "w", encoding="utf-8") as f:
            f.write("not a directory")
        monkeypatch.setenv("PENSYVE_SNAPSHOT_DIR", os.path.join(blocker, "snapshots"))
        p = pensyve.Pensyve(path=d)
        alice = p.entity("alice", kind="user")
        p.remember(entity=alice, fact=FACT)

        with pytest.raises(RuntimeError, match="nothing was deleted"):
            p.forget(alice)

        assert any(FACT in content for content in _recalled(p, alice)), (
            "a forget that could not write its snapshot must leave the memory in place"
        )


def test_forget_with_nothing_to_delete_writes_no_snapshot(monkeypatch):
    monkeypatch.delenv("PENSYVE_SNAPSHOT_DIR", raising=False)
    with tempfile.TemporaryDirectory() as d:
        p = pensyve.Pensyve(path=d)
        nobody = p.entity("nobody", kind="user")

        result = p.forget(nobody)

        assert result["forgotten_count"] == 0
        assert "snapshot_path" not in result
