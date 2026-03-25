"""Pensyve session manager for Hermes Agent integration.

Drop-in replacement for HonchoSessionManager. Uses Pensyve's PyO3
in-process bindings for zero-latency memory access.

Usage:
    from integrations.hermes.session import PensyveSessionManager
    mgr = PensyveSessionManager()
    session = mgr.get_or_create("telegram:123456")
    session.add_message("user", "Hello!")
    mgr.save(session)
"""

from __future__ import annotations

import logging
import threading
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from queue import Queue
from typing import Any

import pensyve

from .client import PensyveClientConfig

logger = logging.getLogger(__name__)

# Sentinel used to signal the async writer thread to shut down.
_SHUTDOWN = object()


# ---------------------------------------------------------------------------
# Session dataclass
# ---------------------------------------------------------------------------


@dataclass
class PensyveSession:
    """Represents an active memory session between a user and an AI peer."""

    key: str
    user_entity: Any
    ai_entity: Any
    episode: Any | None = None
    messages: list[dict[str, Any]] = field(default_factory=list)
    created_at: datetime = field(default_factory=datetime.now)
    updated_at: datetime = field(default_factory=datetime.now)
    metadata: dict[str, Any] = field(default_factory=dict)

    def add_message(self, role: str, content: str, **kwargs: Any) -> None:
        """Append a message to the session buffer.

        Messages are marked ``_synced=False`` until explicitly flushed to
        Pensyve storage.
        """
        self.messages.append(
            {
                "role": role,
                "content": content,
                "timestamp": datetime.now().isoformat(),
                "_synced": False,
                **kwargs,
            }
        )
        self.updated_at = datetime.now()

    def unsynced_messages(self) -> list[dict[str, Any]]:
        """Return messages that have not yet been persisted."""
        return [m for m in self.messages if not m.get("_synced")]

    def mark_synced(self) -> None:
        """Flag every message in the buffer as persisted."""
        for m in self.messages:
            m["_synced"] = True


# ---------------------------------------------------------------------------
# Session manager
# ---------------------------------------------------------------------------


