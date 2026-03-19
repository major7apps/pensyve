# Track 2: Claude Code Plugin — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a Pensyve plugin to the Claude Code marketplace as a full cognitive memory layer for coding sessions.

**Architecture:** Plugin wraps the existing pensyve-mcp binary via .mcp.json, adding slash commands for memory operations, skills for workflow guidance, sub-agents for autonomous memory curation and context research, and lifecycle hooks for session-aware memory capture. All memory operations go through MCP tools — the plugin never accesses storage directly.

**Tech Stack:** Claude Code plugin system (plugin.json, markdown-based components), pensyve-mcp (Rust binary), MCP protocol

**Key Design Constraints:**
- Plugin NEVER reads or writes `.claude/` memory files
- CLAUDE.md owns static project conventions; Pensyve owns dynamic cross-session memory
- All memory ops go through MCP tools (pensyve_recall, pensyve_remember, pensyve_episode_start, pensyve_episode_end, pensyve_forget, pensyve_inspect)
- Configuration via `pensyve-plugin.local.md` with YAML frontmatter
- Hooks: SessionStart (load context), Stop (extract decisions), PreCompact (persist data), UserPromptSubmit (enrich prompts — off by default)

**Sprint Layout:**
```
Sprint 1: Task 2.1 (scaffold) + Task 2.2 (commands)
Sprint 2: Task 2.3 (skills) + Task 2.4 (agents)
Sprint 3: Task 2.5 (hooks) + Task 2.6 (packaging)
```

**Task Dependencies:**
```
Task 2.1 (scaffold + MCP) → Task 2.2 (commands)
Task 2.2 → Task 2.3 (skills) → Task 2.4 (agents)
Task 2.4 → Task 2.5 (hooks) → Task 2.6 (packaging)
```

**Spec:** `docs/superpowers/specs/2026-03-18-pensyve-full-buildout-design.md` (Track 2 section)

---

## File Structure

```
pensyve-plugin/
├── plugin.json                         # Plugin manifest — name, version, component registry
├── .mcp.json                           # MCP server config pointing to pensyve-mcp binary
├── README.md                           # Marketplace documentation
├── pensyve-plugin.local.md             # User configuration template (YAML frontmatter)
├── commands/
│   ├── remember.md                     # /remember <fact> — store semantic memory
│   ├── recall.md                       # /recall <query> — search memories
│   ├── forget.md                       # /forget <entity> — delete entity memories
│   ├── inspect.md                      # /inspect [entity] — view memories by type
│   ├── consolidate.md                  # /consolidate — trigger dreaming cycle
│   └── memory-status.md               # /memory-status — show namespace stats
├── skills/
│   ├── session-memory.md               # End-of-session memory capture
│   ├── memory-informed-refactor.md     # Recall procedural memories before refactoring
│   ├── context-loader.md               # Load relevant memories at session start
│   └── memory-review.md               # Flag stale/contradictory facts
├── agents/
│   ├── memory-curator.md               # Background: identify memorable events
│   └── context-researcher.md           # On-demand: search memory for prior context
└── hooks/
    ├── session-start.md                # SessionStart: load relevant memories
    ├── stop.md                         # Stop: extract decisions/outcomes
    ├── pre-compact.md                  # PreCompact: persist in-flight data
    └── user-prompt-submit.md           # UserPromptSubmit: enrich with context (off by default)
```

---

## Task 2.1: Plugin Scaffold & MCP Integration (Sprint 1)

**Goal:** Create the plugin manifest and MCP server configuration so that installing the plugin gives Claude Code access to all 6 Pensyve memory tools.

**Files:**
- Create: `pensyve-plugin/plugin.json`
- Create: `pensyve-plugin/.mcp.json`
- Create: `pensyve-plugin/pensyve-plugin.local.md`

### Steps

- [ ] **2.1.1** Create the `pensyve-plugin/` directory

```bash
mkdir -p pensyve-plugin/{commands,skills,agents,hooks}
```

- [ ] **2.1.2** Create `pensyve-plugin/plugin.json` with the following contents:

```json
{
  "name": "pensyve",
  "version": "0.1.0",
  "description": "Universal memory runtime — cross-session cognitive memory for Claude Code. Remembers decisions, patterns, and context across coding sessions.",
  "author": "Major7 Apps",
  "homepage": "https://pensyve.com",
  "repository": "https://github.com/major7apps/pensyve",
  "license": "Apache-2.0",
  "commands": [
    "commands/remember.md",
    "commands/recall.md",
    "commands/forget.md",
    "commands/inspect.md",
    "commands/consolidate.md",
    "commands/memory-status.md"
  ],
  "skills": [
    "skills/session-memory.md",
    "skills/memory-informed-refactor.md",
    "skills/context-loader.md",
    "skills/memory-review.md"
  ],
  "agents": [
    "agents/memory-curator.md",
    "agents/context-researcher.md"
  ],
  "hooks": [
    "hooks/session-start.md",
    "hooks/stop.md",
    "hooks/pre-compact.md",
    "hooks/user-prompt-submit.md"
  ]
}
```

- [ ] **2.1.3** Create `pensyve-plugin/.mcp.json` with the following contents:

This points to the pensyve-mcp binary. The binary communicates over stdio using the MCP protocol. Environment variables configure the storage path and namespace.

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": [],
      "env": {
        "PENSYVE_NAMESPACE": "${PENSYVE_NAMESPACE:-default}",
        "PENSYVE_PATH": "${PENSYVE_PATH:-~/.pensyve/default}"
      },
      "description": "Pensyve memory runtime — 6 tools for cross-session cognitive memory"
    }
  }
}
```

**Note:** If `pensyve-mcp` is not on PATH, the user should set the full path to the binary (e.g., from `cargo build --release -p pensyve-mcp`). The `PENSYVE_NAMESPACE` env var controls the namespace (defaults to "default"). Users can set it to their project name for project-scoped memory.

- [ ] **2.1.4** Create `pensyve-plugin/pensyve-plugin.local.md` — the user configuration template:

```markdown
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

- **namespace** — Memory namespace. Set to your project name for project-scoped memory. Default: directory name.
- **auto_capture** — Enable the memory-curator agent for background memory capture. Default: false.
- **consolidation_frequency** — When to run memory consolidation: `manual`, `session_end`, or `daily`. Default: manual.
- **context_loading** — How much context to load at session start: `off`, `summary`, or `full`. Default: summary.
- **prompt_enrichment** — Enable the UserPromptSubmit hook to automatically enrich prompts with memory context. Default: false (opt-in only).
```

- [ ] **2.1.5** Verify the pensyve-mcp binary builds and the 6 tools are available:

```bash
# Build the MCP binary
cargo build -p pensyve-mcp

# Verify it starts (it will wait for stdio input, so just check it launches)
timeout 3 cargo run -p pensyve-mcp 2>&1 || true
# Should see: "pensyve-mcp starting up" and "pensyve-mcp ready, listening on stdio"
```

- [ ] **2.1.6** Verify plugin structure is correct:

```bash
# Check all expected files exist
ls -la pensyve-plugin/plugin.json
ls -la pensyve-plugin/.mcp.json
ls -la pensyve-plugin/pensyve-plugin.local.md
ls -d pensyve-plugin/commands pensyve-plugin/skills pensyve-plugin/agents pensyve-plugin/hooks
```

- [ ] **2.1.7** Commit the scaffold:

```bash
git add pensyve-plugin/plugin.json pensyve-plugin/.mcp.json pensyve-plugin/pensyve-plugin.local.md
git commit -m "feat(plugin): scaffold plugin manifest and MCP integration

