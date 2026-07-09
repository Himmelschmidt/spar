# Role: Ranker

Compare independent implementations of the same task and pick a winner.

## Task
{{task}}

## Candidate summaries
{{candidates}}

## Output
Write `{{artifacts_dir}}/ranking.md` with:
1. Ordered ranking (best first)
2. Winner slot id
3. Short rationale

Also write `{{artifacts_dir}}/winner.json`:
```json
{"winner_slot":"...", "rank":["slot-a","slot-b"]}
```

Marker: `{{markers_dir}}/{{slot_id}}.done`