class PensyveSessionManager:
    """Core session manager mirroring the HonchoSessionManager interface.

    Manages session lifecycle, message persistence, context retrieval,
    prefetch, and migration — all backed by Pensyve's local engine.
    """

    def __init__(
        self,
        config: PensyveClientConfig | None = None,
        namespace: str = "hermes",
        storage_path: str | None = None,
    ) -> None:
        self._config = config or PensyveClientConfig()
        self._namespace = namespace or self._config.namespace
        self._storage_path = storage_path or self._config.effective_storage_path()

        # Ensure storage directory exists.
        Path(self._storage_path).mkdir(parents=True, exist_ok=True)

        self._pensyve = pensyve.Pensyve(
            path=self._storage_path,
            namespace=self._namespace,
        )

        # Session cache: session_key -> PensyveSession
        self._sessions: dict[str, PensyveSession] = {}

        # Prefetch caches (protected by locks)
        self._context_cache: dict[str, dict] = {}
        self._context_lock = threading.Lock()

        self._dialectic_cache: dict[str, str] = {}
        self._dialectic_lock = threading.Lock()

        # Write-frequency bookkeeping
        self._save_counter: dict[str, int] = {}

        # Async writer thread (if configured)
        self._write_queue: Queue | None = None
        self._writer_thread: threading.Thread | None = None

        if self._config.write_frequency == "async":
            self._write_queue = Queue()
            self._writer_thread = threading.Thread(
                target=self._async_writer,
                daemon=True,
                name="pensyve-async-writer",
            )
            self._writer_thread.start()

    # -- Session lifecycle --------------------------------------------------

    def get_or_create(self, key: str) -> PensyveSession:
        """Return the cached session for *key*, creating one if needed."""
        if key in self._sessions:
            return self._sessions[key]

        try:
            peer_name = self._config.peer_name or key
            user_entity = self._pensyve.entity(peer_name, kind="user")
            ai_entity = self._pensyve.entity(self._config.ai_peer, kind="agent")

            session = PensyveSession(
                key=key,
                user_entity=user_entity,
                ai_entity=ai_entity,
            )
            self._sessions[key] = session
            logger.debug(
                "Created session %s (user=%s, ai=%s)", key, peer_name, self._config.ai_peer
            )
            return session
        except Exception:
            logger.exception("Failed to create session for key=%s", key)
            raise

    def new_session(self, key: str) -> PensyveSession:
        """End the current episode (if any) and start a fresh session."""
        existing = self._sessions.get(key)
        if existing and existing.episode is not None:
            try:
                existing.episode.outcome("session ended")
            except Exception:
                logger.warning("Failed to end episode for session %s", key, exc_info=True)

        # Remove stale session from cache so get_or_create builds a new one.
        self._sessions.pop(key, None)
        return self.get_or_create(key)

    def delete(self, key: str) -> bool:
        """Remove a session from the in-memory cache.

        Returns ``True`` if the session existed, ``False`` otherwise.
        """
        removed = self._sessions.pop(key, None)
        self._save_counter.pop(key, None)
        return removed is not None

    def list_sessions(self) -> list[dict]:
        """Return metadata for all cached sessions."""
        result: list[dict] = []
        for key, session in self._sessions.items():
            result.append(
                {
                    "key": key,
                    "user": getattr(session.user_entity, "name", str(session.user_entity)),
                    "ai": getattr(session.ai_entity, "name", str(session.ai_entity)),
                    "message_count": len(session.messages),
                    "last_activity": session.updated_at.isoformat(),
                }
            )
        return result

    # -- Message persistence ------------------------------------------------

    def save(self, session: PensyveSession) -> None:
        """Persist unsynced messages according to the configured write frequency."""
        freq = self._config.write_frequency

        unsynced = session.unsynced_messages()
        if not unsynced:
            return

        try:
            if freq == "async":
                if self._write_queue is not None:
                    # Snapshot the messages so the list can be mutated safely.
                    self._write_queue.put((session, list(unsynced)))
                session.mark_synced()

            elif freq == "turn":
                self._flush_session(session, list(unsynced))
                session.mark_synced()

            elif freq == "session":
                # Defer — flush_all or shutdown will handle it.
                pass

            elif isinstance(freq, int) and freq > 0:
                counter = self._save_counter.get(session.key, 0) + 1
                self._save_counter[session.key] = counter
                if counter % freq == 0:
                    self._flush_session(session, list(unsynced))
                    session.mark_synced()

            else:
                # Unknown frequency — treat as turn.
                self._flush_session(session, list(unsynced))
                session.mark_synced()

        except Exception:
            logger.exception("Error during save for session %s", session.key)

    def _flush_session(
        self,
        session: PensyveSession,
        messages: list[dict[str, Any]],
    ) -> None:
        """Write *messages* to Pensyve storage.

        If an episode is active on the session, messages are stored as
        episode messages. Otherwise they are stored as semantic facts.
        """
        for msg in messages:
            role = msg.get("role", "unknown")
            content = msg.get("content", "")
            if not content:
                continue

            try:
                if session.episode is not None:
                    session.episode.message(role, content)
                else:
                    self._pensyve.remember(
                        entity=session.user_entity,
                        fact=f"[{role}] {content}",
                        confidence=0.8,
                    )
            except Exception:
                logger.exception(
                    "Failed to flush message (role=%s) for session %s",
                    role,
                    session.key,
                )

    def flush_all(self) -> None:
        """Flush unsynced messages for every cached session."""
        for session in self._sessions.values():
            unsynced = session.unsynced_messages()
            if unsynced:
                try:
                    self._flush_session(session, list(unsynced))
                    session.mark_synced()
                except Exception:
                    logger.exception("flush_all failed for session %s", session.key)

    def shutdown(self) -> None:
        """Shut down the async writer thread (if running) and flush remaining data."""
        if self._writer_thread is not None and self._write_queue is not None:
            self._write_queue.put(_SHUTDOWN)
            self._writer_thread.join(timeout=10)
            if self._writer_thread.is_alive():
                logger.warning("Async writer thread did not terminate within 10s")

        # Flush anything that didn't make it through the queue.
        self.flush_all()

    # -- Async writer -------------------------------------------------------

    def _async_writer(self) -> None:
        """Background thread that drains the write queue."""
        while True:
            item = self._write_queue.get()  # type: ignore[union-attr]
            if item is _SHUTDOWN:
                break
            session, messages = item
            try:
                self._flush_session(session, messages)
            except Exception:
                logger.exception("Async writer failed for session %s", session.key)

    # -- Context retrieval --------------------------------------------------

    def search_context(
        self,
        session_key: str,
        query: str,
        max_tokens: int = 800,
    ) -> str:
        """Search Pensyve for memories relevant to *query*.

        Returns a formatted text string truncated to approximately
        *max_tokens* (estimated at 4 characters per token).
        """
        try:
            session = self.get_or_create(session_key)
            memories = self._pensyve.recall(
                query,
                entity=session.user_entity,
                limit=10,
            )

            if not memories:
                return ""

            lines: list[str] = []
            for mem in memories:
                score = getattr(mem, "score", None)
                content = getattr(mem, "content", str(mem))
                prefix = f"[{score:.2f}] " if score is not None else ""
                lines.append(f"{prefix}{content}")

            text = "\n".join(lines)

            # Rough truncation: 4 chars ≈ 1 token.
            max_chars = max_tokens * 4
            if len(text) > max_chars:
                text = text[:max_chars] + "..."

            return text

        except Exception:
            logger.exception("search_context failed for session %s", session_key)
            return ""

    def get_peer_card(self, session_key: str) -> list[str]:
        """Return the user's top memories as a list of strings."""
        try:
            session = self.get_or_create(session_key)
            memories = self._pensyve.recall(
                "",
                entity=session.user_entity,
                limit=10,
            )

            if not memories:
                return ["No memories yet."]

            return [getattr(m, "content", str(m)) for m in memories]

        except Exception:
            logger.exception("get_peer_card failed for session %s", session_key)
            return ["No memories yet."]

    def create_conclusion(self, session_key: str, content: str) -> bool:
        """Store a conclusion as a semantic memory for the user entity."""
        try:
            session = self.get_or_create(session_key)
            self._pensyve.remember(
                entity=session.user_entity,
                fact=content,
                confidence=0.85,
            )
            return True
        except Exception:
            logger.exception("create_conclusion failed for session %s", session_key)
            return False

    # -- Prefetch (async, non-blocking) ------------------------------------

    def get_prefetch_context(
        self,
        session_key: str,
        user_message: str | None = None,
    ) -> dict[str, str]:
        """Build a full context dict for the session.

        Keys: ``representation``, ``card``, ``ai_representation``, ``ai_card``.
        """
        result: dict[str, str] = {
            "representation": "",
            "card": "",
            "ai_representation": "",
            "ai_card": "",
        }

        try:
            session = self.get_or_create(session_key)

            # User context: recall with user message + inspect (top memories).
            query = user_message or ""
            user_memories = self._pensyve.recall(
                query,
                entity=session.user_entity,
                limit=10,
            )
            if user_memories:
                result["representation"] = "\n".join(
                    getattr(m, "content", str(m)) for m in user_memories
                )

            user_card = self._pensyve.recall(
                "",
                entity=session.user_entity,
                limit=10,
            )
            if user_card:
                result["card"] = "\n".join(getattr(m, "content", str(m)) for m in user_card)

            # AI context: inspect AI entity.
            ai_memories = self._pensyve.recall(
                "",
                entity=session.ai_entity,
                limit=10,
            )
            if ai_memories:
                result["ai_representation"] = "\n".join(
                    getattr(m, "content", str(m)) for m in ai_memories
                )
                result["ai_card"] = "\n".join(getattr(m, "content", str(m)) for m in ai_memories)

        except Exception:
            logger.exception("get_prefetch_context failed for session %s", session_key)

        return result

    def prefetch_context(
        self,
        session_key: str,
        user_message: str | None = None,
    ) -> None:
        """Fire a background thread to prefetch context and cache the result."""

        def _worker() -> None:
            ctx = self.get_prefetch_context(session_key, user_message)
            self.set_context_result(session_key, ctx)

        thread = threading.Thread(
            target=_worker,
            daemon=True,
            name=f"pensyve-prefetch-{session_key}",
        )
        thread.start()

    def set_context_result(self, session_key: str, result: dict) -> None:
        """Store a prefetched context result in the cache."""
        with self._context_lock:
            self._context_cache[session_key] = result

    def pop_context_result(self, session_key: str) -> dict:
        """Pop and return the prefetched context for *session_key*.

        Returns an empty dict if nothing has been cached.
        """
        with self._context_lock:
            return self._context_cache.pop(session_key, {})

    # -- Dialectic prefetch ------------------------------------------------

    def prefetch_dialectic(self, session_key: str, query: str) -> None:
        """Fire a background thread to run a dialectic search and cache the result."""

        def _worker() -> None:
            result = self.search_context(session_key, query)
            self.set_dialectic_result(session_key, result)

        thread = threading.Thread(
            target=_worker,
            daemon=True,
            name=f"pensyve-dialectic-{session_key}",
        )
        thread.start()

    def set_dialectic_result(self, session_key: str, result: str) -> None:
        """Store a prefetched dialectic result in the cache."""
        with self._dialectic_lock:
            self._dialectic_cache[session_key] = result

    def pop_dialectic_result(self, session_key: str) -> str:
        """Pop and return the prefetched dialectic result.

        Returns an empty string if nothing has been cached.
        """
        with self._dialectic_lock:
            return self._dialectic_cache.pop(session_key, "")

    def dialectic_query(
        self,
        session_key: str,
        query: str,
        reasoning_level: str | None = None,
        peer: str = "user",
    ) -> str:
        """Synchronous dialectic query — calls search_context directly.

        *reasoning_level* and *peer* are accepted for interface
        compatibility with HonchoSessionManager but not yet used.
        """
        return self.search_context(session_key, query)

    # -- Migration ---------------------------------------------------------

    def migrate_local_history(
        self,
        session_key: str,
        messages: list[dict],
    ) -> bool:
        """Replay a list of messages into a new Pensyve episode.

        Each dict in *messages* must have ``role`` and ``content`` keys.
        Returns ``True`` on success.
        """
        try:
            session = self.get_or_create(session_key)
            episode = self._pensyve.episode(session.user_entity, session.ai_entity)
            session.episode = episode

            for msg in messages:
                role = msg.get("role", "unknown")
                content = msg.get("content", "")
                if content:
                    episode.message(role, content)

            logger.info(
                "Migrated %d messages into session %s",
                len(messages),
                session_key,
            )
            return True

        except Exception:
            logger.exception("migrate_local_history failed for session %s", session_key)
            return False

    def migrate_memory_files(
        self,
        session_key: str,
        memory_dir: str,
    ) -> bool:
        """Import MEMORY.md and USER.md from *memory_dir* into Pensyve.

        Sections are split on ``\u00a7`` (section sign) or double-newlines. Each
        section is stored as a semantic memory.
        """
        try:
            session = self.get_or_create(session_key)
            dir_path = Path(memory_dir)
            imported = 0

            for filename in ("MEMORY.md", "USER.md"):
                filepath = dir_path / filename
                if not filepath.exists():
                    logger.debug("Skipping %s (not found)", filepath)
                    continue

                text = filepath.read_text(encoding="utf-8").strip()
                if not text:
                    continue

                # Split on section sign or double-newlines.
                if "\u00a7" in text:
                    sections = [s.strip() for s in text.split("\u00a7")]
                else:
                    sections = [s.strip() for s in text.split("\n\n")]

                for section in sections:
                    if not section:
                        continue
                    try:
                        self._pensyve.remember(
                            entity=session.user_entity,
                            fact=section,
                            confidence=0.8,
                        )
                        imported += 1
                    except Exception:
                        logger.warning(
                            "Failed to store section from %s",
                            filename,
                            exc_info=True,
                        )

            logger.info(
                "Migrated %d memory sections from %s into session %s",
                imported,
                memory_dir,
                session_key,
            )
            return True

        except Exception:
            logger.exception("migrate_memory_files failed for session %s", session_key)
            return False
