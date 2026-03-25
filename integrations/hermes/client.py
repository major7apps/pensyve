"""Pensyve client configuration for Hermes Agent integration.

Usage:
    from integrations.hermes.client import PensyveClientConfig
    config = PensyveClientConfig.from_global_config()
"""

from __future__ import annotations

import json
import os
import re
import uuid
from dataclasses import dataclass, field
from pathlib import Path

# Mapping from camelCase JSON keys to snake_case dataclass fields.
_JSON_KEY_MAP: dict[str, str] = {
    "enabled": "enabled",
    "namespace": "namespace",
    "storagePath": "storage_path",
    "peerName": "peer_name",
    "aiPeer": "ai_peer",
    "memoryMode": "memory_mode",
    "recallMode": "recall_mode",
    "writeFrequency": "write_frequency",
    "sessionStrategy": "session_strategy",
    "peerMemoryModes": "peer_memory_modes",
}


@dataclass
class PensyveClientConfig:
    """Configuration for the Pensyve ↔ Hermes integration.

    Controls how Pensyve stores, recalls, and synchronises memory for a
    Hermes Agent session.
    """

    host: str = "hermes"
    namespace: str = "hermes"
    storage_path: str | None = None
    enabled: bool = False
    peer_name: str | None = None
    ai_peer: str = "hermes"
    memory_mode: str = "hybrid"
    peer_memory_modes: dict[str, str] = field(default_factory=dict)
    write_frequency: str | int = "async"
    recall_mode: str = "hybrid"
    session_strategy: str = "per-directory"

    # -- factory -----------------------------------------------------------

    @classmethod
    def from_global_config(
        cls,
        host: str = "hermes",
        config_path: str | None = None,
    ) -> PensyveClientConfig:
        """Build a config by merging file-based and environment-based settings.

        Resolution order:
            1. ``$HERMES_HOME/pensyve.json`` (if *HERMES_HOME* is set)
            2. *config_path* (explicit) **or** ``~/.pensyve/hermes.json``
            3. Environment variables as final fallback
        """
        host_data: dict[str, object] = {}

        # 1. $HERMES_HOME/pensyve.json
        hermes_home = os.environ.get("HERMES_HOME")
        if hermes_home:
            candidate = Path(hermes_home) / "pensyve.json"
            host_data = _load_host_block(candidate, host)

        # 2. Explicit config_path or default ~/.pensyve/hermes.json
        if not host_data:
            if config_path:
                candidate = Path(config_path)
            else:
                candidate = Path.home() / ".pensyve" / "hermes.json"
            host_data = _load_host_block(candidate, host)

        # Translate camelCase → snake_case
        kwargs: dict[str, object] = {"host": host}
        for json_key, py_key in _JSON_KEY_MAP.items():
            if json_key in host_data:
                kwargs[py_key] = host_data[json_key]

        # 3. Environment-variable fallbacks (only when the key was *not*
        #    already set from file).
        if "storage_path" not in kwargs:
            env_path = os.environ.get("PENSYVE_PATH")
            if env_path:
                kwargs["storage_path"] = env_path

        if "namespace" not in kwargs:
            env_ns = os.environ.get("PENSYVE_NAMESPACE")
            if env_ns:
                kwargs["namespace"] = env_ns

        if "enabled" not in kwargs:
            env_enabled = os.environ.get("PENSYVE_HERMES_ENABLED")
            if env_enabled is not None:
                kwargs["enabled"] = env_enabled.lower() in ("1", "true", "yes")

        return cls(**kwargs)  # type: ignore[arg-type]

    # -- helpers -----------------------------------------------------------

    def peer_memory_mode(self, peer_name: str) -> str:
        """Return the memory mode for *peer_name*, falling back to the global mode."""
        return self.peer_memory_modes.get(peer_name, self.memory_mode)

    def resolve_session_name(
        self,
        cwd: str | None = None,
        session_title: str | None = None,
        session_id: str | None = None,
    ) -> str:
        """Derive a session name based on the configured *session_strategy*.

        Args:
            cwd: Working directory (defaults to ``os.getcwd()``).
            session_title: Human-readable title (unused by current strategies
                but reserved for future use).
            session_id: Explicit session identifier for the *per-session*
                strategy.

        Returns:
            A sanitised session name string.
        """
        if cwd is None:
            cwd = os.getcwd()

        strategy = self.session_strategy

        if strategy == "per-directory":
            return _sanitize(cwd)

        if strategy == "per-session":
            return session_id if session_id else str(uuid.uuid4())

        if strategy == "per-repo":
            return _find_repo_name(cwd)

        if strategy == "global":
            return "global"

        # Unknown strategy - fall back to per-directory.
        return _sanitize(cwd)

    def effective_storage_path(self) -> str:
        """Return the resolved storage directory path."""
        return self.storage_path or os.path.expanduser("~/.pensyve/hermes")


# -- module-private helpers ------------------------------------------------


def _load_host_block(path: Path, host: str) -> dict[str, object]:
    """Read *path* and return the ``hosts.<host>`` block, or ``{}``."""
    try:
        with path.open() as fh:
            data = json.load(fh)
        return data.get("hosts", {}).get(host, {})
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return {}


_SAFE_RE = re.compile(r"[^a-zA-Z0-9_-]+")


def _sanitize(value: str) -> str:
    """Replace characters outside ``[a-zA-Z0-9_-]`` with underscores."""
    return _SAFE_RE.sub("_", value)


def _find_repo_name(cwd: str) -> str:
    """Walk up from *cwd* looking for a ``.git`` directory.

    Returns the containing directory's name, or falls back to a sanitised
    *cwd* if no repository root is found.
    """
    current = Path(cwd).resolve()
    while True:
        if (current / ".git").exists():
            return current.name
        parent = current.parent
        if parent == current:
            break
        current = parent
    return _sanitize(cwd)
