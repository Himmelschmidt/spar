# Agent operator contract

How an **outer agent** (Claude, Grok, agy, etc.) should call `spar`.

Also: `spar skills get core` (preferred; always current).

## Principles

1. Prefer `--json` and parse stdout. Both `run_id` and `id` are present on emit JSON.
2. Prefer `--detach` + `wait` / `status` over long blocking calls when you need to stay responsive.
3. Read artifacts from disk under `.spar/runs/<run-id>/` — do not rely on chat alone.
4. Branch on exit codes, not only stderr text.
5. Never merge to the default branch; shipping is gated.
6. Use `--dry-run` (or `SPAR_DRY_RUN=1`) to exercise workflows without live provider CLIs.

## Exit codes

| Code | Constant | When |
|------|----------|------|
| 0 | Success | Command succeeded; plan auto-approved / done |
| 1 | Failure | Hard error, rejected plan, no usable providers, failed run |
| 2 | HumanGate | Plan approval, winner confirm, or ship confirm required |
| 3 | Stuck | Policy chain exhausted; needs human |
| 4 | Quota | No usable provider / quota pause (`phase: quota`) |

**`status` is observe-only:** process exit is always `0` when the run loads. Read `phase` / JSON `exit_code` for run state. Use `wait` when you want process exit coded by gate/stuck.

**`--dry-run`:** no real git worktrees; only `.spar/` state + stubbed agents. Live runs provision sibling worktrees.

## Path A (plan → approve → implement) — **one run id**

```bash
spar plan --task "$TASK" --detach --json
# → { "run_id": "...", "id": "...", "phase": "...", ... }

spar wait "$RUN_ID" --json
# exit 2 + phase awaiting_plan_approval  (manual autonomy)
# OR exit 0 + plan_approved             (semi/high/full)

# Read plan: .spar/runs/$RUN_ID/artifacts/plan.md

spar approve "$RUN_ID" --json   # only if still awaiting_plan_approval

# SAME run id continues into implement (workflow becomes loop).
spar implement --run "$RUN_ID" --detach --json
# → { "run_id": "$RUN_ID", ... }   # not a child run

spar wait "$RUN_ID" --json
# exit 2 + awaiting_ship_confirm when ready

spar ship "$RUN_ID" --confirm --json
```

**Note:** `exit_code` in JSON is only set when the phase is terminal or a human gate (`null` while in-flight). Prefer `phase` + polling `wait`.

## Path B (autonomous task)

```bash
spar implement --task "$TASK" --detach --json
spar wait "$RUN_ID" --timeout 2h --json
```

## Providers (dual backend)

```bash
# bare names = native-cli
--providers claude,grok

# explicit
--providers cli:claude,api:openai,api:xai
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `*_BASE_URL` / `*_MODEL`.

## Arena

```bash
spar run --workflow arena --task "$TASK" --json
spar confirm "$RUN_ID" [--winner slot-id] --json
# or: spar reconcile "$RUN_ID" --json
spar ship "$RUN_ID" --confirm --json
```

## Bus / peer

```bash
spar bus send "$RUN_ID" -m "hello" --to broadcast
spar bus log "$RUN_ID"
spar run --workflow peer --task "$TASK" --json
```

## Status JSON

`status` / `wait` print full `RunState` (`id` field). Workflow emits also include `run_id` (alias of `id`).
