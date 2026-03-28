"""Smoke tests for Pensyve integration modules.

Each integration has its own comprehensive test suite in its directory.
This file does basic import-level validation to catch broken imports in CI.
"""

import sys
from pathlib import Path

# Add repo root to path so integration modules are importable
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))


class TestLangChainImports:
    def test_store_importable(self):
        from integrations.langchain.pensyve_langchain import PensyveStore

        assert PensyveStore is not None

    def test_item_importable(self):
        from integrations.langchain.pensyve_langchain import Item

        assert Item is not None


class TestCrewAIImports:
    def test_memory_importable(self):
        from integrations.crewai.pensyve_crewai import PensyveMemory

        assert PensyveMemory is not None

    def test_result_types_importable(self):
        from integrations.crewai.pensyve_crewai import MemoryMatch, MemoryRecord

        assert MemoryMatch is not None
        assert MemoryRecord is not None


class TestAutoGenImports:
    def test_memory_importable(self):
        from integrations.autogen.pensyve_autogen import PensyveMemory

        assert PensyveMemory is not None
