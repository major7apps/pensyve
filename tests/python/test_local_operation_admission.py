"""Concurrency regression for the embedded local runtime.

A blocked extractor round trip must neither hold the GIL nor hold the local
operation permit that recall and remember wait on.

Requires the current PyO3 extension to be installed with ``maturin develop``.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

pensyve = pytest.importorskip("pensyve")


def test_blocked_extraction_releases_gil_and_local_permit(tmp_path: Path) -> None:
    request_entered = threading.Event()
    release_request = threading.Event()

    class BlockingExtractorHandler(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802 - stdlib callback name
            content_length = int(self.headers.get("Content-Length", "0"))
            self.rfile.read(content_length)
            request_entered.set()
            release_request.wait(timeout=60)
            body = json.dumps({"choices": [{"message": {"content": "[]"}}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, _format: str, *_args: object) -> None:
            return

    server = ThreadingHTTPServer(("127.0.0.1", 0), BlockingExtractorHandler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()

    episode_done = threading.Event()
    recall_done = threading.Event()
    progress = threading.Event()
    episode_thread: threading.Thread | None = None
    recall_thread: threading.Thread | None = None
    progress_thread: threading.Thread | None = None
    try:
        base_url = f"http://127.0.0.1:{server.server_port}"
        handle = pensyve.Pensyve(
            path=str(tmp_path / "runtime"),
            namespace="gil-release",
            extractor="local-llm",
            extractor_base_url=base_url,
            extractor_model="test",
            reranker=None,
        )
        entity = handle.entity("alice")

        def blocked_episode_commit() -> None:
            try:
                with handle.episode(entity) as episode:
                    episode.message("user", "block the extraction permit")
            finally:
                episode_done.set()

        episode_thread = threading.Thread(target=blocked_episode_commit)
        episode_thread.start()
        assert request_entered.wait(timeout=10), "extractor request never reached the test server"

        # The episode rows are durable and the local permit is back before the
        # extractor round trip, so an unrelated recall completes while the
        # extraction is still parked on the network.
        def recall_while_blocked() -> None:
            try:
                handle.recall("gil release probe")
            finally:
                recall_done.set()

        recall_thread = threading.Thread(target=recall_while_blocked)
        recall_thread.start()
        recall_completed_while_blocked = recall_done.wait(timeout=30)
        episode_still_blocked_after_recall = not episode_done.is_set()

        # And the parked extraction thread must not be holding the GIL.
        progress_thread = threading.Thread(target=progress.set)
        progress_thread.start()
        progressed_while_blocked = progress.wait(timeout=2)
        episode_still_blocked_after_progress = not episode_done.is_set()
    finally:
        release_request.set()
        for thread in (episode_thread, recall_thread, progress_thread):
            if thread is not None:
                thread.join(timeout=10)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)

    assert recall_completed_while_blocked, "recall queued behind the extractor round trip"
    assert episode_still_blocked_after_recall, "episode extraction was not deliberately blocked"
    assert progressed_while_blocked, "independent Python thread could not acquire the GIL"
    assert episode_still_blocked_after_progress, "episode extraction was not deliberately blocked"
    assert episode_thread is not None and not episode_thread.is_alive()
    assert recall_thread is not None and not recall_thread.is_alive()
    assert progress_thread is not None and not progress_thread.is_alive()
