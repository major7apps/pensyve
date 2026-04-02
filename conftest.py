import os
import sys

# Ensure the project root is on sys.path so pensyve_server is importable
sys.path.insert(0, os.path.dirname(__file__))

# Default auth mode to disabled for tests (production default is "required")
os.environ.setdefault("PENSYVE_AUTH_MODE", "disabled")
