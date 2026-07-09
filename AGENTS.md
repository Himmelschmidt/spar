# spar — for outer agents

Multi-agent coding product. Humans open `spar` (TUI). You drive it via CLI.

```bash
spar skills list
spar skills get core          # full operator skill (read this)
spar doctor --json
spar plan -t "..." --providers cli:claude,cli:grok --json [--dry-run] [--big]
spar approve <run_id> --json  # exit 2 = human gate until approve
spar implement --run <id> --providers cli:claude,cli:grok,cli:agy --json   # SAME run id
spar status [run_id] --json
spar wait <run_id> --follow --json
spar logs <run_id> [slot] -f
spar bus send <run_id> -m "..."
spar reconcile <run_id>       # arena merge path
```

**`--providers` is required** on plan/implement/run. Examples: `claude`, `cli:grok`, `api:openai` (mixable).

**Exit codes:** `0` ok · `1` fail · `2` human gate · `3` stuck · `4` quota  

State: `.spar/runs/<id>/` (`state.json`, `events.jsonl`, `bus/`, `logs/`).  
Ship is draft PR only — never merge. Worktrees only for coding slots.
