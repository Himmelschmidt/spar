# spar core skill

**spar** is a multi-agent coding product (fleet TUI + headless CLI). Outer agents drive it via CLI; humans use the TUI.

## Discovery

```bash
spar skills list
spar skills get core
spar doctor [--json]
spar provider list [--json]
spar model list|pick|refresh|cache [--json]
spar model list --provider openrouter [--all] [--json]   # OpenRouter catalog, tool-capable by default
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

# pin a model per slot with @model
spar implement -t "..." --providers 'cli:codex@openai/gpt-4o-mini,api:openai@gpt-5' --dry-run
```

Native CLI adapters: `cli:claude`, `cli:grok`, `cli:agy`, `cli:codex`. Run `spar provider list`
to see which resolve on this box.

**`@model` suffix.** Any ref may carry an optional model, split off on the **first `@`**:
`cli:codex@openai/gpt-4o-mini`, `api:openai@gpt-5`. The split happens before the
provider-name check, so the model may contain `:` and `/` (OpenRouter slugs like
`tencent/hy3:free`) while the adapter name may not. `@model` variants share one quota
bucket with their bare provider (`cli:claude@opus` and `cli:claude@haiku` both bucket as
`cli:claude`) — rate limits are per account, not per model. An explicit `@model` beats a
model chosen by `--select`.

