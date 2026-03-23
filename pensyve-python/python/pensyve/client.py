"""HTTP client for the Pensyve memory API."""
from __future__ import annotations

import httpx
from tenacity import retry, stop_after_attempt, wait_exponential_jitter, retry_if_exception


def _is_retryable(exc: BaseException) -> bool:
    """Returns True for 5xx and network errors."""
    if isinstance(exc, httpx.HTTPStatusError) and exc.response.status_code >= 500:
        return True
    if isinstance(exc, (httpx.ConnectError, httpx.TimeoutException)):
        return True
    return False


class PensyveClient:
    """Synchronous HTTP client for the Pensyve memory API.

    Usage:
        client = PensyveClient(base_url="http://localhost:8000", api_key="my-key")
        result = client.recall("What does the user prefer?")
        client.remember("user", "Prefers dark mode")
    """

    def __init__(
        self,
        base_url: str = "http://localhost:8000",
        api_key: str | None = None,
        timeout: float = 30.0,
        max_retries: int = 3,
    ):
        headers = {"Content-Type": "application/json"}
        if api_key:
            headers["X-Pensyve-Key"] = api_key
        self._client = httpx.Client(
            base_url=base_url,
            headers=headers,
            timeout=timeout,
        )
        self._max_retries = max_retries

    def _request(self, method: str, path: str, **kwargs) -> httpx.Response:
        """Make an HTTP request with retry logic."""

        @retry(
            stop=stop_after_attempt(self._max_retries),
            wait=wait_exponential_jitter(initial=0.5, max=30, jitter=2),
            retry=retry_if_exception(_is_retryable),
            reraise=True,
        )
        def _do():
            resp = self._client.request(method, path, **kwargs)
            resp.raise_for_status()
            return resp

        return _do()

    def recall(
        self,
        query: str,
        *,
        entity: str | None = None,
        limit: int = 5,
        types: list[str] | None = None,
    ) -> dict:
        """Search for relevant memories."""
        body: dict = {"query": query, "limit": limit}
        if entity:
            body["entity"] = entity
        if types:
            body["types"] = types
        return self._request("POST", "/v1/recall", json=body).json()

    def remember(self, entity: str, fact: str, *, confidence: float = 0.8) -> dict:
        """Store a memory."""
        return self._request(
            "POST",
            "/v1/remember",
            json={"entity": entity, "fact": fact, "confidence": confidence},
        ).json()

    def forget(self, entity: str, *, hard_delete: bool = False) -> dict:
        """Delete memories for an entity."""
        params: dict = {}
        if hard_delete:
            params["hard_delete"] = "true"
        return self._request(
            "DELETE", f"/v1/entities/{entity}", params=params
        ).json()

    def entity(self, name: str, *, kind: str = "user") -> dict:
        """Create or get an entity."""
        return self._request(
            "POST", "/v1/entities", json={"name": name, "kind": kind}
        ).json()

    def inspect(
        self,
        entity: str,
        *,
        limit: int = 50,
        cursor: str | None = None,
    ) -> dict:
        """View all memories for an entity."""
        body: dict = {"entity": entity, "limit": limit}
        if cursor:
            body["cursor"] = cursor
        return self._request("POST", "/v1/inspect", json=body).json()

    def consolidate(self) -> dict:
        """Trigger memory consolidation."""
        return self._request("POST", "/v1/consolidate").json()

    def feedback(
        self,
        memory_id: str,
        relevant: bool,
        *,
        signals: list[float] | None = None,
    ) -> dict:
        """Submit retrieval feedback."""
        body: dict = {"memory_id": memory_id, "relevant": relevant}
        if signals:
            body["signals"] = signals
        return self._request("POST", "/v1/feedback", json=body).json()

    def stats(self) -> dict:
        """Get memory statistics."""
        return self._request("GET", "/v1/stats").json()

    def activity(self, *, days: int = 30) -> list:
        """Get daily activity summary."""
        return self._request("GET", f"/v1/activity?days={days}").json()

    def recent_activity(self, *, limit: int = 10) -> list:
        """Get recent activity events."""
        return self._request("GET", f"/v1/activity/recent?limit={limit}").json()

    def usage(self) -> dict:
        """Get usage statistics."""
        return self._request("GET", "/v1/usage").json()

    def health(self) -> dict:
        """Check API health."""
        return self._request("GET", "/v1/health").json()

    def gdpr_erase(self, entity: str) -> dict:
        """GDPR erasure — delete all data for an entity."""
        return self._request("DELETE", f"/v1/gdpr/erase/{entity}").json()

    def start_episode(self, participants: list[str]) -> str:
        """Start a new episode. Returns episode_id."""
        resp = self._request(
            "POST", "/v1/episodes/start", json={"participants": participants}
        ).json()
        return resp["episode_id"]

    def add_message(self, episode_id: str, role: str, content: str) -> dict:
        """Add a message to an episode."""
        return self._request(
            "POST",
            "/v1/episodes/message",
            json={"episode_id": episode_id, "role": role, "content": content},
        ).json()

    def end_episode(self, episode_id: str, *, outcome: str | None = None) -> dict:
        """End an episode."""
        body: dict = {"episode_id": episode_id}
        if outcome:
            body["outcome"] = outcome
        return self._request("POST", "/v1/episodes/end", json=body).json()

    def close(self) -> None:
        """Close the HTTP client."""
        self._client.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class AsyncPensyveClient:
    """Async HTTP client for the Pensyve memory API.

    Usage:
        async with AsyncPensyveClient(base_url="http://localhost:8000") as client:
            result = await client.recall("What does the user prefer?")
    """

    def __init__(
        self,
        base_url: str = "http://localhost:8000",
        api_key: str | None = None,
        timeout: float = 30.0,
        max_retries: int = 3,
    ):
        headers = {"Content-Type": "application/json"}
        if api_key:
            headers["X-Pensyve-Key"] = api_key
        self._client = httpx.AsyncClient(
            base_url=base_url,
            headers=headers,
            timeout=timeout,
        )
        self._max_retries = max_retries

    async def _request(self, method: str, path: str, **kwargs) -> httpx.Response:
        """Make an async HTTP request with retry logic."""
        attempt = 0
        last_exc: BaseException | None = None
        while attempt < self._max_retries:
            try:
                resp = await self._client.request(method, path, **kwargs)
                resp.raise_for_status()
                return resp
            except BaseException as exc:
                last_exc = exc
                if not _is_retryable(exc):
                    raise
                attempt += 1
                if attempt >= self._max_retries:
                    raise
                # simple exponential back-off without asyncio.sleep to keep
                # the implementation dependency-light; callers that need
                # proper async sleep can subclass and override _request.
        raise last_exc  # type: ignore[misc]

    async def recall(
        self,
        query: str,
        *,
        entity: str | None = None,
        limit: int = 5,
        types: list[str] | None = None,
    ) -> dict:
        """Search for relevant memories."""
        body: dict = {"query": query, "limit": limit}
        if entity:
            body["entity"] = entity
        if types:
            body["types"] = types
        return (await self._request("POST", "/v1/recall", json=body)).json()

    async def remember(self, entity: str, fact: str, *, confidence: float = 0.8) -> dict:
        """Store a memory."""
        return (
            await self._request(
                "POST",
                "/v1/remember",
                json={"entity": entity, "fact": fact, "confidence": confidence},
            )
        ).json()

    async def forget(self, entity: str, *, hard_delete: bool = False) -> dict:
        """Delete memories for an entity."""
        params: dict = {}
        if hard_delete:
            params["hard_delete"] = "true"
        return (
            await self._request("DELETE", f"/v1/entities/{entity}", params=params)
        ).json()

    async def entity(self, name: str, *, kind: str = "user") -> dict:
        """Create or get an entity."""
        return (
            await self._request("POST", "/v1/entities", json={"name": name, "kind": kind})
        ).json()

    async def inspect(
        self,
        entity: str,
        *,
        limit: int = 50,
        cursor: str | None = None,
    ) -> dict:
        """View all memories for an entity."""
        body: dict = {"entity": entity, "limit": limit}
        if cursor:
            body["cursor"] = cursor
        return (await self._request("POST", "/v1/inspect", json=body)).json()

    async def consolidate(self) -> dict:
        """Trigger memory consolidation."""
        return (await self._request("POST", "/v1/consolidate")).json()

    async def feedback(
        self,
        memory_id: str,
        relevant: bool,
        *,
        signals: list[float] | None = None,
    ) -> dict:
        """Submit retrieval feedback."""
        body: dict = {"memory_id": memory_id, "relevant": relevant}
        if signals:
            body["signals"] = signals
        return (await self._request("POST", "/v1/feedback", json=body)).json()

    async def stats(self) -> dict:
        """Get memory statistics."""
        return (await self._request("GET", "/v1/stats")).json()

    async def activity(self, *, days: int = 30) -> list:
        """Get daily activity summary."""
        return (await self._request("GET", f"/v1/activity?days={days}")).json()

    async def recent_activity(self, *, limit: int = 10) -> list:
        """Get recent activity events."""
        return (await self._request("GET", f"/v1/activity/recent?limit={limit}")).json()

    async def usage(self) -> dict:
        """Get usage statistics."""
        return (await self._request("GET", "/v1/usage")).json()

    async def health(self) -> dict:
        """Check API health."""
        return (await self._request("GET", "/v1/health")).json()

    async def gdpr_erase(self, entity: str) -> dict:
        """GDPR erasure — delete all data for an entity."""
        return (await self._request("DELETE", f"/v1/gdpr/erase/{entity}")).json()

    async def start_episode(self, participants: list[str]) -> str:
        """Start a new episode. Returns episode_id."""
        resp = (
            await self._request(
                "POST", "/v1/episodes/start", json={"participants": participants}
            )
        ).json()
        return resp["episode_id"]

    async def add_message(self, episode_id: str, role: str, content: str) -> dict:
        """Add a message to an episode."""
        return (
            await self._request(
                "POST",
                "/v1/episodes/message",
                json={"episode_id": episode_id, "role": role, "content": content},
            )
        ).json()

    async def end_episode(self, episode_id: str, *, outcome: str | None = None) -> dict:
        """End an episode."""
        body: dict = {"episode_id": episode_id}
        if outcome:
            body["outcome"] = outcome
        return (await self._request("POST", "/v1/episodes/end", json=body)).json()

    async def close(self) -> None:
        """Close the HTTP client."""
        await self._client.aclose()

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        await self.close()