Add plugin.json manifest, .mcp.json MCP server configuration pointing
to pensyve-mcp binary, and pensyve-plugin.local.md configuration template.
Plugin exposes all 6 MCP tools (recall, remember, episode_start/end,
forget, inspect) to Claude Code."
```

---

## Task 2.2: Slash Commands (Sprint 1)

**Goal:** Create 6 slash commands that wrap the MCP tools with user-friendly formatting and error handling.

**Files:**
- Create: `pensyve-plugin/commands/remember.md`
- Create: `pensyve-plugin/commands/recall.md`
- Create: `pensyve-plugin/commands/forget.md`
- Create: `pensyve-plugin/commands/inspect.md`
- Create: `pensyve-plugin/commands/consolidate.md`
- Create: `pensyve-plugin/commands/memory-status.md`

### Steps

- [ ] **2.2.1** Create `pensyve-plugin/commands/remember.md`:

```markdown
---
name: remember
description: Store an explicit fact or decision as a semantic memory
arguments:
  - name: fact
    description: The fact, decision, or pattern to remember (include the entity/subject)
    required: true
    type: string
---

# /remember

Store an explicit fact, decision, or pattern in Pensyve's cross-session memory.

## Instructions

When the user invokes `/remember`, follow these steps:

1. Parse the user's input to identify:
   - **Entity**: The subject of the fact (a person, project, tool, concept, or codebase component). If not explicitly stated, infer from context — use the project name for project-level facts, or a specific component/file name for scoped facts.
   - **Fact**: The actual information to store.

2. Call the `pensyve_remember` MCP tool with:
   - `entity`: The identified entity name
   - `fact`: The fact text
   - `confidence`: 1.0 for explicit user statements, 0.8 for inferred facts

3. Format the response:

**Stored in memory:**
> [fact text]

Entity: `[entity name]` | Confidence: [confidence] | ID: `[memory_id]`

4. If the tool returns an error, display it clearly:

**Failed to store memory:** [error message]

## Examples

User: `/remember The auth service uses JWT with RS256 signing`
- Entity: "auth-service"
- Fact: "uses JWT with RS256 signing"

User: `/remember We decided to use PostgreSQL instead of MongoDB for the user store`
- Entity: "user-store"
- Fact: "decided to use PostgreSQL instead of MongoDB"

User: `/remember @alice prefers explicit error types over anyhow in library code`
- Entity: "alice"
- Fact: "prefers explicit error types over anyhow in library code"

## Constraints

- NEVER write to `.claude/` memory files — all storage goes through the MCP tool
- Always confirm what was stored so the user can verify
- Keep entity names lowercase, hyphenated (e.g., "auth-service", not "Auth Service")
```

- [ ] **2.2.2** Create `pensyve-plugin/commands/recall.md`:

```markdown
---
name: recall
description: Search cross-session memories by semantic similarity and text matching
arguments:
  - name: query
    description: What to search for in memory
    required: true
    type: string
---

# /recall

Search Pensyve's cross-session memory using semantic similarity and BM25 text matching.

## Instructions

When the user invokes `/recall`, follow these steps:

1. Call the `pensyve_recall` MCP tool with:
   - `query`: The user's search query
   - `limit`: 10 (default, adjust if user requests more/fewer)
   - `entity`: Include if the user scopes the query to a specific entity
   - `types`: Include if the user asks for specific memory types

2. Format the results as a structured list, grouped by memory type:

### Recall Results for: "[query]"

**Semantic Memories** (facts & decisions):
1. **[subject] [predicate] [object]** (confidence: [conf], score: [score])
2. ...

**Episodic Memories** (past interactions):
1. **[content summary]** (from: [timestamp], score: [score])
2. ...

**Procedural Memories** (action patterns):
1. **[action] → [outcome]** (reliability: [reliability], score: [score])
2. ...

_[N] memories found | Query took [timing if available]_

3. If no results are found:

### No memories found for: "[query]"
No matching memories in the current namespace. Use `/remember` to store facts, or check `/memory-status` for namespace info.

4. If the tool returns an error, display it clearly.

## Examples

User: `/recall authentication setup`
User: `/recall what database did we choose`
User: `/recall @alice coding preferences`

## Constraints

- NEVER fabricate memories — only show what the MCP tool returns
- Always show the relevance score so the user can gauge confidence
- Group results by type for readability
```

- [ ] **2.2.3** Create `pensyve-plugin/commands/forget.md`:

```markdown
---
name: forget
description: Delete all memories associated with an entity
arguments:
  - name: entity
    description: The entity whose memories to remove
    required: true
    type: string
---

# /forget

Delete all memories associated with a specific entity.

## Instructions

When the user invokes `/forget`, follow these steps:

1. **Confirm before deleting.** This is a destructive operation. Ask the user to confirm:

**Are you sure you want to forget all memories for `[entity]`?** This will delete all episodic, semantic, and procedural memories. This cannot be undone.

Reply "yes" to confirm.

2. If the user confirms, call the `pensyve_forget` MCP tool with:
   - `entity`: The entity name to forget

3. Format the response:

**Forgotten:** Removed [count] memories for entity `[entity]` (ID: `[entity_id]`)

4. If the entity is not found:

**Entity not found:** No entity named `[entity]` exists in the current namespace.

## Examples

User: `/forget old-auth-service`
User: `/forget deprecated-module`

## Constraints

- ALWAYS confirm before deleting — never auto-delete
- NEVER write to `.claude/` memory files
- Show the count of deleted memories so the user knows what was removed
```

- [ ] **2.2.4** Create `pensyve-plugin/commands/inspect.md`:

```markdown
---
name: inspect
description: View all memories stored for an entity, optionally filtered by type
arguments:
  - name: entity
    description: The entity to inspect (optional — shows all entities if omitted)
    required: false
    type: string
---

# /inspect

View all memories stored for an entity, grouped by type.

## Instructions

When the user invokes `/inspect`, follow these steps:

1. If an entity name is provided, call the `pensyve_inspect` MCP tool with:
   - `entity`: The entity name
   - `limit`: 20 (default)

2. Format the results:

### Memory Inspection: `[entity]`

**Entity ID:** `[entity_id]`
**Total memories:** [count]

#### Semantic Memories ([count])
| Subject | Predicate | Object | Confidence | Created |
|---------|-----------|--------|------------|---------|
| ... | ... | ... | ... | ... |

#### Episodic Memories ([count])
| Content | Episode | Created |
|---------|---------|---------|
| ... | ... | ... |

#### Procedural Memories ([count])
| Action | Outcome | Reliability | Observations |
|--------|---------|-------------|--------------|
| ... | ... | ... | ... |

3. If no entity is provided, inform the user:

Use `/inspect <entity-name>` to view memories for a specific entity, or `/memory-status` for an overview of the namespace.

4. If the entity is not found, say so clearly.

## Examples

User: `/inspect auth-service`
User: `/inspect @alice`
User: `/inspect` (shows help)

## Constraints

