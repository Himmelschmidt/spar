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

## Dual backend

Providers are `cli:name` (subscription CLIs) or `api:name` (OpenAI-compatible SDKs):

```bash
# native CLIs (default bare names = cli)
spar plan -t "..." --providers cli:claude,cli:grok --dry-run

# mix CLI + API slots
spar implement -t "..." --providers cli:cli:claude,api:openai --dry-run
spar run --workflow arena -t "..." --providers api:xai,cli:claude,cli:grok
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `OPENAI_BASE_URL` / `XAI_BASE_URL` / `*_MODEL`.

## Workflows

**`--providers` is required** for `plan`, `implement`, and `run` (no silent default fleet).

```bash
# Plan (ends HumanGate / awaiting_plan_approval unless autonomy auto-approves)
spar plan -t "describe the work" --providers cli:claude,cli:grok [--big] [--dry-run] [--json] [--detach]

spar approve <run_id> [--json]
spar reject <run_id> [--reason "..."] [--json]

# Implement continues THE SAME run id (plan → implement → ship)
spar implement --run <run_id> --providers cli:claude,cli:grok,cli:agy [--dry-run] [--json] [--detach]
spar implement -t "small task" --providers cli:claude [--dry-run]

# Named workflows
spar run --workflow loop|arena|roles|peer|review -t "..." --providers cli:claude,cli:grok [--dry-run] [--big]

# Independent concurrent multi-review (not peer/split-stack):
spar run --workflow review -t "Review PR #12 for auth bugs" --providers cli:claude,cli:grok

spar confirm <run_id> [--winner <slot>]   # arena winner
spar reconcile <run_id>                  # arena merge-good-parts + review
spar ship <run_id> --confirm             # draft PR (never merges)
spar cleanup <run_id> [--purge]
```

## Swarm bus

```bash
spar bus send <run_id> -m "hello" [--from human] [--to broadcast|slot]
spar bus log <run_id> [--json]
spar bus presence <run_id>
spar bus reserve <run_id> path/to/file --holder <slot>
spar bus release <run_id> path/to/file --holder <slot>
```

Layout: `.spar/runs/<id>/bus/{events.jsonl,agents.jsonl,inbox/,reserves.json,tasks/}`

## Observe

```bash
spar status [run_id] [--json]
spar wait <run_id> [--timeout 2h] [--follow] [--json]
spar logs <run_id> [slot] [-f|--follow]
```

- Run state: `.spar/runs/<id>/state.json`
- Events (orchestrator): `.spar/runs/<id>/events.jsonl`
- Logs: `.spar/runs/<id>/logs/<slot>.log`

## Exit codes (stable)

| Code | Meaning |
|------|---------|
| 0 | Success / terminal ok (e.g. plan approved, done) |
| 1 | Failure |
| 2 | Human gate (approve plan / winner / ship) |
| 3 | Stuck / escalated / wait timeout |
| 4 | No usable providers (quota/pause) |

**`status` is observe-only:** process exit is always `0` if the run loads. Read JSON `exit_code` / `phase` for run state. Use `wait` when you want the process exit coded by gate/stuck/quota.

**`--dry-run`:** stubs agent processes only; writes `.spar/runs/<id>/`. Does **not** create real git worktrees (cwd under `.spar/…/cwd-*`). Live runs create sibling worktrees.

**Providers:** always pass `--providers` explicitly. A single name is fine (`--providers cli:claude`); multiple names cycle across slots.

## Config knobs (`spar.toml`)

```toml
autonomy = "manual" | "semi" | "high" | "full"
message_budget = "none" | "lean" | "normal" | "chatty"
auto_cleanup = false
[gates]
plan = true
winner = true
ship = true
```

## Rules of the road

- One run id plan → implement → ship.
- Coding slots always use git worktrees; never check out feature branches on the primary tree.
- Ship is draft PR only — never merge.
- State lives under `.spar/` in the project root.
