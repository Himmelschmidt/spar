# spar core skill

**spar** is a multi-agent coding product (fleet TUI + headless CLI). Outer agents drive it via CLI; humans use the TUI.

## Discovery

```bash
spar skills list
spar skills get core
spar doctor [--json]
spar provider list [--json]
```

## Default surfaces

| Who | How |
|-----|-----|
| Human | `spar` (no subcommand) → product TUI in current git repo |
| Outer agent | subcommands + `--json` + exit codes |

## Workflows

```bash
# Plan (ends HumanGate / awaiting_plan_approval)
spar plan -t "describe the work" [--providers claude,grok] [--dry-run] [--json] [--detach]

spar approve <run_id> [--json]
spar reject <run_id> [--reason "..."] [--json]

# Implement from approved plan run, plan file, or direct task
spar implement --run <run_id> [--dry-run] [--json] [--detach]
spar implement -t "small task" [--dry-run]

# Named workflows
spar run --workflow loop|arena|roles|peer -t "..." [--dry-run]

spar confirm <run_id> [--winner <slot>]   # arena winner
spar ship <run_id> --confirm             # record + draft PR (never merges)
spar cleanup <run_id> [--purge]
```

## Observe

```bash
spar status [run_id] [--json]
spar wait <run_id> [--timeout 2h] [--follow] [--json]
spar logs <run_id> [slot] [-f|--follow]
```

- Run state: `.spar/runs/<id>/state.json`
- Events: `.spar/runs/<id>/events.jsonl`
- Logs: `.spar/runs/<id>/logs/<slot>.log`

## Exit codes (stable)

| Code | Meaning |
|------|---------|
| 0 | Success / terminal ok (e.g. plan approved, done) |
| 1 | Failure |
| 2 | Human gate (approve plan / winner / ship) |
| 3 | Stuck / escalated / wait timeout |
| 4 | No usable providers (quota/pause) |

Always branch on exit code; use `--json` for machine-readable state.

## Providers

```bash
spar provider list [--json]
spar provider pause <name> [--until 1h|RFC3339]
spar provider resume <name>
```

Workers: subscription CLIs (`claude`, `grok`, `agy`) via native-cli backend. Prefer headless; tmux only as namespaced `spar-<run_id>` opt-in.

## Rules of the road

- One run id plan → implement → ship (product goal; avoid inventing parallel orchestration).
- Coding slots always use git worktrees; never check out feature branches on the primary tree.
- Ship is draft PR only — never merge.
- State lives under `.spar/` in the project root.

## Prefer dry-run while learning

```bash
spar plan -t "..." --dry-run --json
spar implement --run <id> --dry-run --json
```
