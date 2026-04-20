---
namespace: "default"
auto_capture: "tiered"
capture_buffer: true
capture_review_point: "stop"
max_auto_memories_per_session: 10
consolidation_frequency: "session_end"
context_loading: "summary"
prompt_enrichment: true
---

# Pensyve Plugin Configuration

This file controls Pensyve plugin behavior. Copy to your project root and edit.

## Settings

- **namespace** — Memory namespace. Set to your project name for project-scoped memory. Default: auto-detected from git root.
- **auto_capture** — Memory capture mode. Default: `"tiered"`.
  - `"off"` — No automatic memory capture. Only manual `/remember` saves.
  - `"tiered"` — High-confidence memories auto-stored in-flight during work; medium-confidence batched for review at Stop. Recommended.
  - `"full"` — All memories above threshold auto-stored silently, both in-flight and at Stop.
  - `"confirm-all"` — Every memory presented for individual confirmation. Legacy; slower.
- **capture_buffer** — Enable PostToolUse signal buffering. Default: `true`. Required for in-flight captures.
- **capture_review_point** — When to present tier-2 batch for review: `"stop"`, `"pre-compact"`, or `"both"`. Default: `"stop"`.
- **max_auto_memories_per_session** — Cap on auto-stored memories per session (in-flight + residual combined). Default: `10`. Longitudinal/eval sessions may benefit from 20-30.
- **consolidation_frequency** — When to run consolidation: `"manual"`, `"session_end"`, or `"daily"`. Default: `"session_end"`.
- **context_loading** — Session-start context amount: `"off"`, `"summary"`, or `"full"`. Default: `"summary"`.
- **prompt_enrichment** — Enrich prompts with memory context before the model sees them. Default: `true` (opt-out via `false`).

## Migration from v1.x

v1.x users had `prompt_enrichment: false` and all captures at Stop. v2 flips these defaults:

- `prompt_enrichment: true` — substantive prompts are now recall-enriched by default.
- Captures happen in-flight — `Stop` handles residuals.
- Thread-aware continuity — sessions that continue prior work resume with prior lessons.

To keep v1.x behavior: set `auto_capture: off` and `prompt_enrichment: false`.
