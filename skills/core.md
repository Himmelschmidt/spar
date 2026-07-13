# spar core skill

**spar** is a multi-agent coding product (fleet TUI + headless CLI). Outer agents drive it via CLI; humans use the TUI.

## Discovery

```bash
spar skills list
spar skills get core
spar doctor [--json]
spar provider list [--json]
spar model list|pick|refresh|cache [--json]
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
spar implement -t "..." --providers cli:claude,api:openai --dry-run
spar run --workflow arena -t "..." --providers api:xai,cli:claude,cli:grok
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `OPENAI_BASE_URL` / `XAI_BASE_URL` / `*_MODEL`.

## Workflows

**`--providers` or `--select`** is required for `plan`, `implement`, and `run` (no silent default fleet).

```bash
# Plan (ends HumanGate / awaiting_plan_approval unless autonomy auto-approves)
spar plan -t "describe the work" --providers cli:claude,cli:grok [--big] [--dry-run] [--json] [--detach]

# Or resolve fleet from vals.ai benchmarks + prefs (see [model_select] in spar.toml)
spar model refresh
spar model list --profile value
spar model pick --role implementer --urgency high --json
spar plan -t "…" --select value --urgency low --dry-run
spar plan -t "…" --select auto --urgency high --dry-run

spar approve <run_id> [--json]
spar reject <run_id> [--reason "..."] [--json]

# Implement continues THE SAME run id (plan → implement → ship)
spar implement --run <run_id> --providers cli:claude,cli:grok,cli:agy [--dry-run] [--json] [--detach]
spar implement -t "small task" --providers cli:claude [--dry-run]
spar implement -t "small task" --select value --urgency high --dry-run

# Named workflows
spar run --workflow loop|arena|roles|peer|review -t "..." --providers cli:claude,cli:grok [--dry-run] [--big]
spar run --workflow arena -t "..." --select best --urgency normal --dry-run

# Independent concurrent multi-provider review (not split-stack peer):
spar run --workflow review -t "Review PR #12 for auth bugs" --providers cli:claude,cli:grok

spar confirm <run_id> [--winner <slot>]   # arena winner
spar reconcile <run_id>                  # arena merge-good-parts + review
spar ship <run_id> --confirm             # draft PR (never merges)
spar stop <run_id> [--json]              # halt dispatch, KEEP branch+worktree (resumable)
spar cleanup <run_id> [--purge]          # remove worktrees (and --purge run data)
```

**`spar stop`** halts a run without discarding work: it writes a `stopped` marker,
signals the orchestrator then the slot process groups (SIGTERM → grace → SIGKILL),
and sets `phase=stopped` (JSON `exit_code: 1`). It never removes the branch or the
worktree — that is `spar cleanup`'s job. A stopped run is **resumable**: rerun
`spar implement --run <id> --providers …` and it clears the marker and continues.
Use `stop` (not killing pids directly) so the orchestrator can't re-dispatch a slot
you just killed.

## Swarm bus

The bus is **workspace-scoped and keyed by `agent_id`**. `--run <id>` is an optional
grouping tag: pass it to scope a send/view to one run; omit it for bare agents. Because
slot ids are not unique across runs, `inbox`/`inbox --claim` are run-scoped too — a run
slot must pass `--run $SPAR_RUN_ID` so it drains only its own run's messages (and never
another concurrent run's); a bare agent omits it and drains only untagged traffic.

```bash
spar bus send -m "hello" [--from human] [--to broadcast|agent] [--run <id>]
spar bus log [--run <id>] [--json]
spar bus presence [--run <id>]
spar bus inbox <agent> [--claim] [--run <id>] [--json]
spar bus reserve path/to/file --holder <agent> [--run <id>]
spar bus release path/to/file --holder <agent> [--run <id>]
```

Layout: `.spar/bus/{events.jsonl,agents.jsonl,inbox/<agent>/,queue/,pending_ack/}`
(workspace, agent-keyed). Per-run `tasks/` + `reserves.json` and a back-compat
event/presence mirror live under `.spar/runs/<id>/bus/`.

## Observe

```bash
spar status [run_id] [--json] [--all]   # --all = every registered project
spar wait <run_id> [--timeout 2h] [--follow] [--json]
spar logs <run_id> [slot] [-f|--follow]

# Global home: open `spar` from anywhere. Runs stay under each project’s
# `.spar/runs/`; project list is ~/.spar/registry.json (or $SPAR_HOME).
# Projects appear when you use spar there — no hardcoded scan paths.
# TUI: default = this project's runs; **p** / Esc = Projects (general view);
# Enter opens a project.
```

- Run state: `.spar/runs/<id>/state.json`
- Events (orchestrator): `.spar/runs/<id>/events.jsonl`
- Logs: `.spar/runs/<id>/logs/<slot>.log`
- `status --json` enriches each slot with `last_log_at`, `silent_for_secs`, `stalled` (log quiet longer than `timeouts.stall_warn_secs` while running)

## Exit codes (stable)

| Code | Meaning |
|------|---------|
| 0 | Success / terminal ok (e.g. plan approved, done) |
| 1 | Failure / halted by operator (`spar stop`, phase=stopped) |
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
[timeouts]
slot_secs = 1800
# review_secs = 1800   # optional; defaults to slot_secs
stall_warn_secs = 300  # running slot silent this long ⇒ stalled in status/TUI (0 = off)
wait = "2h"
# Full suite channel (cheap/dumb model). Implementers/reviewers: smoke/diff only.
[suite]
enabled = true
# provider = "cli:claude"   # else first usable of claude/grok/agy/api:xai/openai
timeout_secs = 7200
# Pre-coding acceptance tests (plan). Separate test-author agent; not planner/critic.
[spec]
enabled = true
# provider = "cli:agy"      # prefer third provider ≠ planner/critic
timeout_secs = 1800
# Dynamic model select (vals). Opt-in with --select; cache under ~/.spar/cache/vals/
[model_select]
# benches = ["swebench"]
# cache_ttl_secs = 86400
# allow = ["cli:*", "api:openai", "api:xai"]
# [model_select.profiles.value]
# quality = 0.6
# cost = 0.8
# speed = 0.3
# min_accuracy = 70
```

## Rules of the road

- One run id plan → implement → ship.
- Coding slots always use git worktrees; never check out feature branches on the primary tree.
- Ship is draft PR only — never merge.
- State lives under `.spar/` in the project root.
- **Spec channel (plan):** after planner+critic, a `test-author` freezes acceptance tests (`artifacts/test-contract.md` + worktree tests) from plan/critique (bus is audit trail), **before** the plan approval gate. Implement overlays those tests into the impl worktree (fail closed if author ran). Disable with `[spec] enabled = false`.
- **Suite channel (implement/loop):** a dedicated `tester` slot runs full test suites; impl/review stay smoke/diff-only when it runs. Artifact: `artifacts/suite.md`. Independent `review` workflow does not spawn a tester by default.
