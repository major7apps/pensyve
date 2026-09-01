"""Concurrency regression for the embedded local runtime.

Requires the current PyO3 extension to be installed with ``maturin develop``.
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

pensyve = pytest.importorskip("pensyve")


def test_blocked_recall_releases_gil_for_an_independent_python_thread(
    tmp_path: Path,
) -> None:
    request_entered = threading.Event()
    release_request = threading.Event()

    class BlockingExtractorHandler(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802 - stdlib callback name
            content_length = int(self.headers.get("Content-Length", "0"))
            self.rfile.read(content_length)
            request_entered.set()
            release_request.wait(timeout=5)
            body = json.dumps(
                {"choices": [{"message": {"content": "[]"}}]}
            ).encode()
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

    episode_done = threading.Event()

    def blocked_episode_commit() -> None:
        try:
            with handle.episode(entity) as episode:
                episode.message("user", "block the local operation permit")
        finally:
            episode_done.set()

    episode_thread = threading.Thread(target=blocked_episode_commit)
    episode_thread.start()
    assert request_entered.wait(timeout=3), "extractor request never reached the test server"

    recall_started = threading.Event()
    recall_done = threading.Event()

    def blocked_recall() -> None:
        recall_started.set()
        try:
            handle.recall("gil release probe")
        finally:
            recall_done.set()

    recall_thread = threading.Thread(target=blocked_recall)
    recall_thread.start()
    assert recall_started.wait(timeout=1)
    # Yield the GIL so recall enters the extension and waits behind the permit
    # held by the deliberately blocked episode extraction.
    time.sleep(0.05)

    progress = threading.Event()
    progress_thread = threading.Thread(target=progress.set)
    progress_thread.start()
    progressed_while_blocked = progress.wait(timeout=2)
    recall_was_still_blocked = not recall_done.is_set()
    episode_was_still_blocked = not episode_done.is_set()

    release_request.set()
    episode_thread.join(timeout=5)
    recall_thread.join(timeout=5)
    progress_thread.join(timeout=5)
    server.shutdown()
    server.server_close()
    server_thread.join(timeout=5)

    assert progressed_while_blocked, "independent Python thread could not acquire the GIL"
    assert recall_was_still_blocked, "recall did not wait for the local permit"
    assert episode_was_still_blocked, "episode extraction was not deliberately blocked"
    assert not episode_thread.is_alive()
    assert not recall_thread.is_alive()
    assert not progress_thread.is_alive()
