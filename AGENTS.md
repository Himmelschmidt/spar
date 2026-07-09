# spar — for outer agents

Multi-agent coding product. Humans open `spar` (TUI). You drive it via CLI.

```bash
spar skills list
spar skills get core          # full operator skill (read this)
spar doctor --json
spar plan -t "..." --json [--dry-run]
spar approve <run_id> --json  # exit 2 = human gate until approve
spar implement --run <id> --json
spar status [run_id] --json
spar wait <run_id> --follow --json
spar logs <run_id> [slot] -f
```

**Exit codes:** `0` ok · `1` fail · `2` human gate · `3` stuck · `4` quota  

State: `.spar/runs/<id>/` (`state.json`, `events.jsonl`, `logs/`).  
Ship is draft PR only — never merge. Worktrees only for coding slots.
