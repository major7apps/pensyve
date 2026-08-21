## Part 2: Entity Detection (Always-Apply)

Shared rules for detecting entity names from tool inputs, prompts, and conversation context. Used by the memory reflex and all memory-woven flows.

### Inputs

Extract candidate entity names from:

1. **File references** — `@filename`, `@path/to/file`, or files mentioned directly in the conversation.
2. **User prompts** — explicit references to components, files, services, research phases.
3. **Code context** — module names, class names, function names in files you are editing or reading.
4. **Git context** — repository root name, branch name (when discoverable).

### Canonicalization

- Lowercase all characters.
- Replace spaces and underscores with hyphens.
- Strip file extensions unless the file is the entity itself (e.g., `package.json`).
- Collapse paths to the most semantically meaningful segment.

### Fallback behavior

- If no specific entity is detected, fall back to the project-level entity (repository root name, lowercase-hyphenated).
- If ambiguous, prefer the entity that already has memories in Pensyve (call `pensyve_inspect` with limit 1).
- **Never fabricate entity names.**

### Output

A set of 1–3 candidate entity names per turn. The primary entity is the most specific. Since `pensyve_recall` accepts only a single `entity` parameter, fold secondary entities into the `query` string.

---

