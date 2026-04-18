# Entity Detection (Shared Reference)

Shared rules for detecting entity names from tool inputs, prompts, and conversation context. Used by hooks and memory-woven skills to scope recalls and `about_entity` fields on observations.

## Inputs

Extract candidate entity names from:

1. **Tool inputs** — Read/Edit/Write `file_path` parameters; Bash commands that reference specific files, services, or subsystems.
2. **User prompts** — explicit references to components, files, services, research phases (e.g., "phase-4.3", "v7r-classifier", "auth-service").
3. **Diffs / code blocks** — module names, class names, function names in recent edits.
4. **Git context** — branch name, recent commit messages (from SessionStart hook).

## Canonicalization

- Lowercase all characters.
- Replace spaces and underscores with hyphens.
- Strip file extensions unless the file is the entity itself (e.g., `package.json`).
- Collapse paths to the most semantically meaningful segment (e.g., `src/engine/hybrid_router.rs` → `hybrid-router`; `tests/integration/auth_test.py` → `auth`).

## Fallback behavior

- If no specific entity is detected, fall back to the project-level entity (detected from git root or `PENSYVE_NAMESPACE`).
- If a candidate entity is ambiguous between two names, prefer the one that already has memories in Pensyve (call `pensyve_inspect` with limit 1 to check).
- Never fabricate entity names. If nothing confident emerges, use the project entity.

## Output

A set of 1–3 candidate entity names per turn. The primary entity is the most specific; secondary entities provide related-entity context for `pensyve_recall`.

## Examples

| Input | Primary entity | Secondary |
|---|---|---|
| Edit on `src/engine/hybrid_router.rs` | `hybrid-router` | `engine` |
| Prompt: "tune V7r noise threshold" | `v7r` | `noise-threshold` |
| Bash: `cargo test -p pensyve-core` | `pensyve-core` | |
| Write to `specs/2026-04-18-foo.md` | `foo` | `specs` |
