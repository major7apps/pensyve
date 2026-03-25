"""Standalone CLI commands for the Pensyve ↔ Hermes integration.

Invoked from the Hermes CLI as subcommands::

    hermes pensyve status
    hermes pensyve sessions
    hermes pensyve mode hybrid
    hermes pensyve migrate [~/.hermes/memories/]
    hermes pensyve peer [name]

All output goes to stdout using stdlib-only formatting (no Rich dependency).
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from .client import PensyveClientConfig
from .session import PensyveSessionManager

# Default config file location.
_DEFAULT_CONFIG = Path.home() / ".pensyve" / "hermes.json"

_VALID_MODES = ("hybrid", "pensyve", "local")


# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------


def _load_and_save_config(config_path: str | None, updates: dict) -> None:
    """Load the config JSON, merge *updates* into the host block, and write back."""
    path = Path(config_path) if config_path else _DEFAULT_CONFIG

    data: dict = {}
    if path.exists():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            data = {}

    hosts = data.setdefault("hosts", {})
    block = hosts.setdefault("hermes", {})
    block.update(updates)

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


def status(config_path: str | None = None) -> None:
    """Print Pensyve integration status."""
    config = PensyveClientConfig.from_global_config(config_path=config_path)

    label_w = 20
    print("Pensyve ↔ Hermes Integration Status")
    print("=" * 40)
    print(f"{'Enabled':<{label_w}} {config.enabled}")
    print(f"{'Namespace':<{label_w}} {config.namespace}")
    print(f"{'Storage path':<{label_w}} {config.effective_storage_path()}")
    print(f"{'Memory mode':<{label_w}} {config.memory_mode}")
    print(f"{'Recall mode':<{label_w}} {config.recall_mode}")
    print(f"{'Write frequency':<{label_w}} {config.write_frequency}")
    print(f"{'Session strategy':<{label_w}} {config.session_strategy}")
    print(f"{'Peer name':<{label_w}} {config.peer_name or '(auto)'}")
    print(f"{'AI peer':<{label_w}} {config.ai_peer}")

    if config.enabled:
        print()
        try:
            mgr = PensyveSessionManager(config=config)
            stats = mgr._pensyve.stats()
            print("Database Stats")
            print("-" * 40)
            print(f"{'Entities':<{label_w}} {stats.get('entities', 0)}")
            print(f"{'Episodic memories':<{label_w}} {stats.get('episodic', 0)}")
            print(f"{'Semantic memories':<{label_w}} {stats.get('semantic', 0)}")
            print(f"{'Procedural memories':<{label_w}} {stats.get('procedural', 0)}")
            mgr.shutdown()
        except Exception as exc:
            print(f"Could not load database: {exc}")


def sessions(config_path: str | None = None) -> None:
    """List all sessions and database stats."""
    config = PensyveClientConfig.from_global_config(config_path=config_path)

    try:
        mgr = PensyveSessionManager(config=config)
    except Exception as exc:
        print(f"Could not create session manager: {exc}")
        sys.exit(1)

    label_w = 20
    stats = mgr._pensyve.stats()

    print("Pensyve Sessions")
    print("=" * 40)
    print(f"{'Namespace':<{label_w}} {config.namespace}")
    print(f"{'Storage path':<{label_w}} {config.effective_storage_path()}")
    print()
    print("Database Counts")
    print("-" * 40)
    print(f"{'Entities':<{label_w}} {stats.get('entities', 0)}")
    print(f"{'Episodic memories':<{label_w}} {stats.get('episodic', 0)}")
    print(f"{'Semantic memories':<{label_w}} {stats.get('semantic', 0)}")
    print(f"{'Procedural memories':<{label_w}} {stats.get('procedural', 0)}")

    mgr.shutdown()


def mode(new_mode: str, config_path: str | None = None) -> None:
    """Switch the memory mode."""
    if new_mode not in _VALID_MODES:
        print(f"Invalid mode: {new_mode}")
        print(f"Valid modes: {', '.join(_VALID_MODES)}")
        sys.exit(1)

    _load_and_save_config(config_path, {"memoryMode": new_mode})
    print(f"Memory mode set to: {new_mode}")


def migrate(memory_dir: str | None = None, config_path: str | None = None) -> None:
    """Import MEMORY.md and USER.md from a Hermes memory directory."""
    if memory_dir is None:
        memory_dir = os.path.expanduser("~/.hermes/memories/")

    dir_path = Path(memory_dir)
    if not dir_path.is_dir():
        print(f"Memory directory not found: {memory_dir}")
        sys.exit(1)

    config = PensyveClientConfig.from_global_config(config_path=config_path)

    try:
        mgr = PensyveSessionManager(config=config)
    except Exception as exc:
        print(f"Could not create session manager: {exc}")
        sys.exit(1)

    mgr.get_or_create("migration")

    # Count sections before migration for comparison.
    stats_before = mgr._pensyve.stats()
    semantic_before = stats_before.get("semantic", 0)

    ok = mgr.migrate_memory_files("migration", memory_dir)

    stats_after = mgr._pensyve.stats()
    semantic_after = stats_after.get("semantic", 0)
    imported = semantic_after - semantic_before

    if ok:
        print(f"Imported {imported} memory sections from {memory_dir}")
    else:
        print("Migration failed — check logs for details.")
        sys.exit(1)

    mgr.shutdown()


def peer(name: str | None = None, config_path: str | None = None) -> None:
    """View or update the peer name."""
    config = PensyveClientConfig.from_global_config(config_path=config_path)

    if name is None:
        current = config.peer_name or "(auto)"
        print(f"Current peer name: {current}")
    else:
        _load_and_save_config(config_path, {"peerName": name})
        print(f"Peer name set to: {name}")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def main() -> None:
    """Simple CLI dispatcher using sys.argv."""
    args = sys.argv[1:]

    if not args or args[0] == "status":
        status()
    elif args[0] == "sessions":
        sessions()
    elif args[0] == "mode" and len(args) > 1:
        mode(args[1])
    elif args[0] == "migrate":
        migrate(args[1] if len(args) > 1 else None)
    elif args[0] == "peer":
        peer(args[1] if len(args) > 1 else None)
    else:
        print(f"Unknown command: {args[0]}")
        print("Usage: pensyve-hermes [status|sessions|mode|migrate|peer]")
        sys.exit(1)


if __name__ == "__main__":
    main()
