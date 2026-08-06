# Agent operator contract

How an **outer agent** (Claude, Grok, agy, etc.) should call `spar`.

Also: `spar skills get core` (preferred; always current).

## Principles

1. Prefer `--json` and parse stdout. Both `run_id` and `id` are present on emit JSON.
2. **Subscribe, don't poll:** to wait on a run, `--detach` to launch it, then block on `spar wait <id> --follow --json` — it returns the instant the run hits a terminal state *or* a human gate (exit `2`), so you're woken exactly when there's something to do instead of having to remember to re-check `status`. Poll `status` only when you can't block (e.g. several runs at once: background one `wait` per run).
3. Read artifacts from disk under `.spar/runs/<run-id>/` — do not rely on chat alone.
4. Branch on exit codes, not only stderr text.
5. Never merge to the default branch; shipping is gated.
6. Use `--dry-run` (or `SPAR_DRY_RUN=1`) to exercise workflows without live provider CLIs.
7. **Know what branch the slots are on.** Slot worktrees are cut from the run's *base*: `--base <ref>` if given, else the HEAD of the directory you invoked spar from. Assert `base_commit` from the launch JSON before you trust a review or a diff.

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

## Reclaiming worktrees

Nothing reclaims slot worktrees on its own: the normal successful path ends at the ship
gate rather than `done`, and `auto_cleanup` is off by default. `spar cleanup --all` sweeps
runs nothing can resume (`done`, `plan_rejected`); add `--older-than 7d` to also take
resumable-but-stale ones (`stopped`, `failed`, `stuck`, `quota`, gates). In-flight runs are
never swept — `spar stop --abandoned` first. `spar reject` reaps its own worktrees.

## Abandoned runs

A run in a non-resting phase with no live orchestrator is abandoned: the driver died and
its slots are still running (they are in their own process groups by design). `spar wait`
exits `3` with an `error` saying so after a 15s grace rather than blocking; `status --json`
carries `orphan_pids`; `spar stop --abandoned` reaps every such run in the project and
parks it at `stopped`. Runs at rest (terminal, gate, stopped) are never swept.
SIGINT/SIGTERM to an orchestrating `spar` now reaps its slots first; SIGKILL cannot be
caught, which is what the sweep is for.

## Run config isolation

`spar.toml` is one file per project and every process reads it, so parallel agents writing
`[roles]` into it clobber each other. Two things fix that:

- **`--role <role>=<provider>`** on `plan` / `implement` / `run` assigns per role without
  touching the file (repeatable; repeated `reviewer` builds the panel). It satisfies the
  `--providers`-or-`--select` requirement on its own. Use it instead of editing `spar.toml`.
- **Each run freezes its config** at creation into `.spar/runs/<id>/config.json`. Every later
  phase of that run reads the snapshot, so another agent's edit cannot change its fleet,
  timeouts, spec/suite channels, or ship gate. `spar implement --run <id> --reload-config`
  re-reads the file and re-freezes; `--role` on a resume without it exits `1`.

## Base ref

`plan` / `implement` / `run` accept **`--base <ref>`** (branch, tag, sha, `origin/main`,
`HEAD~2`), resolved in your current directory when it is inside the project's repo, against
the main checkout otherwise. It fixes the commit every slot worktree in the
run is cut from. Without it the base is the HEAD of the invoking directory — *not*
`project_root`, which is always the repo's main checkout even when you drive spar from a
linked worktree (that is where `.spar/` lives).

The base is resolved once per run id and reported as `base_ref` / `base_commit` in every run
JSON and in `status --json` (both the single-run and the list form). It is a commit, so
uncommitted work in your tree is not in it — spar warns about that on **stderr**, which is the
only channel for it, so capture stderr as well as stdout. `implement --run <id>` inherits the
base; a run cannot be re-based after creation (`--base` with a different commit exits `1`).

```bash
BASE=$(spar run --workflow review -t "$TASK" --providers cli:grok,cli:claude --json | jq -r .base_commit)
[ "$BASE" = "$(git rev-parse HEAD)" ] || echo "slots are not on my branch"
```

`spar ship` targets its draft PR at the run's base branch when a local remote-tracking ref
exists for it (no network call — `git fetch --prune` if stale), else the repo default (noted
on stderr); `spar ship <id> --base <branch>` forces the target.

## Path A (plan → approve → implement) — **one run id**

```bash
spar plan --task "$TASK" --providers cli:claude,cli:grok --detach --json
# → { "run_id": "...", "id": "...", "phase": "...", ... }

spar wait "$RUN_ID" --json
# exit 2 + phase awaiting_plan_approval  (manual autonomy)
# OR exit 0 + plan_approved             (semi/high/full)

# Read plan + acceptance contract:
#   .spar/runs/$RUN_ID/artifacts/plan.md
#   .spar/runs/$RUN_ID/artifacts/test-contract.md

spar approve "$RUN_ID" --json   # only if still awaiting_plan_approval

# SAME run id continues into implement (workflow becomes loop).
spar implement --run "$RUN_ID" --providers cli:claude,cli:grok,cli:agy --detach --json
# → { "run_id": "$RUN_ID", ... }   # not a child run

spar wait "$RUN_ID" --json
# exit 2 + awaiting_ship_confirm when ready

spar ship "$RUN_ID" --confirm --json
```

`--providers` is **required** for plan / implement / run (no implicit fleet).

**Note:** `exit_code` in JSON is only set when the phase is terminal or a human gate (`null` while in-flight). Block on `wait --follow` and branch on its exit code rather than polling `status` for `phase`.

## Path B (autonomous task)

```bash
spar implement --task "$TASK" --providers cli:claude --detach --json
spar wait "$RUN_ID" --timeout 2h --json
```

## Providers (dual backend)

```bash
# bare names = native-cli
--providers cli:claude,cli:grok

# explicit
--providers cli:claude,api:openai,api:xai
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `*_BASE_URL` / `*_MODEL`.

## Arena

```bash
spar run --workflow arena --task "$TASK" --providers cli:claude,cli:grok,cli:agy --json
spar confirm "$RUN_ID" [--winner slot-id] --json
# or: spar reconcile "$RUN_ID" --json
spar ship "$RUN_ID" --confirm --json
```

## Bus / peer

The bus is workspace-scoped and keyed by `agent_id`; `--run` is an optional grouping tag.

```bash
spar bus send --run "$RUN_ID" -m "hello" --to broadcast
spar bus log --run "$RUN_ID"
spar run --workflow peer --task "$TASK" --providers cli:claude,cli:grok --json
```

## Status JSON

`status` / `wait` print full `RunState` (`id` field). Workflow emits also include `run_id` (alias of `id`).
