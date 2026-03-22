# Pensyve Open Evaluation Dataset

A community-contributed evaluation dataset for agent memory systems.

## Format

- `scenarios.json` — MemoryArena-format scenarios (preference, mistake avoidance, procedure, contradiction)
- `conversations.json` — LoCoMo-format conversations for temporal/multi-hop evaluation
- `queries.json` — Evaluation queries with gold answers

## Contributing

Add scenarios to `scenarios.json` following the schema:
```json
{
  "scenario_id": "community-001",
  "category": "preference|mistake_avoidance|procedure_selection|contradiction",
  "setup_messages": [{"role": "user|assistant", "content": "..."}],
  "test_query": "...",
  "correct_action": "expected keyword",
  "incorrect_action": "wrong keyword"
}
```

## License

Apache 2.0
