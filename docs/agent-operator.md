# Agent operator contract

How an **outer agent** (Claude, Grok, agy, etc.) should call `agent-swarm`.

## Principles

1. Prefer `--json` and parse stdout.
2. Prefer `--detach` + `wait` / `status` over long blocking calls when you need to stay responsive.
3. Read artifacts from disk under `.swarm/runs/<run-id>/` — do not rely on chat alone.
4. Branch on exit codes, not only stderr text.
5. Never merge to the default branch; shipping is gated.
6. Use `--dry-run` (or `AGENT_SWARM_DRY_RUN=1`) to exercise workflows without live provider CLIs.

## Exit codes

| Code | Constant | When |
|------|----------|------|
| 0 | Success | Command succeeded; or status of a completed/healthy run |
| 1 | Failure | Hard error, rejected plan, failed run |
| 2 | HumanGate | Plan approval, winner confirm, or ship confirm required |
| 3 | Stuck | Policy chain exhausted; needs human |
| 4 | Quota | No usable provider / quota pause (`phase: quota`) |

## Typical Path A (plan → approve → implement)

```bash
agent-swarm plan --task "$TASK" --detach --json
# → { "run_id": "...", "phase": "...", ... }

agent-swarm wait "$RUN_ID" --json
# exit 2 + phase awaiting_plan_approval

# Read plan for the human:
#   .swarm/runs/$RUN_ID/artifacts/plan.md

agent-swarm approve "$RUN_ID" --json

# implement creates a *new* run id (parent plan stays plan_approved).
# Always take run_id from implement's JSON — do not wait on the plan id.
agent-swarm implement --run "$RUN_ID" --detach --json
# → { "run_id": "<IMPL_ID>", "parent_run": "<PLAN_ID>", ... }
# Plan state also records child_run for discovery:
#   agent-swarm status "$RUN_ID" --json  →  .child_run

IMPL_ID=...   # from implement JSON run_id
agent-swarm wait "$IMPL_ID" --json
# exit 2 + awaiting_ship_confirm when ready

agent-swarm ship "$IMPL_ID" --confirm --json
```

**Note:** `exit_code` in JSON is only set when the phase is terminal or a human gate (`null` while in-flight). Prefer `phase` + polling `wait`.

## Typical Path B (autonomous task)

```bash
agent-swarm implement --task "$TASK" --detach --json
# or: agent-swarm run --workflow loop --task "$TASK" --detach --json
# Use the run_id from that command's JSON response:
agent-swarm wait "$RUN_ID" --timeout 2h --json
```

## Arena

```bash
agent-swarm run --workflow arena --task "$TASK" --json
# exit 2 awaiting_winner_confirm
agent-swarm confirm "$RUN_ID" [--winner slot-id] --json
agent-swarm ship "$RUN_ID" --confirm --json
```

## Roles / peer

```bash
agent-swarm run --workflow roles --task "$TASK" --json
agent-swarm run --workflow peer --task "$TASK" --json
# mailbox under .swarm/runs/$RUN_ID/mailbox/
```

## Status JSON (shape)

`agent-swarm status <run-id> --json` returns `RunState`:

```json
{
  "id": "a1b2c3d4",
  "workflow": "plan",
  "phase": "awaiting_plan_approval",
  "task": "...",
  "created_at": "...",
  "updated_at": "...",
  "slots": [
    {
      "id": "planner-claude",
      "provider": "claude",
      "role": "planner",
      "status": "done"
    }
  ],
  "project_root": "/path/to/repo",
  "dry_run": false,
  "gates": { "plan_approved": false, "ship_confirmed": false }
}
```

Machine-oriented start responses (`plan`/`implement`/`run --json`) use:

```json
{
  "run_id": "...",
  "workflow": "...",
  "phase": "...",
  "exit_code": 2,
  "slots": []
}
```

## Wait / logs / attach

```bash
agent-swarm wait "$RUN_ID" --timeout 30m --json
agent-swarm logs "$RUN_ID"
agent-swarm logs "$RUN_ID" impl-claude
agent-swarm attach "$RUN_ID"   # tmux backend only
```

## Doctor

```bash
agent-swarm doctor --json
```

Use before a swarm if you are unsure providers are installed. `ok: false` means git missing or no providers on PATH (dry-run still works for workflow testing).

## Providers / quota

```bash
agent-swarm provider list --json
agent-swarm provider pause claude [--until 1h]
agent-swarm provider resume claude
```

First-class v1 names: `claude`, `grok`, `agy`.

Paused providers are skipped by the scheduler. If **every** selected provider is paused, commands return **exit code 4** (Quota) — they do not silently re-enable paused providers.

## Cleanup

```bash
agent-swarm cleanup "$RUN_ID"           # remove worktrees
agent-swarm cleanup "$RUN_ID" --purge   # also delete .swarm/runs/<id>
```

## What not to do

- Do not invent API keys or call provider HTTP APIs on behalf of swarm.
- Do not `git checkout` feature branches in the primary worktree for swarm workers.
- Do not treat timeout alone as success.
- Do not ship (`push`/`gh pr create`) without an explicit human confirm when gates are active.
- Do not bare force-push; swarm only uses `--force-with-lease`.