- NEVER fabricate memory contents — only show what the MCP tool returns
- Strip embedding data from display (it's binary noise)
- Show timestamps in human-readable format
```

- [ ] **2.2.5** Create `pensyve-plugin/commands/consolidate.md`:

```markdown
---
name: consolidate
description: Trigger a memory consolidation ("dreaming") cycle
arguments: []
---

# /consolidate

Trigger Pensyve's memory consolidation cycle — the "dreaming" process that:
- Promotes repeated episodic memories to semantic facts
- Decays unaccessed memories (FSRS forgetting curve)
- Archives memories below the retention threshold

## Instructions

When the user invokes `/consolidate`, follow these steps:

1. Inform the user what will happen:

**Starting memory consolidation...**
This will:
- Promote frequently recurring episodic memories to semantic facts
- Apply FSRS decay to unaccessed memories
- Archive memories below the retention threshold

2. Call the `pensyve_recall` MCP tool with a broad query (e.g., empty or "*") to get current memory stats, then use `pensyve_inspect` on key entities to understand the current state.

3. Report the consolidation intent since the MCP server does not yet expose a direct consolidate tool:

**Consolidation Summary:**
- Namespace: `[namespace]`
- Memories inspected: [count]
- Note: Full automated consolidation requires the consolidate MCP tool (planned). Currently, review stale memories with `/memory-review` and remove outdated ones with `/forget`.

## Future Enhancement

When the `pensyve_consolidate` MCP tool is added, this command will call it directly and report:
- Memories promoted (episodic -> semantic): [count]
- Memories decayed: [count]
- Memories archived: [count]
- Time elapsed: [duration]

## Constraints

- This is a read-heavy operation — warn the user it may take a moment for large namespaces
- NEVER modify `.claude/` memory files
```

- [ ] **2.2.6** Create `pensyve-plugin/commands/memory-status.md`:

```markdown
---
name: memory-status
description: Show memory namespace statistics and health overview
arguments: []
---

# /memory-status

Show an overview of the current Pensyve memory namespace — entity counts, memory counts by type, and health indicators.

## Instructions

When the user invokes `/memory-status`, follow these steps:

1. Call `pensyve_inspect` MCP tool for well-known entities to gather stats. Use a broad approach:
   - Inspect with a common entity name or use `pensyve_recall` with a broad query to discover what entities exist.

2. Compile and format the status report:

### Pensyve Memory Status

**Namespace:** `[namespace]`
**Storage:** `[storage_path]`

| Metric | Count |
|--------|-------|
| Entities | [count] |
| Semantic memories | [count] |
| Episodic memories | [count] |
| Procedural memories | [count] |
| **Total memories** | **[count]** |

**Top Entities:**
1. `[entity]` — [N] memories
2. `[entity]` — [N] memories
3. `[entity]` — [N] memories

**Health:**
- Stale memories (>30 days unaccessed): [count or "unknown"]
- Low-confidence facts (<0.5): [count or "unknown"]

_Use `/recall <query>` to search, `/inspect <entity>` to drill down, `/consolidate` to clean up._

3. If the namespace is empty:

### Pensyve Memory Status

**Namespace:** `[namespace]`
**Status:** Empty — no memories stored yet.

Get started:
- `/remember <fact>` — store a fact
- Use the `session-memory` skill at the end of a work session to capture decisions

## Constraints

- NEVER fabricate counts — only report what the MCP tools return
- If stat gathering fails partially, show what's available and note the gaps
```

- [ ] **2.2.7** Verify all command files have valid YAML frontmatter and correct structure:

```bash
# Check all files exist
for f in remember recall forget inspect consolidate memory-status; do
  test -f "pensyve-plugin/commands/$f.md" && echo "OK: $f.md" || echo "MISSING: $f.md"
done

# Verify YAML frontmatter starts each file
for f in pensyve-plugin/commands/*.md; do
  head -1 "$f" | grep -q "^---$" && echo "OK frontmatter: $f" || echo "BAD frontmatter: $f"
done
```

- [ ] **2.2.8** Update `pensyve-plugin/plugin.json` to ensure all 6 commands are listed (verify against step 2.1.2 — they should already be there).

- [ ] **2.2.9** Manual verification — test each command conceptually:

```
Verification checklist:
- [ ] /remember includes entity parsing logic and confidence assignment
- [ ] /recall formats results grouped by memory type with scores
- [ ] /forget requires confirmation before deletion
- [ ] /inspect shows tabular output per memory type
- [ ] /consolidate explains current limitations clearly
- [ ] /memory-status compiles stats from available MCP tools
- [ ] All commands reference MCP tools, never .claude/ files
- [ ] All commands have proper YAML frontmatter with name, description, arguments
```

- [ ] **2.2.10** Commit the commands:

```bash
git add pensyve-plugin/commands/
git commit -m "feat(plugin): add 6 slash commands for memory operations

Add /remember, /recall, /forget, /inspect, /consolidate, and
/memory-status commands. Each wraps pensyve MCP tools with
user-friendly formatting, error handling, and confirmation
dialogs (for destructive operations like /forget)."
```

---

## Task 2.3: Skills (Sprint 2)

**Goal:** Create 4 skills that provide workflow-level guidance for memory-related tasks.

**Files:**
- Create: `pensyve-plugin/skills/session-memory.md`
- Create: `pensyve-plugin/skills/memory-informed-refactor.md`
- Create: `pensyve-plugin/skills/context-loader.md`
- Create: `pensyve-plugin/skills/memory-review.md`

### Steps

- [ ] **2.3.1** Create `pensyve-plugin/skills/session-memory.md`:

```markdown
---
name: session-memory
description: Capture decisions, outcomes, and learned patterns at the end of a work session
arguments:
  - name: scope
    description: What to capture — "all" for full session, or a topic to focus on
    required: false
    type: string
---

# Session Memory Capture

Systematically extract and store the key decisions, outcomes, and patterns from the current work session.

## Instructions

This skill should be used at the end of a work session or after completing a significant task. Follow these steps:

### Step 1: Analyze the Session

Review the conversation history to identify:

1. **Decisions made** — Architecture choices, technology selections, approach decisions
   - Example: "Chose to use a connection pool instead of per-request connections"
   - Example: "Decided to split the module into two crates for better compilation"

2. **Outcomes observed** — What worked, what failed, what was surprising
   - Example: "The SQLite WAL mode fix resolved the concurrent write issue"
   - Example: "Attempt to use async traits caused lifetime issues, fell back to sync"

3. **Patterns learned** — Reusable knowledge about the codebase or tools
   - Example: "This project's test suite requires `make build` before pytest runs"
   - Example: "The auth middleware must be registered before route handlers"

4. **Bug fixes and their root causes** — For procedural memory
   - Example: "Segfault was caused by double-free in the FFI boundary"

### Step 2: Filter for Significance

Discard trivial items. DO NOT store:
- Routine file edits without notable context
- Standard build/test commands that are already in CLAUDE.md
- Information that duplicates what's in CLAUDE.md (static conventions)
- Temporary debugging steps

DO store:
- Information that would help a future session avoid repeating mistakes
- Context that explains WHY a decision was made (not just WHAT)
- Cross-cutting patterns not obvious from reading the code alone

### Step 3: Present for Confirmation

Show the user what you plan to store:

### Session Memory Capture

I identified the following items to remember:

**Decisions:**
1. [decision description] → Entity: `[entity]`
2. ...

**Outcomes:**
1. [outcome description] → Entity: `[entity]`
2. ...

**Patterns:**
1. [pattern description] → Entity: `[entity]`
2. ...

Shall I store these? (Reply "yes" to store all, or specify numbers to exclude)

### Step 4: Store Confirmed Items

For each confirmed item, call the `pensyve_remember` MCP tool:
- `entity`: The relevant entity name
- `fact`: The fact text, written for future recall
- `confidence`: 0.9 for decisions (user-confirmed), 0.8 for observed outcomes, 0.7 for inferred patterns

Report the results:

**Stored [N] memories from this session:**
- [entity]: [fact summary] (ID: [id])
- ...

## Constraints

- NEVER auto-store without user confirmation
- NEVER store information that belongs in CLAUDE.md (static project conventions)
- NEVER access `.claude/` memory files
- Keep fact text concise but include enough context for future recall
- Use consistent entity naming (lowercase, hyphenated)
```

- [ ] **2.3.2** Create `pensyve-plugin/skills/memory-informed-refactor.md`:

```markdown
---
name: memory-informed-refactor
description: Recall past refactoring outcomes and procedural memories before starting a refactor
arguments:
  - name: target
    description: What you're about to refactor (module, component, pattern)
    required: true
    type: string
---

# Memory-Informed Refactoring

Before starting a refactor, search Pensyve memory for relevant prior context — past refactoring outcomes, known pitfalls, architectural decisions, and procedural patterns.

## Instructions

### Step 1: Gather Memory Context

Run these queries against Pensyve memory:

1. **Direct match** — Call `pensyve_recall` with:
   - `query`: "[target] refactor" and "[target] architecture"
   - `types`: ["semantic", "procedural"]
   - `limit`: 10

2. **Related outcomes** — Call `pensyve_recall` with:
   - `query`: "[target] failed" and "[target] issues"
   - `types`: ["episodic", "procedural"]
   - `limit`: 5

3. **Inspect the entity** — Call `pensyve_inspect` with:
   - `entity`: "[target]" (or closest entity name)

### Step 2: Compile the Briefing

Present the gathered context:

### Pre-Refactor Briefing: `[target]`

**Known Facts:**
- [semantic memories about this component]

**Past Refactoring Outcomes:**
- [any episodic/procedural memories about previous refactors]
- If none: "No prior refactoring history found for this component."

**Potential Pitfalls:**
- [any failure-outcome procedural memories]
- [any contradictory facts]

**Related Decisions:**
- [any decision-related semantic memories]

**Recommendation:**
Based on stored memory, here are the key things to watch for:
1. [actionable insight]
2. [actionable insight]

### Step 3: Offer to Track This Refactor

After presenting the briefing:

Shall I start tracking this refactoring session as an episode? This will capture the outcome for future reference.

If yes, call `pensyve_episode_start` with the target entity as a participant.

## Constraints

- Only present memories that actually exist — never fabricate context
- If no relevant memories are found, say so clearly and proceed normally
- NEVER access `.claude/` files or the filesystem for memory data
- Focus on procedural memories (action -> outcome patterns) as the highest-value signal
```

- [ ] **2.3.3** Create `pensyve-plugin/skills/context-loader.md`:

```markdown
---
name: context-loader
description: Load relevant memories from Pensyve at the start of a session or when switching context
arguments:
  - name: topic
    description: Optional topic or area to focus context loading on
    required: false
    type: string
---

# Context Loader

Load relevant cross-session memories to prime the current session with historical context.

## Instructions

### Step 1: Determine Scope

- If a **topic** is provided, focus queries on that topic.
- If no topic is provided, use the current working directory name and any visible project context to determine relevant entities.

### Step 2: Query Memory

Run a series of recall queries:

1. **Recent decisions** — Call `pensyve_recall` with:
   - `query`: "[project/topic] decisions architecture choices"
   - `types`: ["semantic"]
   - `limit`: 10

2. **Known issues** — Call `pensyve_recall` with:
   - `query`: "[project/topic] issues bugs problems"
   - `types`: ["semantic", "procedural"]
   - `limit`: 5

3. **Workflow patterns** — Call `pensyve_recall` with:
   - `query`: "[project/topic] workflow process pattern"
   - `types`: ["procedural"]
   - `limit`: 5

4. **Recent activity** — Call `pensyve_recall` with:
   - `query`: "[project/topic]"
   - `limit`: 5
   (to get the most recent memories by recency scoring)

### Step 3: Format Based on Configuration

Check `pensyve-plugin.local.md` for `context_loading` setting:

**If "off":** Do nothing, return immediately.

**If "summary"** (default):

### Session Context

**Key facts about [project/topic]:**
- [top 3-5 most relevant semantic memories, one line each]

**Watch out for:**
- [any known issues or pitfalls from procedural memory]

**Last session:**
- [1-2 line summary of most recent episodic memories]

_[N] relevant memories loaded. Use `/recall <query>` to search for more._

**If "full":**

Present the complete "summary" output plus:
- Full list of all recalled semantic memories with confidence scores
- All procedural memories with reliability scores
- Entity relationship map (which entities are connected)

### Step 4: Start Episode (Optional)

If context loading found relevant memories, offer:

Shall I start tracking this session as an episode?

If yes, call `pensyve_episode_start` with the project entity.

## Constraints

- Keep summary mode concise — no more than 10-15 lines
- NEVER fabricate memories — only present what recall returns
- Do not slow down session start — if queries take too long, present partial results
- NEVER read `.claude/` files for memory data
```

- [ ] **2.3.4** Create `pensyve-plugin/skills/memory-review.md`:

```markdown
---
name: memory-review
description: Review stored memories for staleness, contradictions, and consolidation opportunities
arguments:
  - name: entity
    description: Optional entity to review (reviews all entities if omitted)
    required: false
    type: string
---

# Memory Review

Audit stored memories to identify stale facts, contradictions, and consolidation opportunities.

## Instructions

### Step 1: Gather Memories

If an entity is specified:
- Call `pensyve_inspect` with that entity, `limit`: 50

If no entity:
- Call `pensyve_recall` with a broad query (e.g., the project name), `limit`: 50
- This surfaces the most relevant memories across all entities

### Step 2: Analyze for Issues

Review each memory for:

1. **Staleness** — Facts that reference outdated technology, old versions, or deprecated patterns
   - Look for: version numbers, date references, "temporary" or "workaround" language
   - Flag any memory older than 30 days that hasn't been accessed

2. **Contradictions** — Memories that conflict with each other
   - Look for: same entity with conflicting predicates (e.g., "uses PostgreSQL" vs "uses MongoDB")
   - Look for: decisions that were later reversed but both versions exist

3. **Low confidence** — Memories stored with confidence < 0.5
   - These may be speculative or inferred and should be verified

4. **Consolidation candidates** — Multiple episodic memories about the same topic
   - These could be merged into a single semantic memory

### Step 3: Present the Review

### Memory Review Report

**Reviewed:** [N] memories across [M] entities

#### Potential Issues

**Stale (may be outdated):**
1. `[entity]`: "[fact]" — stored [date], last accessed [date]
2. ...

**Contradictions:**
1. `[entity]` has conflicting memories:
   - "[fact A]" (confidence: [conf])
   - "[fact B]" (confidence: [conf])
2. ...

**Low Confidence:**
1. `[entity]`: "[fact]" — confidence: [conf]
2. ...

#### Consolidation Opportunities

1. [N] episodic memories about `[topic]` could be merged into a semantic fact
2. ...

#### Recommended Actions

- `/forget [entity]` — to remove stale entity memories
- `/remember [corrected fact]` — to update contradictory facts
- `/consolidate` — to trigger automatic consolidation

**No issues found?** Your memory is clean.

### Step 4: Offer to Act

For each identified issue, offer to take action (with user confirmation):
- Remove stale memories via `pensyve_forget`
- Store corrected facts via `pensyve_remember`
- The user decides — never auto-modify

## Constraints

- NEVER auto-delete or auto-modify memories — always present findings and ask
- NEVER access `.claude/` files
- Be conservative about flagging contradictions — different time periods may explain apparent conflicts
- If reviewing a large namespace, process in batches and show progress
```

- [ ] **2.3.5** Verify all skill files have valid YAML frontmatter:

```bash
for f in session-memory memory-informed-refactor context-loader memory-review; do
  test -f "pensyve-plugin/skills/$f.md" && echo "OK: $f.md" || echo "MISSING: $f.md"
done

for f in pensyve-plugin/skills/*.md; do
  head -1 "$f" | grep -q "^---$" && echo "OK frontmatter: $f" || echo "BAD frontmatter: $f"
done
```

- [ ] **2.3.6** Manual verification checklist:

```
- [ ] session-memory extracts decisions, outcomes, and patterns — never auto-stores
- [ ] memory-informed-refactor runs recall queries before refactoring begins
- [ ] context-loader respects the context_loading config setting (off/summary/full)
- [ ] memory-review identifies staleness, contradictions, low confidence
- [ ] All skills use MCP tools exclusively — no filesystem access
- [ ] All skills have proper YAML frontmatter with name, description, arguments
- [ ] Skills compose with slash commands (e.g., memory-review suggests /forget)
```

- [ ] **2.3.7** Commit the skills:

```bash
git add pensyve-plugin/skills/
git commit -m "feat(plugin): add 4 skills for memory workflows

Add session-memory (end-of-session capture), memory-informed-refactor
(pre-refactor context loading), context-loader (session start priming),
and memory-review (staleness/contradiction detection). All skills use
MCP tools and require user confirmation before modifying memory."
```

---

## Task 2.4: Sub-Agents (Sprint 2)

**Goal:** Create 2 sub-agents — one for background memory curation, one for on-demand context research.

**Files:**
- Create: `pensyve-plugin/agents/memory-curator.md`
- Create: `pensyve-plugin/agents/context-researcher.md`

### Steps

- [ ] **2.4.1** Create `pensyve-plugin/agents/memory-curator.md`:

```markdown
---
name: memory-curator
description: Background agent that monitors coding sessions and identifies memorable events worth storing
model_preference: fast
---

# Memory Curator Agent

You are a background memory curation agent for Pensyve. Your job is to monitor coding sessions and identify events worth storing in long-term memory. You are selective and thoughtful — you capture signal, not noise.

## Role

You observe the coding session and identify non-trivial, memorable events that would benefit future sessions. You SUGGEST storage — you never store autonomously.

## What IS Memorable

- **Architecture decisions** and the reasoning behind them
- **Non-obvious solutions** to problems (especially after debugging)
- **Failed approaches** and why they failed (high-value procedural memory)
- **Cross-cutting discoveries** about the codebase that aren't in docs
- **Dependency behavior** that was learned through trial and error
- **Performance findings** — what's slow, what was optimized, what the thresholds are
- **Integration patterns** — how components connect in non-obvious ways
- **User/team preferences** expressed during the session

## What is NOT Memorable

- Routine file edits (added a function, fixed a typo)
- Standard build/test/lint commands
- Information already in CLAUDE.md or README
- Temporary debugging output or print statements
- One-off questions that won't recur
- File paths or line numbers (too volatile)

## Behavior

### Monitoring

Watch the session for memorable events. When you identify one:

1. Classify it:
   - **Decision** (semantic memory, confidence 0.9)
   - **Outcome** (semantic or procedural memory, confidence 0.8)
   - **Pattern** (procedural memory, confidence 0.7)
   - **Discovery** (semantic memory, confidence 0.8)

2. Draft the memory:
   - **Entity**: The component, tool, or person this is about
   - **Fact**: Concise, context-rich statement optimized for future recall

3. Present to the user (never auto-store):

**Memory suggestion:** I noticed something worth remembering:

> `[entity]`: [fact]

Type: [decision/outcome/pattern/discovery] | Confidence: [conf]

Store this? (yes/no/edit)

### Batch Mode

At the end of a session, compile all identified events and present them together (similar to the `session-memory` skill but running passively throughout).

## Tools Available

- `pensyve_remember` — Store a fact (only after user confirms)
- `pensyve_recall` — Check if a similar memory already exists (to avoid duplicates)
- `pensyve_inspect` — Look up existing entity memories for context

## Activation

This agent is activated when `auto_capture: true` is set in `pensyve-plugin.local.md`. When `auto_capture: false` (default), this agent does not run.

## Constraints

- NEVER store memories without explicit user confirmation
- NEVER read or write `.claude/` memory files
- NEVER store information that duplicates CLAUDE.md content
- Before suggesting a memory, call `pensyve_recall` to check for duplicates
- Prefer quality over quantity — 2-3 high-value memories per session is ideal
- Use the `fast` model to minimize resource usage during background monitoring
```

- [ ] **2.4.2** Create `pensyve-plugin/agents/context-researcher.md`:

```markdown
---
name: context-researcher
description: On-demand agent that searches Pensyve memory for relevant prior context and returns a structured briefing
model_preference: default
---

# Context Researcher Agent

You are an on-demand research agent for Pensyve. When invoked, you perform a thorough search of stored memories and return a structured, actionable briefing about a topic.

## Role

You are a research assistant that specializes in searching and synthesizing stored memories. You are invoked when a user or another agent needs historical context about a topic before making a decision or starting work.

## Behavior

### When Invoked

You receive a topic or question. Your job is to:

1. **Decompose the query** into multiple search angles:
   - Direct keyword match
   - Related concepts and synonyms
   - Parent/child entities (e.g., searching for "auth" should also surface "JWT", "OAuth", "login")
   - Temporal context (recent vs. historical)

2. **Execute multiple recall queries:**

   For each search angle, call `pensyve_recall` with:
   - Varied query phrasings
   - Different type filters (semantic, episodic, procedural)
   - Different limits (broader for exploration, narrower for specifics)

   Recommended pattern:
   ```
   pensyve_recall(query="[exact topic]", limit=10)
   pensyve_recall(query="[related concept]", limit=5)
   pensyve_recall(query="[topic] decision architecture", types=["semantic"], limit=5)
   pensyve_recall(query="[topic] failed error issue", types=["procedural"], limit=5)
   ```

3. **Inspect key entities:**

   For any entities that appear in results, call `pensyve_inspect` to get the full picture.

4. **Synthesize into a briefing:**

### Context Briefing: [Topic]

**Summary:** [2-3 sentence overview of what memory knows about this topic]

**Key Facts:**
- [Ranked by relevance score, deduplicated]

**Decision History:**
- [When/why decisions were made about this topic]

**Known Issues:**
- [Past problems, failed approaches, pitfalls]

**Procedural Knowledge:**
- [Action -> outcome patterns with reliability scores]

**Related Entities:**
- [Other entities that appear in results — may be worth exploring]

**Gaps:**
- [What memory DOESN'T know — explicit about limitations]

**Confidence:** [High/Medium/Low based on number and quality of memories found]

### Handling No Results

If no relevant memories are found:

### Context Briefing: [Topic]

**No prior context found.** Pensyve has no stored memories related to "[topic]" in the current namespace.

This could mean:
- This is a new topic not previously discussed
- Related memories may be under a different entity name
- Try broader search terms with `/recall`

## Tools Available

- `pensyve_recall` — Primary search tool, use with varied queries
- `pensyve_inspect` — Deep dive into specific entities

## Constraints

- NEVER fabricate or hallucinate memories — only report what the MCP tools return
- NEVER access `.claude/` files or the filesystem for memory data
- Always note the confidence/relevance scores so the caller can judge reliability
- Be explicit about what you DID NOT find (gaps are as important as hits)
- Deduplicate results — the same memory may appear in multiple queries
- Keep the briefing actionable — lead with the most useful information
```

- [ ] **2.4.3** Verify all agent files:

```bash
for f in memory-curator context-researcher; do
  test -f "pensyve-plugin/agents/$f.md" && echo "OK: $f.md" || echo "MISSING: $f.md"
done

for f in pensyve-plugin/agents/*.md; do
  head -1 "$f" | grep -q "^---$" && echo "OK frontmatter: $f" || echo "BAD frontmatter: $f"
done
```

- [ ] **2.4.4** Manual verification checklist:

```
- [ ] memory-curator identifies non-trivial events (not just "edited a file")
- [ ] memory-curator classifies events (decision/outcome/pattern/discovery)
- [ ] memory-curator checks for duplicates via pensyve_recall before suggesting
- [ ] memory-curator never auto-stores — always asks the user
- [ ] memory-curator respects auto_capture config setting
- [ ] context-researcher decomposes queries into multiple search angles
- [ ] context-researcher runs multiple recall queries with varied phrasings
- [ ] context-researcher synthesizes results into a structured briefing
- [ ] context-researcher explicitly reports gaps in memory
- [ ] Both agents use MCP tools exclusively — no filesystem access
- [ ] Both agents have proper YAML frontmatter
```

- [ ] **2.4.5** Commit the agents:

```bash
git add pensyve-plugin/agents/
git commit -m "feat(plugin): add memory-curator and context-researcher agents

memory-curator: background agent that monitors sessions for memorable
events (decisions, outcomes, patterns) and suggests storage with user
confirmation. Only active when auto_capture is enabled.

context-researcher: on-demand agent that performs multi-angle memory
search and returns structured briefings with confidence scores and
explicit gap reporting."
```

---

## Task 2.5: Hooks (Sprint 3)

**Goal:** Create 4 lifecycle hooks that integrate memory operations into the Claude Code session lifecycle.

**Files:**
- Create: `pensyve-plugin/hooks/session-start.md`
- Create: `pensyve-plugin/hooks/stop.md`
- Create: `pensyve-plugin/hooks/pre-compact.md`
- Create: `pensyve-plugin/hooks/user-prompt-submit.md`

### Steps

- [ ] **2.5.1** Create `pensyve-plugin/hooks/session-start.md`:

```markdown
---
name: session-start
description: Load relevant memories at the start of a Claude Code session
event: SessionStart
---

# Session Start Hook

Fires when a Claude Code session begins. Loads relevant memories to prime the session with cross-session context.

## Behavior

### Step 1: Check Configuration

Read `pensyve-plugin.local.md` for the `context_loading` setting:
- **"off"**: Exit immediately. Do not load any memories or produce output.
- **"summary"** (default): Load a concise summary of relevant memories.
- **"full"**: Load comprehensive memory context.

### Step 2: Determine Context

Identify the current project/namespace:
1. Use the `PENSYVE_NAMESPACE` environment variable if set
2. Otherwise, use the current working directory name as the namespace

### Step 3: Load Memories (if not "off")

Call `pensyve_recall` with a broad query based on the namespace/project name:
- `query`: "[namespace] recent decisions issues patterns"
- `limit`: 10 for summary mode, 25 for full mode
- No type filter (get all types)

### Step 4: Present Context

**For "summary" mode:**

> **Pensyve:** [N] memories loaded for `[namespace]`. Key context:
> - [Top 3 most relevant facts, one line each]
> - [Any active issues or warnings from procedural memory]

**For "full" mode:**

Present the full output as described in the `context-loader` skill.

### Step 5: Start Episode (Optional)

If `context_loading` is not "off", silently start an episode to track the session:
- Call `pensyve_episode_start` with participants: ["claude-code", "[namespace]"]
- Store the episode_id for use by the Stop hook

## Performance

- This hook MUST complete quickly (< 2 seconds for summary mode)
- Use a single recall query, not multiple
- If the MCP server is unavailable, log a warning and continue — never block session start

## Constraints

- NEVER read `.claude/` memory files
- NEVER slow down session startup significantly
- Respect the user's context_loading preference
- If the pensyve-mcp server is not running, fail silently with a brief note
```

- [ ] **2.5.2** Create `pensyve-plugin/hooks/stop.md`:

```markdown
---
name: stop
description: Extract and offer to store decisions and outcomes when a task completes
event: Stop
---

# Stop Hook

Fires when a task completes (Stop event) or a sub-agent finishes (SubagentStop). Analyzes the completed work to identify decisions and outcomes worth storing.

## Behavior

### Step 1: Analyze the Completed Work

Review the conversation since the last stop event (or session start) to identify:

1. **Decisions made** — Architecture choices, technology selections, approach changes
2. **Outcomes** — What succeeded, what failed, what was learned
3. **Procedural patterns** — Reproducible action -> outcome sequences

Apply the same significance filter as the `session-memory` skill:
- Skip routine edits, standard commands, trivial changes
- Focus on items that would help a future session

### Step 2: Filter for Quality

Only proceed if at least one significant item was identified. If the completed work was routine (e.g., a simple file edit, a lint fix), do nothing — exit silently.

### Step 3: Present for Confirmation

If significant items were found:

**Pensyve detected [N] memorable event(s) from this task:**

1. **[type]:** [description] → `[entity]`
2. ...

Store these memories? (yes/no/select)

### Step 4: Store Confirmed Items

For each confirmed item, call `pensyve_remember`:
- `entity`: The relevant entity
- `fact`: The fact text
- `confidence`: Based on type (decision: 0.9, outcome: 0.8, pattern: 0.7)

### Step 5: Close Episode (if one is active)

If a session episode was started by the SessionStart hook:
- Call `pensyve_episode_end` with the stored episode_id
- Set outcome based on the task result (success/failure/partial)

## Significance Threshold

To avoid nagging the user with trivial suggestions, only trigger the confirmation prompt when:
- At least one decision or non-trivial outcome was identified
- The work involved more than simple edits (debugging, architecture, investigation, etc.)
- The conversation was more than a few exchanges (very short interactions are usually routine)

## Constraints

- NEVER auto-store — always ask the user
- NEVER read or write `.claude/` memory files
- Keep suggestions concise — max 5 items per stop event
- If the user dismisses suggestions, remember that preference for the session (don't re-suggest the same items)
- Fail silently if the MCP server is unavailable
```

- [ ] **2.5.3** Create `pensyve-plugin/hooks/pre-compact.md`:

```markdown
---
name: pre-compact
description: Persist in-flight episode data before context window compaction
event: PreCompact
---

# Pre-Compact Hook

Fires before Claude Code compresses the context window. Ensures any in-flight episode data is persisted to Pensyve before context is lost.

## Behavior

### Step 1: Check for Active Episode

Determine if there is an active episode (started by SessionStart hook or user command).

### Step 2: Extract Key Context

Before context is compacted, extract from the current conversation:

1. **Active episode summary** — A brief summary of what has been discussed/accomplished since the episode started
2. **Pending decisions** — Any decisions that were discussed but not yet stored
3. **Current task state** — What was being worked on (for continuity after compaction)

### Step 3: Store Episode Context

Call `pensyve_remember` to persist critical context that would otherwise be lost:
- `entity`: The project/namespace entity
- `fact`: "Pre-compaction snapshot: [summary of current work state, pending decisions, active investigation threads]"
- `confidence`: 0.7 (this is a snapshot, not a confirmed decision)

### Step 4: Do NOT Close the Episode

The episode remains open — compaction doesn't end the session. The episode will be closed by the Stop hook when the task actually completes.

## Why This Matters

Context compaction can cause Claude Code to lose track of:
- What was being debugged and what hypotheses were tested
- Decisions that were verbally agreed but not yet stored
- The "why" behind the current approach

This hook captures that context so it can be recalled after compaction.

## Constraints

- Keep the stored memory concise — this is a snapshot, not a full transcript
- NEVER access `.claude/` files
- Execute quickly — compaction should not be delayed significantly
- Only store if there is meaningful context to preserve (don't store "nothing notable happened")
- Mark pre-compaction memories with low-ish confidence (0.7) since they're automated snapshots
```

- [ ] **2.5.4** Create `pensyve-plugin/hooks/user-prompt-submit.md`:

```markdown
---
name: user-prompt-submit
description: Optionally enrich user prompts with relevant memory context before processing
event: UserPromptSubmit
---

# User Prompt Submit Hook

Fires when the user submits a prompt. Optionally enriches the prompt with relevant memory context. **Disabled by default** — must be explicitly enabled via `prompt_enrichment: true` in `pensyve-plugin.local.md`.

## Behavior

### Step 1: Check Configuration

Read `pensyve-plugin.local.md` for `prompt_enrichment`:
- **false** (default): Exit immediately. Do nothing.
- **true**: Proceed with enrichment.

### Step 2: Analyze the Prompt

Determine if the prompt would benefit from memory context:

**Enrich when the prompt involves:**
- Architecture or design decisions ("how should we...", "what's the best way to...")
- Debugging or troubleshooting ("why is this failing...", "this error...")
- Referencing past work ("last time we...", "we decided to...", "what did we...")
- Refactoring ("refactor", "restructure", "reorganize")

**Do NOT enrich when the prompt is:**
- A simple command ("run tests", "lint this file")
- A question about the current file content ("what does this function do")
- A request to write new code with no historical context needed
- Very short (< 10 words)

### Step 3: Quick Recall

If enrichment is warranted, call `pensyve_recall`:
- `query`: The user's prompt text (or key phrases from it)
- `limit`: 3 (keep it lightweight)
- Timeout: 1 second max — never delay the user

### Step 4: Inject Context

If relevant memories are found (score > 0.3), prepend to the prompt processing:

> **Pensyve context:** Prior memories relevant to this prompt:
> - [memory 1]
> - [memory 2]
> - [memory 3]

This context is injected into the agent's reasoning, not shown to the user as separate output.

### Step 5: No Results

If no relevant memories are found, or scores are all < 0.3, proceed without enrichment. Do not inform the user that enrichment was attempted.

## Performance Requirements

This hook runs on EVERY user prompt when enabled. It MUST be:
- **Fast**: < 1 second total execution time
- **Lightweight**: Single recall query, max 3 results
- **Non-blocking**: If MCP server is slow, skip enrichment entirely
- **Silent**: No user-visible output unless memories are injected

## Why Disabled by Default

- Adds latency to every prompt
- Can inject irrelevant context if memory quality is low
- Users should build up a quality memory corpus first (via /remember, session-memory skill)
- Power users enable this after they trust their stored memories

## Constraints

- NEVER enabled by default — requires explicit opt-in
- NEVER access `.claude/` files
- NEVER show "no memories found" messages — fail silently
- Keep enrichment to 3 memories max to avoid context bloat
- Respect the 1-second timeout strictly
```

- [ ] **2.5.5** Verify all hook files:

```bash
for f in session-start stop pre-compact user-prompt-submit; do
  test -f "pensyve-plugin/hooks/$f.md" && echo "OK: $f.md" || echo "MISSING: $f.md"
done

for f in pensyve-plugin/hooks/*.md; do
  head -1 "$f" | grep -q "^---$" && echo "OK frontmatter: $f" || echo "BAD frontmatter: $f"
done
```

- [ ] **2.5.6** Verify hook event types are correct:

```
- [ ] session-start.md: event: SessionStart
- [ ] stop.md: event: Stop
- [ ] pre-compact.md: event: PreCompact
- [ ] user-prompt-submit.md: event: UserPromptSubmit
```

- [ ] **2.5.7** Manual verification checklist:

```
- [ ] SessionStart hook respects context_loading config (off/summary/full)
- [ ] SessionStart hook completes quickly (< 2 seconds for summary)
- [ ] SessionStart hook fails silently if MCP server unavailable
- [ ] Stop hook only triggers for significant events (not routine edits)
- [ ] Stop hook never auto-stores — always asks user
- [ ] Stop hook closes episode if one was started
- [ ] PreCompact hook captures in-flight context before compaction
- [ ] PreCompact hook does NOT close the episode
- [ ] UserPromptSubmit hook is OFF by default (requires prompt_enrichment: true)
- [ ] UserPromptSubmit hook respects 1-second timeout
- [ ] UserPromptSubmit hook fails silently on no results
- [ ] All hooks use MCP tools exclusively — no .claude/ access
- [ ] All hooks have proper YAML frontmatter with name, description, event
```

- [ ] **2.5.8** Commit the hooks:

```bash
git add pensyve-plugin/hooks/
git commit -m "feat(plugin): add 4 lifecycle hooks for session-aware memory

SessionStart: loads relevant memories at session start (configurable:
off/summary/full). Stop: extracts decisions/outcomes after task
completion with user confirmation. PreCompact: persists in-flight
episode data before context compaction. UserPromptSubmit: optionally
enriches prompts with memory context (disabled by default)."
```

---

## Task 2.6: Marketplace Packaging (Sprint 3)

**Goal:** Finalize the plugin for marketplace distribution — README, settings documentation, final manifest validation.

**Files:**
- Create: `pensyve-plugin/README.md`
- Update: `pensyve-plugin/plugin.json` (final version bump, verify all components)
- Verify: All component files

### Steps

- [ ] **2.6.1** Create `pensyve-plugin/README.md` — the marketplace-facing documentation:

```markdown
# Pensyve — Cross-Session Memory for Claude Code

Pensyve gives Claude Code a persistent, cognitive memory layer that spans across sessions. It remembers your decisions, learned patterns, debugging outcomes, and project context — so you never repeat the same investigation twice.

## What It Does

- **Remembers decisions and their reasoning** across coding sessions
- **Recalls relevant context** when you start a new session or switch tasks
- **Tracks outcomes** — what worked, what failed, and why
- **Consolidates knowledge** — promotes repeated patterns to long-term facts, decays stale information
- **Never forgets the hard-won lessons** from debugging sessions

## How It Works

Pensyve runs a local memory engine (Rust-based, SQLite-backed) that stores memories as embeddings with multi-signal retrieval. It connects to Claude Code via MCP, giving the AI access to 6 memory tools. The plugin adds slash commands, workflow skills, background agents, and lifecycle hooks on top.

```
Your coding session
    ↓
Claude Code + Pensyve Plugin
    ↓ (MCP protocol)
pensyve-mcp server
    ↓
SQLite + ONNX embeddings + vector index
```

## Quick Start

### Prerequisites

1. **Build the pensyve-mcp binary:**
   ```bash
   git clone https://github.com/major7apps/pensyve
   cd pensyve
   cargo build --release -p pensyve-mcp
   ```

2. **Add the binary to your PATH** or set the full path in `.mcp.json`

### Install the Plugin

Install from the Claude Code marketplace, or manually:

```bash
# Copy the plugin to Claude Code's plugin directory
cp -r pensyve-plugin ~/.claude/plugins/pensyve
```

### Configure (Optional)

Copy `pensyve-plugin.local.md` to your project root and edit:

```yaml
namespace: "my-project"        # Scope memories to this project
auto_capture: false            # Enable background memory curation
consolidation_frequency: manual
context_loading: summary       # Load memories at session start
prompt_enrichment: false       # Enrich prompts with memory (power user)
```

## Commands

| Command | Description |
|---------|-------------|
| `/remember <fact>` | Store a fact, decision, or pattern |
| `/recall <query>` | Search memories by semantic similarity |
| `/forget <entity>` | Delete all memories for an entity |
| `/inspect [entity]` | View all memories grouped by type |
| `/consolidate` | Trigger memory consolidation cycle |
| `/memory-status` | Show namespace statistics |

## Skills

| Skill | When to Use |
|-------|-------------|
| `session-memory` | End of a work session — captures decisions and outcomes |
| `memory-informed-refactor` | Before refactoring — loads relevant prior context |
| `context-loader` | Session start or context switch — loads historical context |
| `memory-review` | Periodic — finds stale facts, contradictions, cleanup opportunities |

## Agents

| Agent | Mode | Purpose |
|-------|------|---------|
| `memory-curator` | Background | Monitors sessions, suggests memorable events |
| `context-researcher` | On-demand | Deep memory search, returns structured briefings |

## Hooks

| Hook | Event | Behavior |
|------|-------|----------|
| Session Start | SessionStart | Loads relevant memories (configurable) |
| Stop | Stop | Extracts decisions/outcomes after tasks |
| Pre-Compact | PreCompact | Persists in-flight data before context compression |
| Prompt Enrichment | UserPromptSubmit | Enriches prompts with memory (off by default) |

## Design Philosophy

- **CLAUDE.md owns static conventions** — project setup, commands, architecture
- **Pensyve owns dynamic memory** — decisions, outcomes, patterns, context
- **Never duplicates** — Pensyve will not store what belongs in CLAUDE.md
- **Always asks** — no memory is stored without user confirmation
- **Local-first** — all data stays on your machine in SQLite

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PENSYVE_NAMESPACE` | `default` | Memory namespace (use project name for isolation) |
| `PENSYVE_PATH` | `~/.pensyve/default` | Storage directory path |

## License

Apache 2.0
```

- [ ] **2.6.2** Final validation of `pensyve-plugin/plugin.json` — verify all component paths resolve:

```bash
# Verify every file referenced in plugin.json exists
cd pensyve-plugin

# Commands
for f in commands/remember.md commands/recall.md commands/forget.md \
         commands/inspect.md commands/consolidate.md commands/memory-status.md; do
  test -f "$f" && echo "OK: $f" || echo "MISSING: $f"
done

# Skills
for f in skills/session-memory.md skills/memory-informed-refactor.md \
         skills/context-loader.md skills/memory-review.md; do
  test -f "$f" && echo "OK: $f" || echo "MISSING: $f"
done

# Agents
for f in agents/memory-curator.md agents/context-researcher.md; do
  test -f "$f" && echo "OK: $f" || echo "MISSING: $f"
done

# Hooks
for f in hooks/session-start.md hooks/stop.md hooks/pre-compact.md \
         hooks/user-prompt-submit.md; do
  test -f "$f" && echo "OK: $f" || echo "MISSING: $f"
done

cd ..
```

- [ ] **2.6.3** Validate YAML frontmatter consistency across all components:

```bash
# Every .md file in the plugin (except README and config) should have frontmatter
for f in pensyve-plugin/commands/*.md pensyve-plugin/skills/*.md \
         pensyve-plugin/agents/*.md pensyve-plugin/hooks/*.md; do
  echo "--- $f ---"
  # Extract frontmatter (between first two --- lines)
  sed -n '1,/^---$/p' "$f" | head -5
  echo ""
done
```

- [ ] **2.6.4** Verify the complete plugin structure:

```bash
find pensyve-plugin -type f | sort
```

Expected output:
```
pensyve-plugin/.mcp.json
pensyve-plugin/README.md
pensyve-plugin/agents/context-researcher.md
pensyve-plugin/agents/memory-curator.md
pensyve-plugin/commands/consolidate.md
pensyve-plugin/commands/forget.md
pensyve-plugin/commands/inspect.md
pensyve-plugin/commands/memory-status.md
pensyve-plugin/commands/recall.md
pensyve-plugin/commands/remember.md
pensyve-plugin/hooks/pre-compact.md
pensyve-plugin/hooks/session-start.md
pensyve-plugin/hooks/stop.md
pensyve-plugin/hooks/user-prompt-submit.md
pensyve-plugin/pensyve-plugin.local.md
pensyve-plugin/plugin.json
pensyve-plugin/skills/context-loader.md
pensyve-plugin/skills/memory-informed-refactor.md
pensyve-plugin/skills/memory-review.md
pensyve-plugin/skills/session-memory.md
```

Total: 18 files (1 manifest, 1 MCP config, 1 README, 1 config template, 6 commands, 4 skills, 2 agents, 4 hooks)

- [ ] **2.6.5** End-to-end integration test (manual):

```
Pre-requisites:
  - [ ] pensyve-mcp binary is built: `cargo build --release -p pensyve-mcp`
  - [ ] Binary is on PATH or .mcp.json has the full path

Test sequence:
  1. [ ] Install the plugin into Claude Code
  2. [ ] Verify MCP tools are available (6 tools: pensyve_recall, pensyve_remember,
         pensyve_episode_start, pensyve_episode_end, pensyve_forget, pensyve_inspect)
  3. [ ] Test /remember: `/remember The project uses SQLite for local storage`
     - Verify: memory stored, entity created, confirmation shown
  4. [ ] Test /recall: `/recall what database`
     - Verify: returns the SQLite memory with relevance score
  5. [ ] Test /inspect: `/inspect project-name`
     - Verify: shows the stored memory in tabular format
  6. [ ] Test /memory-status
     - Verify: shows namespace stats with at least 1 memory
  7. [ ] Test /forget: `/forget project-name`
     - Verify: asks for confirmation, then deletes
  8. [ ] Test session-memory skill at end of session
     - Verify: identifies events, asks for confirmation
  9. [ ] Test context-loader skill
     - Verify: loads and presents relevant context
  10. [ ] Verify SessionStart hook fires and loads context
  11. [ ] Verify Stop hook detects significant events
  12. [ ] Verify UserPromptSubmit hook is disabled by default
```

- [ ] **2.6.6** Commit the packaging:

```bash
git add pensyve-plugin/README.md
git commit -m "feat(plugin): add marketplace README and finalize packaging

Complete the Pensyve Claude Code plugin with marketplace documentation
covering installation, configuration, all 6 commands, 4 skills, 2 agents,
and 4 hooks. Plugin is ready for marketplace submission."
```

- [ ] **2.6.7** Final commit — tag the plugin as complete:

```bash
git tag -a plugin-v0.1.0 -m "Pensyve Claude Code Plugin v0.1.0

Complete plugin with:
- 6 slash commands (remember, recall, forget, inspect, consolidate, memory-status)
- 4 skills (session-memory, memory-informed-refactor, context-loader, memory-review)
- 2 agents (memory-curator, context-researcher)
- 4 hooks (session-start, stop, pre-compact, user-prompt-submit)
- MCP integration pointing to pensyve-mcp binary (6 tools)
- Configuration via pensyve-plugin.local.md"
```

---

## MCP Tools Reference

The plugin wraps these 6 MCP tools exposed by the `pensyve-mcp` binary:

| Tool | Parameters | Returns |
|------|-----------|---------|
| `pensyve_recall` | `query`, `entity?`, `types?`, `limit?` | Ranked array of memories with scores |
| `pensyve_remember` | `entity`, `fact`, `confidence?` | Stored memory object |
| `pensyve_episode_start` | `participants` | `episode_id`, started_at |
| `pensyve_episode_end` | `episode_id`, `outcome?` | memories_created count |
| `pensyve_forget` | `entity`, `hard_delete?` | forgotten_count |
| `pensyve_inspect` | `entity`, `memory_type?`, `limit?` | Array of memories with stats |

All tools communicate over stdio using the MCP protocol. The plugin never bypasses MCP to access storage directly.

---

## Configuration Reference

`pensyve-plugin.local.md` YAML frontmatter settings:

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `namespace` | string | `"default"` | Memory namespace — use project name for isolation |
| `auto_capture` | boolean | `false` | Enable memory-curator background agent |
| `consolidation_frequency` | string | `"manual"` | When to consolidate: `manual`, `session_end`, `daily` |
| `context_loading` | string | `"summary"` | Session start context: `off`, `summary`, `full` |
| `prompt_enrichment` | boolean | `false` | Enable UserPromptSubmit hook |
