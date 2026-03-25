"""Shared Pensyve client for Python integrations.

Supports both local (localhost) and cloud (api.pensyve.com) backends
with auto-detection, API key resolution, and graceful degradation.

Usage:
    from shared.pensyve_client import PensyveClient, resolve_config
    cfg = resolve_config({"mode": "auto", "entity": "my-agent"})
    client = PensyveClient(cfg)
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

import pensyve

LOCAL_DEFAULT = "http://localhost:8000"
CLOUD_DEFAULT = "https://api.pensyve.com"


@dataclass
class PensyveConfig:
    """Resolved configuration for dual-mode Pensyve client."""

    mode: str = "auto"
    local_base_url: str = LOCAL_DEFAULT
    cloud_base_url: str = CLOUD_DEFAULT
    api_key: str = ""
    entity: str = "pensyve-agent"
    namespace: str = "default"
    path: str | None = None
    auto_recall: bool = True
    auto_capture: bool = True
    recall_limit: int = 5


def resolve_config(raw: dict[str, Any] | None = None) -> PensyveConfig:
    """Resolve a config dict into a PensyveConfig.

    API key priority: raw["apiKey"] > raw["cloud"]["apiKey"] > PENSYVE_API_KEY env
    Mode: "auto" → cloud if API key present, else local
    """
    raw = raw or {}
    api_key = (
        raw.get("apiKey")
        or raw.get("api_key")
        or (raw.get("cloud") or {}).get("apiKey")
        or os.environ.get("PENSYVE_API_KEY")
        or ""
    )

    mode = raw.get("mode", "auto")
    if mode == "auto":
        mode = "cloud" if api_key else "local"

    return PensyveConfig(
        mode=mode,
        local_base_url=(raw.get("local") or {}).get("baseUrl", LOCAL_DEFAULT),
        cloud_base_url=(raw.get("cloud") or {}).get("baseUrl", CLOUD_DEFAULT),
        api_key=api_key,
        entity=raw.get("entity", "pensyve-agent"),
        namespace=raw.get("namespace", "default"),
        path=raw.get("path"),
        auto_recall=raw.get("autoRecall", True),
        auto_capture=raw.get("autoCapture", True),
        recall_limit=raw.get("recallLimit", 5),
    )


class PensyveClient:
    """Dual-mode Pensyve client — local PyO3 or cloud REST API.

    In local mode, uses the PyO3 bindings directly (zero latency).
    In cloud mode, uses the REST API with API key auth.
    """

    def __init__(self, config: PensyveConfig | None = None) -> None:
        self._config = config or PensyveConfig()
        self.is_cloud = self._config.mode == "cloud"

        if self.is_cloud:
            self._pensyve = None
            self._entity = None
            self._base_url = self._config.cloud_base_url.rstrip("/")
            self._headers: dict[str, str] = {"Content-Type": "application/json"}
            if self._config.api_key:
                self._headers["Authorization"] = f"Bearer {self._config.api_key}"
        else:
            self._pensyve = pensyve.Pensyve(
                path=self._config.path,
                namespace=self._config.namespace,
            )
            self._entity = self._pensyve.entity(self._config.entity, kind="agent")
            self._base_url = ""
            self._headers = {}

    @property
    def entity_name(self) -> str:
        return self._config.entity

    def recall(self, query: str, limit: int = 5) -> list[Any]:
        """Search memories."""
        if self.is_cloud:
            return self._cloud_recall(query, limit)
        return self._pensyve.recall(query, entity=self._entity, limit=limit)  # type: ignore[union-attr]

    def remember(self, fact: str, confidence: float = 0.85) -> None:
        """Store a memory."""
        if self.is_cloud:
            self._cloud_remember(fact, confidence)
        else:
            self._pensyve.remember(entity=self._entity, fact=fact, confidence=confidence)  # type: ignore[union-attr]

    def forget(self) -> None:
        """Delete all memories for the entity."""
        if self.is_cloud:
            self._cloud_forget()
        else:
            self._pensyve.forget(entity=self._entity)  # type: ignore[union-attr]

    def stats(self) -> dict[str, int]:
        """Get memory statistics."""
        if self.is_cloud:
            return self._cloud_stats()
        return self._pensyve.stats()  # type: ignore[union-attr]

    def status(self) -> dict[str, Any]:
        """Get connection status and memory counts."""
        try:
            s = self.stats()
            return {
                "mode": "cloud" if self.is_cloud else "local",
                "connected": True,
                "endpoint": self._base_url if self.is_cloud else "local (PyO3)",
                **s,
            }
        except Exception:
            return {
                "mode": "offline",
                "connected": False,
                "endpoint": self._base_url or "local",
            }

    def account(self) -> dict[str, Any] | None:
        """Get cloud account info (None if local)."""
        if not self.is_cloud:
            return None
        try:
            import json
            import urllib.request

            req = urllib.request.Request(
                f"{self._base_url}/v1/account",
                headers=self._headers,
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                return json.loads(resp.read())
        except Exception:
            return None

    # -- Cloud REST helpers --------------------------------------------------

    def _cloud_recall(self, query: str, limit: int) -> list[Any]:
        try:
            import json
            import urllib.request

            data = json.dumps(
                {"query": query, "entity": self._config.entity, "limit": limit}
            ).encode()
            req = urllib.request.Request(
                f"{self._base_url}/v1/recall",
                data=data,
                headers=self._headers,
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=10) as resp:
                result = json.loads(resp.read())
                return result.get("memories") or result.get("results") or []
        except Exception:
            return []

    def _cloud_remember(self, fact: str, confidence: float) -> None:
        try:
            import json
            import urllib.request

            data = json.dumps(
                {
                    "entity": self._config.entity,
                    "fact": fact,
                    "confidence": confidence,
                }
            ).encode()
            req = urllib.request.Request(
                f"{self._base_url}/v1/remember",
                data=data,
                headers=self._headers,
                method="POST",
            )
            urllib.request.urlopen(req, timeout=10)
        except Exception:
            pass

    def _cloud_forget(self) -> None:
        try:
            import urllib.request

            req = urllib.request.Request(
                f"{self._base_url}/v1/entities/{self._config.entity}",
                headers=self._headers,
                method="DELETE",
            )
            urllib.request.urlopen(req, timeout=10)
        except Exception:
            pass

    def _cloud_stats(self) -> dict[str, int]:
        try:
            import json
            import urllib.request

            req = urllib.request.Request(
                f"{self._base_url}/v1/stats",
                headers=self._headers,
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                return json.loads(resp.read())
        except Exception:
            return {}
