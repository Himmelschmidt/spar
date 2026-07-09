# Agent operator contract

How an **outer agent** (Claude, Grok, agy, etc.) should call `spar`.

## Principles

1. Prefer `--json` and parse stdout.
2. Prefer `--detach` + `wait` / `status` over long blocking calls when you need to stay responsive.
3. Read artifacts from disk under `.spar/runs/<run-id>/` — do not rely on chat alone.
4. Branch on exit codes, not only stderr text.
5. Never merge to the default branch; shipping is gated.
6. Use `--dry-run` (or `SPAR_DRY_RUN=1`) to exercise workflows without live provider CLIs.

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
spar plan --task "$TASK" --detach --json
# → { "run_id": "...", "phase": "...", ... }

spar wait "$RUN_ID" --json
# exit 2 + phase awaiting_plan_approval

# Read plan for the human:
#   .spar/runs/$RUN_ID/artifacts/plan.md

spar approve "$RUN_ID" --json

# implement creates a *new* run id (parent plan stays plan_approved).
# Always take run_id from implement's JSON — do not wait on the plan id.
spar implement --run "$RUN_ID" --detach --json
# → { "run_id": "<IMPL_ID>", "parent_run": "<PLAN_ID>", ... }
# Plan state also records child_run for discovery:
#   spar status "$RUN_ID" --json  →  .child_run

IMPL_ID=...   # from implement JSON run_id
spar wait "$IMPL_ID" --json
# exit 2 + awaiting_ship_confirm when ready

spar ship "$IMPL_ID" --confirm --json
```

**Note:** `exit_code` in JSON is only set when the phase is terminal or a human gate (`null` while in-flight). Prefer `phase` + polling `wait`.

## Typical Path B (autonomous task)

```bash
spar implement --task "$TASK" --detach --json
# or: spar run --workflow loop --task "$TASK" --detach --json
# Use the run_id from that command's JSON response:
spar wait "$RUN_ID" --timeout 2h --json
```

## Arena

```bash
spar run --workflow arena --task "$TASK" --json
# exit 2 awaiting_winner_confirm
spar confirm "$RUN_ID" [--winner slot-id] --json
spar ship "$RUN_ID" --confirm --json
```

## Roles / peer

```bash
spar run --workflow roles --task "$TASK" --json
spar run --workflow peer --task "$TASK" --json
# mailbox under .spar/runs/$RUN_ID/mailbox/
```

## Status JSON (shape)

`spar status <run-id> --json` returns `RunState`:

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
spar wait "$RUN_ID" --timeout 30m --json
spar logs "$RUN_ID"
spar logs "$RUN_ID" impl-claude
spar attach "$RUN_ID"   # tmux backend only
```

## Doctor

```bash
spar doctor --json
```

Use before a swarm if you are unsure providers are installed. `ok: false` means git missing or no providers on PATH (dry-run still works for workflow testing).

## Providers / quota

```bash
spar provider list --json
spar provider pause claude [--until 1h]
spar provider resume claude
```

First-class v1 names: `claude`, `grok`, `agy`.

Paused providers are skipped by the scheduler. If **every** selected provider is paused, commands return **exit code 4** (Quota) — they do not silently re-enable paused providers.

## Cleanup

```bash
spar cleanup "$RUN_ID"           # remove worktrees
spar cleanup "$RUN_ID" --purge   # also delete .spar/runs/<id>
```

## What not to do

- Do not invent API keys or call provider HTTP APIs on behalf of swarm.
- Do not `git checkout` feature branches in the primary worktree for swarm workers.
- Do not treat timeout alone as success.
- Do not ship (`push`/`gh pr create`) without an explicit human confirm when gates are active.
- Do not bare force-push; swarm only uses `--force-with-lease`.
