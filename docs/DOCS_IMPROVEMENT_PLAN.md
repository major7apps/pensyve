# Documentation Improvement Plan: First-Timer Grip

**Date:** 2026-03-26
**Goal:** Make first-time visitors convert to users within 5 minutes by leading with outcomes, not features.

---

## The Core Problem

Pensyve's docs currently say *what it is* (universal memory runtime) and *what it has* (8-signal fusion, FSRS decay, procedural memory). But first-timers don't care about features. They care about outcomes:

- "My agent forgets everything between sessions"
- "My chatbot asks the same questions every time"
- "I have no idea which tool-calling strategies actually work"
- "Users get frustrated repeating themselves to my AI"

The docs need to start with these pain points and show Pensyve resolving them — then let curiosity pull people deeper into the architecture.

---

## Principle: Outcomes Before Mechanisms

Every section should answer "what does this let me *do*?" before "how does this *work*?"

| Current (mechanism-first) | Proposed (outcome-first) |
|---|---|
| "8-signal fusion retrieval" | "Your agent finds the right memory even when the user phrases things differently than last time" |
| "FSRS forgetting curve with reinforcement" | "Important memories stay sharp. Irrelevant ones fade naturally — no manual cleanup" |
| "Procedural memory with Bayesian reliability" | "Your agent learns which approaches actually work and stops repeating mistakes" |
| "Episodic memory from conversations" | "Your agent remembers not just facts, but the full story of what happened and why" |
| "Offline-first with SQLite" | "Works on your laptop right now. No API keys, no cloud signup, no billing page" |

---

## Priority 1: README "Before/After" (30 min, highest impact)

Add a scenario to the very top of README, before the feature list. Show the pain, then the fix:

```
### Without memory
User: "I prefer dark mode"
Agent: "Got it!"
[next session]
User: "Update my settings"
Agent: "What settings would you like to change?"
User: "I ALREADY TOLD YOU I want dark mode" 😤

### With Pensyve
User: "I prefer dark mode"
Agent: "Got it!" → pensyve.remember(entity, "prefers dark mode")
[next session]
User: "Update my settings"
Agent: → pensyve.recall("settings preferences")
Agent: "I remember you prefer dark mode. Want me to apply that now?"
User: 😊
```

This should take 5 lines of real code alongside the narrative. The outcome is immediately clear: your agent stops being amnesiac.

---

## Priority 2: Lead with Episodes in Quickstart (30 min)

Episodes are Pensyve's killer feature — multi-turn conversation capture with outcome tracking. But they appear *after* `recall()` and `remember()` in the docs. Flip the order:

1. Show an episode capturing a real interaction
2. Show the agent recalling that interaction later
3. Show outcome tracking ("this approach worked")
4. *Then* introduce `remember()` for explicit facts and `recall()` for search

The outcome: "Your agent doesn't just remember facts — it remembers *conversations* and *what worked*."

---

## Priority 3: Recipes/Cookbook (2-3 hrs)

Create `docs/RECIPES.md` with 5-6 outcome-driven patterns. Each recipe: problem statement, 10-15 lines of code, expected behavior.

### Recipe ideas (each named by the outcome):

**1. "My agent remembers users across sessions"**
- Create entity per user, remember preferences, recall on reconnect
- Outcome: personalized experience without user repeating themselves

**2. "My agent learns which strategies work"**
- Episode with outcome tracking, procedural memory query
- Outcome: agent stops retrying failed approaches

**3. "My chatbot has conversation continuity"**
- Episode capture in a chat loop, recall context at start of new session
- Outcome: users can say "remember when we talked about X?" and get a real answer

**4. "My agent shares knowledge between tools"**
- Semantic memory from one tool's output used by another tool's input
- Outcome: multi-agent systems that don't silo information

**5. "My agent's memory stays clean without manual pruning"**
- Consolidation loop, FSRS decay in practice, archival
- Outcome: storage doesn't grow unbounded, relevant memories stay fresh

**6. "I added memory to my existing LangChain/CrewAI agent"**
- Drop-in integration, 3 lines of config
- Outcome: existing agent gains persistence with minimal code change

---

## Priority 4: Competitive Table Rewrite (1 hr)

Replace the checkmark matrix with an outcome-oriented comparison:

| What you need | Pensyve | Mem0 | Zep | Honcho |
|---|---|---|---|---|
| Works offline, no cloud required | Yes — SQLite, runs on your laptop | No — cloud API | No — requires server | No — cloud API |
| Agent learns from outcomes | Yes — procedural memory tracks what works | No | No | No |
| Memories fade naturally | Yes — FSRS forgetting curve | No — manual cleanup | Basic TTL | No |
| Finds memories by meaning, not just keywords | Yes — 8-signal fusion (vector + BM25 + graph + intent + 4 more) | Vector only | Vector + temporal | Vector only |
| Multi-turn conversation capture | Yes — episodes with outcome tracking | Basic | Yes | Yes |
| Framework agnostic | Yes — Python, TypeScript, Go, MCP, REST, CLI | Python SDK | Python/JS | Python |

Each row answers a *need*, not a feature checkbox.

---

## Priority 5: End-to-End Tutorial (2-3 hrs)

Write `docs/TUTORIAL_DISCORD_BOT.md` — "Build a Discord bot with persistent memory in 15 minutes."

Source material: the Gandalf Bot we just built in `~/workspace/gandalf-bot/`. Simplified version:
1. Create a bot, install deps (2 min)
2. Wire up Pensyve — entity per user, episode per conversation (3 min)
3. Add recall to inject memory into prompts (3 min)
4. Run it, talk to it, watch it remember (2 min)
5. Come back tomorrow — it still remembers (the "aha" moment)

The tutorial's job is to deliver the emotional payoff: "holy shit, it actually remembers."

---

## Priority 6: Landing Page (half day)

Even a single-page site at pensyve.com with:
- Hero: "Your AI agents forget everything. Pensyve fixes that."
- 3 outcome blocks with code snippets
- Architecture diagram (visual, not ASCII)
- "Get started in 5 minutes" button → GETTING_STARTED.md
- Competitive positioning (outcome-oriented table from Priority 4)

This is lower priority than the README/recipes work but important for discoverability beyond GitHub.

---

## Measuring Success

The docs are working when:
- A developer goes from "what is this?" to running the 5-line demo in under 5 minutes
- They can describe Pensyve's value in one sentence tied to an outcome, not a feature
- They pick one of the recipes and have it working in their own project within 30 minutes
- They tell someone else about it by describing the *problem it solved*, not the tech stack

---

## Implementation Order

| Priority | Effort | Impact | Description |
|---|---|---|---|
| 1 | 30 min | Very high | Before/after scenario at top of README |
| 2 | 30 min | High | Lead with episodes in quickstart |
| 3 | 2-3 hrs | High | Recipes/cookbook with outcome-driven patterns |
| 4 | 1 hr | Medium | Rewrite competitive table around outcomes |
| 5 | 2-3 hrs | Medium | End-to-end Discord bot tutorial |
| 6 | 4-6 hrs | Medium | Landing page at pensyve.com |
