"""Pytest configuration — add the integration package to sys.path."""

import sys
from pathlib import Path

# Add integrations/langchain/ to the path so `import pensyve_langchain` works
_langchain_dir = str(Path(__file__).resolve().parent.parent)
if _langchain_dir not in sys.path:
    sys.path.insert(0, _langchain_dir)
