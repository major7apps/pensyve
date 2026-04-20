---
name: Entity Detection
description: Pensyve entity detection — canonicalization and fallback rules for recall scoping (always-apply)
---

# Entity Detection (Always-Apply Reference)

Shared rules for detecting entity names from tool inputs, prompts, and conversation context. Used by the memory reflex and all memory-woven flows to scope recalls and `about_entity` fields on observations.

## Inputs

Extract candidate entity names from:

1. **File references** — `@filename`, `@path/to/file`, or files mentioned directly in the conversation.
2. **User prompts** — explicit references to components, files, services, research phases (e.g., "phase-4.3", "v7r-classifier", "auth-service").
3. **Code context** — module names, class names, function names in files you are editing or reading.
4. **Git context** — repository root name, branch name (when discoverable).

## Canonicalization

- Lowercase all characters.
- Replace spaces and underscores with hyphens.
- Strip file extensions unless the file is the entity itself (e.g., `package.json`).
- Collapse paths to the most semantically meaningful segment (e.g., `src/engine/hybrid_router.rs` → `hybrid-router`; `tests/integration/auth_test.py` → `auth`).

## Fallback behavior

- If no specific entity is detected, fall back to the project-level entity (repository root name, lowercase-hyphenated).
- If a candidate entity is ambiguous between two names, prefer the one that already has memories in Pensyve (call `pensyve_inspect` with limit 1 to check).
- **Never fabricate entity names.** If nothing confident emerges, use the project entity.

## Output

A set of 1–3 candidate entity names per turn. The primary entity is the most specific; secondary entities provide additional context. Since `pensyve_recall` accepts only a single `entity` parameter, fold secondary entities into the `query` string instead.

## Examples

| Input | Primary entity | Secondary (fold into query) |
|---|---|---|
| Edit on `src/engine/hybrid_router.rs` | `hybrid-router` | `engine` |
| Prompt: "tune V7r noise threshold" | `v7r` | `noise-threshold` |
| File `tests/test_auth.py` in a project named `auth-service` | `auth` | `auth-service` |
| User mentions "phase-4.3 calibration" | `phase-4.3` | `calibration` |