`cli:codex` (codex, `codex exec --json`) drives whatever backend + model a codex **profile**
defines (a profile is codex's own (backend, model) bundle), and parses codex JSONL for real
token/cost tracking. Not a takeover target. Selection, highest precedence first:
- A per-slot model — from a `cli:codex@<model>` ref or `--select` — becomes codex's model.
  A model containing `/` is treated as an OpenRouter slug and routed with
  `-c model_provider=openrouter -m <slug>` (so `cli:codex@openai/gpt-4o-mini` and
  `cli:codex@tencent/hy3:free` just work, and different slots can run different OpenRouter
  models in one run); a bare model (`gpt-5`) uses codex's own default provider. An explicit
  model **supersedes** the profile (`-p` is omitted). Discover tool-capable slugs with
  `spar model list --provider openrouter` — it fetches the OpenRouter catalog (public, no
  key) and shows id, context length, and per-million pricing. It **filters to tool-capable
  models by default** (`supported_parameters` contains `tools`): a model without tool
  support silently fails as an agent — it generates text and never calls a tool, exiting 0
  with no artifact. Pass `--all` to include those; `--json` emits every entry with a
  `tool_capable` boolean.
- `SPAR_CODEX_MODEL` → same routing, when no per-slot model is set (e.g. `x-ai/grok-4`).
- `SPAR_CODEX_PROFILE` picks the backend bundle (`-p`): **unset → the `muse` profile**
  (OpenRouter + Muse Spark, the default); set-but-empty → omit `-p` (codex's own config default,
  e.g. plain OpenAI); any other value → that `$CODEX_HOME/<name>.config.toml`.
- OpenRouter profiles need `OPENROUTER_API_KEY` exported in spar's env (codex reads it via `env_key`).

```bash
spar run --workflow review -t "..." --providers cli:codex               # Muse Spark via OpenRouter
SPAR_CODEX_MODEL=x-ai/grok-4 spar run ... --providers cli:codex          # different OpenRouter model
SPAR_CODEX_PROFILE=gpt        spar run ... --providers cli:codex          # a different codex profile
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `OPENAI_BASE_URL` / `XAI_BASE_URL` / `*_MODEL`.

## Workflows

**`--providers` or `--select`** is required for `plan`, `implement`, and `run` (no silent default fleet).

```bash
# Plan (ends HumanGate / awaiting_plan_approval unless autonomy auto-approves)
spar plan -t "describe the work" --providers cli:claude,cli:grok [--big] [--dry-run] [--json] [--detach]

# Or resolve fleet from vals.ai benchmarks + prefs (see [model_select] in spar.toml)
spar model refresh
spar model refresh --if-stale   # refresh only stale/missing benches (cron-friendly)
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

**`spar cleanup`** reaps before it removes: for each of the run's own worktrees it kills
every process whose **cwd is inside that worktree** (SIGTERM → grace → SIGKILL — this is
how orphaned dev servers get collected), then removes the worktree, falling back to a
directory delete if git no longer tracks it. It never touches the project root or anything
outside the run's worktrees. `--json` reports `worktrees[]` with `killed` pids and `removed`.

## Swarm bus

The bus is **workspace-scoped and keyed by a globally-unique `agent_id`**. Run-slot role
ids repeat across concurrent runs, so a run slot's bus id is run-qualified to `run:slot`;
`$SPAR_AGENT_ID` already holds this unique id. `--run <id>` is an optional grouping tag for
sends/views, and also lets `inbox`/`deliver` resolve a short role id to its unique id — so
`spar bus inbox $SPAR_AGENT_ID --claim` (unique id, no `--run` needed) and
`spar bus inbox <role> --claim --run $SPAR_RUN_ID` are equivalent. There is **no run-tag
filter** on the drain: each unique id has its own inbox, so a slot never sees another
run's messages, and a bare agent and a run slot can directed-message each other by id.

```bash
spar bus send -m "hello" [--from human] [--to broadcast|agent] [--run <id>]
spar bus log [--run <id>] [--json]
spar bus presence [--run <id>]
spar bus inbox <agent> [--claim] [--run <id>] [--json]
spar bus reserve path/to/file --holder <agent> [--run <id>]
spar bus release path/to/file --holder <agent> [--run <id>]
spar bus deliver <agent> [--run <id>]              # drain inbox + inject at turn boundary (Stop-hook driven)
spar bus ack <msg_id> --from <agent> [--run <id>]  # stop a requires_ack redelivery
```

A message to `@human` (or any `Blocked` agent) surfaces in the TUI's Activity tab (with a
badge on the tab and the status line) and,
if `[notify]` is configured, also fires an external notifier. A `requires_ack` message
redelivers until acked, then escalates to `@human`.

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
```

**Subscribe, don't poll.** When you are waiting on a run, block on `wait` instead
of spinning on `status` — you don't have to remember to check back:

```bash
spar wait <run_id> --follow --json     # blocks; returns at terminal OR human gate
# exit 0 done · 2 gate (needs you) · 3 stuck/wait-timeout · 4 quota
```

`wait` releases you the instant the run reaches a waitable stop — a **human gate**
(exit `2`, needs a decision) as well as done/failed — so it wakes you exactly when
there is something to act on, not just at the very end. `--json --follow` blocks
quietly and prints the final `RunState` at the stop; text `--follow` live-tails the
event log. `--timeout` (default `2h`) caps the block and returns exit `3` if it
lapses. Poll `status --json` / `status --all` only when you genuinely can't block —
e.g. supervising several runs at once, where you background one `wait --follow` per
run and reconcile as each returns.

### TUI shape (humans)

A **rail** + **one main area**. Main always shows the rail's selection.

- Rail: `projects ▸ runs ▸ agents` drill-down. `Enter` pushes a level, `Esc` pops one
  (never quits). `Enter` on an agent **takes it over** in the Shell tab. `/` filters the
  rail (Esc clears). The rail is **attention-sorted**: runs at a gate or broken fly a
  `⚑` and float to the top (and roll up to their project row).
- Main tabs: `Log · Activity · Diff · Shell`, switched with `[` / `]` (Activity carries
  the `@human` alert badge). Diff is the selected slot's real worktree diff.
- Focus: `1` rail · `2` main (Tab cycles the two). `+` / `_` zoom Main.
- `:` opens the **command palette** — `approve`/`reject`/`ship`/`confirm`/`reconcile`/
  `takeover`/`implement`/`plan`/`spawn`/`chat`, Tab-completes run ids.
- **`a` jumps to the next run that needs you** (or tap the `⚑N need you` status token);
  the status line rolls up how many runs want you across the fleet. `r`/`s` reject/ship
  at a gate; approve = tap the button or `:approve`.
- `p` = Projects · `w` log wrap · `g`/`G` top/bottom · `?` help · **`q` quits**.
- Shell tab = a real tmux client: **every key goes to the agent** (incl. `Ctrl+C`);
  `F12` (or `C-a d`) hands focus back to spar. Focusing it full-screen is **Driving
  mode** — green banner + border, rail collapsed.
- Width bands: `<80` cols Main only (rail folds away, tappable tab strip — phone/SSH);
  `80–119` rail + Main; `>=120` the extra width goes to Main.

- Run state: `.spar/runs/<id>/state.json`
- Events (orchestrator): `.spar/runs/<id>/events.jsonl`
- Logs: `.spar/runs/<id>/logs/<slot>.log`
- `status --json` enriches each slot with `slot` (the slot id, mirroring `id`), `last_log_at`, `silent_for_secs`, `last_heartbeat_at`, `stalled`. `stalled` fires for a running slot that has been log-quiet past `timeouts.stall_warn_secs` **and** either has stopped heartbeating (process likely dead/gone) **or** has stayed silent for its entire role timeout (alive but hung too long). A slot that emits nothing loggable but is still heartbeating inside its role budget (e.g. a streaming-json agent mid tool-call) is working, not stalled. `stalled` is advisory (colouring/label only) — a hard hang still surfaces as `Phase::Stuck` / exit code 3 via the role timeout.
- Slot status is reconciled against on-disk markers at read time: a slot recorded as `running` that has a `<slot>.done` / `<slot>.failed` marker is reported `done` / `failed`. `status` never rewrites `state.json`.
- `status --json` also carries **`"abandoned": true|false`** per run: the run is in a non-terminal phase but no live orchestrator owns it (the driving process died). Not an exit code — exit codes are unchanged. Resume with `spar implement --run <id> --providers …`, park it with `spar stop <id>`, or discard with `spar cleanup <id>`.

## Exit codes (stable)

| Code | Meaning |
|------|---------|
| 0 | Success / terminal ok (e.g. plan approved, done) |
| 1 | Failure / halted by operator (`spar stop`, phase=stopped) |
| 2 | Human gate (approve plan / winner / ship) |
| 3 | Stuck / escalated / wait timeout |
| 4 | No usable providers (quota/pause) |

**`status` is observe-only:** process exit is always `0` if the run loads. Read JSON `exit_code` / `phase` for run state. Use `wait` (see **Subscribe, don't poll** above) when you want to block until the run needs you and get the process exit coded by gate/stuck/quota.

**`--dry-run`:** stubs agent processes only; writes `.spar/runs/<id>/`. Does **not** create real git worktrees (cwd under `.spar/…/cwd-*`). Live runs create sibling worktrees.

**Providers (three-tier precedence):** each slot's provider is resolved **explicit `--providers` (positional one-off) > `[roles]` > `[providers].order`**. `--providers` still works exactly as before — a single name fills every slot, multiple names map positionally (impl at 0, then reviewers). If you set a `[roles]` block (see config knobs), it satisfies the requirement on its own: `spar plan`/`implement` run with **no** `--providers`, drawing planner/critic/implementer/tester/test_author and the reviewer list from `[roles]`. `--select <profile>` is the fourth option. Explicit `--providers` always overrides `[roles]` positionally.

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
# Provider assignment by role (@model-capable refs). `reviewer` is a list. This is
# NOT [model_select.role_profiles] below — that maps a role to a benchmark *profile*,
# this maps a role to a *provider*. tester/test_author replace the old [suite]/[spec]
# `provider =` fields (removed).
[roles]
# planner = "cli:claude"
# plan_critic = "cli:grok"
# implementer = "cli:codex@anthropic/claude-opus-4.5"
# reviewer = ["cli:grok", "cli:agy", "cli:claude"]
# tester = "cli:agy"
# test_author = "cli:grok"
# Full suite channel (cheap/dumb model). Implementers/reviewers: smoke/diff only.
[suite]
enabled = true
timeout_secs = 7200
# Reviewer verdict / acceptance gate (review timeouts stay under [timeouts]).
[review]
require_all_criteria = true   # false ⇒ an `unverified` AC no longer blocks the ship
# Pre-coding acceptance tests (plan). Separate test-author agent; not planner/critic.
[spec]
enabled = true
timeout_secs = 1800
# External @human notifier (user-level config only; ignored from a repo spar.toml).
[notify]
# command = "..."   # shell out; message on argv/stdin
# webhook = "..."   # POST message json
# Dynamic model select (vals). Opt-in with --select; cache under ~/.spar/cache/vals/
[model_select]
# benches = ["swebench"]
# cache_ttl_secs = 86400
# auto_refresh = true   # false = never fetch during --select
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
- **Spec channel (plan):** after planner+critic, a `test-author` freezes acceptance tests (`artifacts/test-contract.md` + worktree tests) from plan/critique (bus is audit trail), **before** the plan approval gate. Implement overlays those tests into the impl worktree (fail closed if author ran). Its provider comes from `[roles].test_author` (falls through to the fleet if unset/unusable). Disable with `[spec] enabled = false`.
- **Criterion ids:** scenarios in `artifacts/test-contract.md` carry stable `AC-<n>` ids (numbered from 1, contiguous, never renumbered) plus a `verify:` hint naming a command, `file:line` + assertion, or observable behavior.
- **Reviewer context:** reviewers get the full `plan.md` and `test-contract.md` in their prompt, so they can check the change against the agreed plan and each `AC-n` criterion rather than guessing intent.
- **Review artifact schema (enforced):** each `artifacts/review-<slot>.md` is `## Verdict` / `## Acceptance` / `## Findings` / `## Tests`. The verdict is read as an **anchored header** — the first non-blank line under the first `## Verdict` must be `approve` or `request_changes`; missing or unparseable is treated as `request_changes`. `## Acceptance` carries one `AC-n: pass|fail|unverified — evidence` line per criterion in `test-contract.md`.
- **Acceptance gate:** a run cannot reach `awaiting_ship_confirm` while any contract `AC-n` is `fail`, is `unverified` (default; relax with `[review] require_all_criteria = false`), or is simply **absent** from a review — an unmentioned criterion always blocks. With no contract at all (`[spec] enabled = false`) the verdict alone gates.
- **Suite channel (implement/loop):** a dedicated `tester` slot runs full test suites; impl/review stay smoke/diff-only when it runs. Its provider comes from `[roles].tester` (falls through to model-select/fleet if unset/unusable). Artifact: `artifacts/suite.md`. Independent `review` workflow does not spawn a tester by default.
- **Human TUI `/spawn`:** `/spawn <cli:provider> <prompt>` launches an agent into a pane on spar's own `tmux -L spar` socket, joined to the selected run's bus — watch and steer it in Main's **Shell** tab without leaving spar.
