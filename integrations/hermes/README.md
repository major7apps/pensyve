# Hermes Integration

Pensyve integration with the Hermes agent framework.

## Status

**Planned** -- not yet implemented.

## Overview

This integration will provide a Hermes-native memory adapter that connects
Hermes agents to Pensyve for persistent episodic, semantic, and procedural
memory across sessions.

## Authentication

1. Sign up at [pensyve.com](https://pensyve.com)
2. Create an API key at [Settings → API Keys](https://pensyve.com/settings/api-keys)
3. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_your_key_here"
   ```

Then configure MCP with headers (see setup instructions above).

## References

- Design spec: `pensyve-docs/specs/` (Hermes integration spec)
- Implementation plan: `pensyve-docs/plans/` (Hermes integration plan)
- Pensyve core engine: `pensyve-core/`
- Pensyve MCP gateway: `pensyve-mcp-gateway/`
