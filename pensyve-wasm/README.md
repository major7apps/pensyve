# pensyve-wasm

> **Experimental / Demo** — This is a lightweight in-browser demo build, NOT a production memory runtime.

## What Works

- Basic `remember(entity, fact)` / `recall(query, limit)` / `forget(entity)` with substring matching
- `stats()` returns memory counts
- In-memory storage (no external dependencies)
- Zero-dependency WASM module via wasm-bindgen

## What Doesn't Work

- **No vector embeddings** — uses substring matching, not semantic search
- **No persistence** — all data lost on page reload
- **No episodic or procedural memory types** — everything stored as semantic
- **No consolidation, decay, or FSRS** — no learning features
- **No confidence or stability tracking** — hardcoded values
- **No graph traversal or multi-signal fusion** — single-signal substring only

## Use Case

Browser-based playground and lightweight demos. See the [pensyve-cloud playground](/playground) for an interactive example.

## Not Suitable For

Production memory storage, semantic search, agent memory, or any use case requiring persistence, embeddings, or the full Pensyve cognitive architecture.

## Building

```bash
wasm-pack build --target web
```

## API

```javascript
import init, { PensyveWasm } from './pkg/pensyve_wasm';

await init();
const p = new PensyveWasm();
p.remember("user", "Prefers dark mode");
const results = p.recall("preferences", 5);
console.log(results); // JSON array of matches
p.forget("user");
console.log(p.stats()); // {"total": 0}
```

## Relationship to pensyve-core

This is a **standalone minimal implementation** that does NOT use pensyve-core. The full Pensyve engine (with embeddings, FSRS decay, graph retrieval, and 8-signal fusion) is available via:

- **Python**: `pip install pensyve`
- **TypeScript**: `npm install pensyve` (HTTP client)
- **Go**: `go get github.com/major7apps/pensyve-go`
- **MCP**: Claude Code / Cursor integration
- **REST API**: Rust/Axum gateway (REST + MCP)
