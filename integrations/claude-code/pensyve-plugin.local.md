---
namespace: "default"
auto_capture: "tiered"
capture_buffer: true
capture_review_point: "stop"
max_auto_memories_per_session: 10
consolidation_frequency: "session_end"
context_loading: "summary"
prompt_enrichment: false
---

# Pensyve Plugin Configuration

This file controls Pensyve plugin behavior. Copy to your project root and edit.

## Settings

- **namespace** -- Memory namespace. Set to your project name for project-scoped memory. Default: directory name.
- **auto_capture** -- Memory capture mode. Default: `"tiered"`.
  - `"off"` -- No automatic memory capture.
  - `"tiered"` -- High-confidence memories auto-stored silently; medium-confidence batched for review at task completion. Recommended.
  - `"full"` -- All memories above threshold auto-stored silently. For power users.
  - `"confirm-all"` -- Every memory presented for individual confirmation. Legacy behavior.
- **capture_buffer** -- Enable PostToolUse signal buffering for richer memory context. Default: true.
- **capture_review_point** -- When to present tier 2 candidates for batch review: `"stop"`, `"pre-compact"`, or `"both"`. Default: `"stop"`.
- **max_auto_memories_per_session** -- Maximum tier 1 (auto-stored) memories per session. Prevents runaway storage. Default: 10.
- **consolidation_frequency** -- When to run memory consolidation: `"manual"`, `"session_end"`, or `"daily"`. Default: `"session_end"`.
- **context_loading** -- How much context to load at session start: `"off"`, `"summary"`, or `"full"`. Default: `"summary"`.
- **prompt_enrichment** -- Enable automatic prompt enrichment with memory context. Default: false (opt-in only).

## Migration from v1.0.x

If upgrading from v1.0.x, your existing settings are backward compatible:
- `auto_capture: false` is treated as `auto_capture: "off"`
- `auto_capture: true` is treated as `auto_capture: "confirm-all"`
