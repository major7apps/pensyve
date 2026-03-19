---
namespace: "default"
auto_capture: false
consolidation_frequency: "manual"
context_loading: "summary"
prompt_enrichment: false
---

# Pensyve Plugin Configuration

This file controls Pensyve plugin behavior. Copy to your project root and edit.

## Settings

- **namespace** -- Memory namespace. Set to your project name for project-scoped memory. Default: directory name.
- **auto_capture** -- Enable the memory-curator agent for background memory capture. Default: false.
- **consolidation_frequency** -- When to run memory consolidation: `manual`, `session_end`, or `daily`. Default: manual.
- **context_loading** -- How much context to load at session start: `off`, `summary`, or `full`. Default: summary.
- **prompt_enrichment** -- Enable the UserPromptSubmit hook to automatically enrich prompts with memory context. Default: false (opt-in only).
