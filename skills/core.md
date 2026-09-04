# spar core skill

**spar** is a multi-agent coding product (fleet TUI + headless CLI). Outer agents drive it via CLI; humans use the TUI.

## Discovery

```bash
spar skills list
spar skills get core
spar skills get fleet   # cheap OpenRouter coders + smart plan/review, assigned by [roles]
spar doctor [--json]
spar provider list [--json]
spar model list|pick|refresh|cache [--json]
spar model list --provider openrouter [--all] [--json]   # OpenRouter catalog, tool-capable by default
```

## Default surfaces

| Who | How |
|-----|-----|
| Human | `spar` (no subcommand) → product TUI, landing on Home (scoped to the current repo when run inside one) |
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

Native CLI adapters: `cli:claude`, `cli:grok`, `cli:agy`, `cli:codex`, `cli:opencode`, `cli:muse`. Run
`spar provider list` to see which resolve on this box and their live pause/cooldown status.

**agy note.** agy runs headless with `--print` and emits almost nothing to stdout, so spar
recovers its tools/tokens/quota from disk: tool counts + activity from agy's per-conversation
transcript, and token/quota counts by teeing agy's statusline payload. To capture the latter,
spar installs a wrapper into `~/.gemini/antigravity-cli/settings.json` that **chains to your
existing statusline** (it wraps, never replaces it) and tees payloads to `~/.gemini/antigravity-cli/.spar/`.
Run `spar provider agy-statusline-uninstall` to remove the wrapper and restore your original.
agy's `--print-timeout` is also derived from the role's hard ceiling, so a long
agy slot runs its full budget instead of dying at agy's 30-minute default.

**Pause / cooldown.** A provider is paused manually (`spar provider pause <ref>`) or
automatically when a slot hits a rate-limit signal. Pauses **auto-recover** ~30 min after
they were set (or at an explicit cooldown reset), so the provider is re-probed rather than
staying dead; `spar provider resume <ref>` clears one immediately. A paused provider is
never silently swapped into another role: `plan`/`implement` exit `4` (quota) naming the
paused ref so per-role assignment stays exactly what you specified.

**Driving an OpenRouter model? Lead with `cli:opencode@<slug>`.** It is the recommended
OpenRouter coder: same OpenRouter-slug routing as `cli:codex` but ~half the per-turn token
overhead (measured ~14.6k input vs codex ~29.5k on the same trivial task + model), so it is
the default choice; `cli:codex` remains the documented alternative.

**Want the muse-spark family? Use `cli:muse`, not OpenRouter.** The muse CLI reaches
muse-spark on Meta's own account pricing, where the contributor tier is roughly 12x cheaper
in and 21x cheaper out than the same family billed through OpenRouter. `cli:opencode@meta/…`
is the fallback when muse is not installed.

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

`cli:opencode` (opencode, `opencode run --format json`) is the **recommended OpenRouter
coder** — OpenRouter is its default routing and its per-turn token overhead is roughly half
codex's. spar parses opencode's NDJSON per-step tokens for real usage tracking. Not a
takeover target. Model selection, highest precedence first:
- A per-slot model — from a `cli:opencode@<model>` ref or `--select` — becomes opencode's
  `-m`. A bare vendor slug is **prefixed with `openrouter/`** so OpenRouter is the default
  (`cli:opencode@meta/muse-spark-1.1` → `-m openrouter/meta/muse-spark-1.1`); an already
  `openrouter/…` model passes through unchanged; a bare word (`gpt-5`) passes through to
  opencode's own default provider. Different slots can run different OpenRouter models in one
  run. Discover tool-capable slugs with `spar model list --provider openrouter`.
- `SPAR_OPENCODE_MODEL` → same routing, when no per-slot model is set.
- Unset → opencode's own config default model.
- OpenRouter models need `OPENROUTER_API_KEY` exported in spar's env (opencode reads it).

```bash
spar run --workflow review -t "..." --providers cli:opencode@meta/muse-spark-1.1  # -> openrouter/meta/muse-spark-1.1
SPAR_OPENCODE_MODEL=x-ai/grok-4 spar run ... --providers cli:opencode              # different OpenRouter model
```

`cli:muse` (Muse Code, `muse exec --json --yolo --user-input-auto-resolve`) runs the
muse-spark family through muse's own Meta account, so it needs **no** OpenRouter key; log in
once with `muse login`. It is the cheap-implementer route: the contributor model bills about
0.10 in / 0.20 out per M tokens against 1.25 / 4.25 for the same family through OpenRouter.
Not a takeover target. Model selection, highest precedence first:
- A per-slot model, from a `cli:muse@<model>` ref or `--select`, becomes `--model`. Ids are
  plain Meta model ids (`muse-spark-1.2`, `muse-spark-1.2-contributor`) with no vendor prefix,
  since meta is the only provider behind this adapter.
- `SPAR_MUSE_MODEL`, when no per-slot model is set.
- Unset → **no `--model` flag at all**, so muse's own `settings.json` decides. That keeps the
  choice of the contributor tier (whose discount is paid for with "your content may be used
  for product improvement") a single decision on the box rather than something spar bakes
  into every repo. On a repo where that matters, pin `cli:muse@muse-spark-1.2`.
- `SPAR_MUSE_REASONING_EFFORT` maps to `--reasoning-effort` (none|minimal|low|medium|high|
  xhigh|ultra); unset leaves muse's default of high.

Token accounting differs from every other adapter, deliberately. muse reports **no** usage on
stdout, so spar reads its session log
(`${XDG_DATA_HOME:-~/.local/share}/muse/sessions/YYYY/MM/DD/<session-id>/`) after the slot
exits and sums the `goal_usage_attribution` records, **including the subagent sessions muse
fans out per turn**: six of them on a trivial one-file task, which is real spend the other
adapters would not see. Input is **summed across model calls** rather than maxed, so
`stats.json` reports billed tokens, not final context size. Watch one trap if you reconcile
these numbers yourself: muse's `cached_tokens` is a **subset** of `input_tokens`, not a
sibling of it as in claude's disjoint pair, so adding the two double-counts. The tool count
is recovered from the same log, so it survives a round that was killed before the stream
reported one.

```bash
spar implement -t "..." --role implementer=cli:muse --role tester=cli:muse   # muse settings pick the model
spar run --workflow review -t "..." --providers cli:muse@muse-spark-1.2      # pin the non-contributor model
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `OPENAI_BASE_URL` / `XAI_BASE_URL` / `*_MODEL`.

## Slots and roles

A **slot** is one agent process: one provider, one prompt template, one worktree, one log,
one expected artifact. A **role** is what that slot is for. There are nine roles. Six are
assignable by name in `[roles]` / `--role`; three are workflow-internal and take fleet
positions.

| Role | In `[roles]` | Spawned by | Count | What it does | Artifact |
|---|---|---|---|---|---|
| `planner` | yes | `plan` | 1 | Writes the plan from the brief. The only plan-phase slot whose failure fails the run. | `plan.md` |
| `plan_critic` | yes | `plan` | 1 | Reads the draft, edits `plan.md` directly, names gaps and the scenarios the test-author must cover (it writes no tests). Its failure is tolerated: the run keeps the plan. | `plan.md`, `plan-critique-<slot>.md` |
| `test_author` | yes | `plan`, if `[spec] enabled` | 1 | Freezes acceptance criteria (`AC-n`) and real test files **before** the plan gate. | `test-contract.md` |
| `implementer` | yes | `loop`, `arena` | 1 (`loop`), `max_agents` (`arena`) | Writes the code in its own worktree. Smoke/diff tests only while the suite channel runs. | `summary-<slot>.md`, `carry-forward-<slot>.md` |
| `tester` | yes | `loop`, if `[suite] enabled` | 1 | Runs the **full** suite and nothing else. Deliberately a cheap model: it runs tests, it does not judge them. | `suite.md` |
| `reviewer` | yes | `loop` (2), `review` (N), `reconcile` (2) | see left | Adversarial review of the diff against `plan.md` and every `AC-n`. Verdict is `approve` or `request_changes`. | `review-<slot>.md` |
| `ranker` | no | `arena` | 1 | Reads every implementer summary and picks a winner. Runs in the project root, not a worktree. | `ranking.md` |
| `reconciler` | no | `spar reconcile` | 1 | Merges the best parts of the candidate worktrees into one implementation in its own worktree. | `summary-reconcile.md` |
| `peer` | no | `roles`, `peer` | 2 | One half of a split-stack pair (`roles` = frontend/backend, `peer` = symmetric), coordinating over the bus. | `summary-<slot>.md` |

- **Only the six assignable roles read `[roles]` / `--role`.** `ranker`, `reconciler` and
  `peer` take fleet positions instead: the ranker gets the **last** provider in the fleet,
  the reconciler the **first**, the peers the first two. All three bill against
  `[budget] other`, not a role budget.
- **The reviewer panel in `loop` is fixed at 2** and is not a config knob. A `[roles].reviewer`
  list longer than two still only fills two slots there; the extra entries matter in
  `--workflow review`, which sizes itself from `--providers`. A shorter list falls through to
  `[providers].order` for the remaining position rather than shrinking the panel.
- **The fleet is positional and cycles.** `loop` resolves `max(max_agents, 3)` providers as
  `implementer, reviewer, reviewer, …`; `arena` resolves `max(max_agents, 2)`, all
  implementers. A fleet shorter than the slot count repeats from the start, so an arena
  driven from a single-entry `[roles].implementer` runs N slots on the **same** provider.
  Pass explicit `--providers` when you want a diverse field.
- **Templates are per role**, embedded in the binary (`templates/`): `planner`, `plan_critic`,
  `test_author`, `implementer`, `tester`, `reviewer_adversarial`, `ranker`, `reconciler`,
  `peer_half` (+ `role_frontend` / `role_backend` notes appended for `--workflow roles`).

## Workflows

**`--providers` or `--select`** is required for `plan`, `implement`, and `run` (no silent default fleet).

| `--workflow` | Slots | Ends at |
|---|---|---|
| `plan` | `planner` + `plan_critic`, then `test_author` when `[spec] enabled` | plan gate (`awaiting_plan_approval`, exit 2) |
| `loop` | `implementer` ×1, `tester` ×1 when `[suite] enabled`, `reviewer` ×2; up to 3 fix rounds, then rotate/widen/`stuck` | ship gate (`awaiting_ship_confirm`) |
| `arena` | `implementer` ×`max_agents` in waves, then `ranker` ×1 | winner gate (`awaiting_winner_confirm`) |
| `roles` | `peer` ×2, frontend/backend split, dispatched one after the other | `done` (no review, no gate) |
| `peer` | `peer` ×2, symmetric, dispatched in parallel over the bus | `done` (no review, no gate) |
| `review` | `reviewer` ×N (one per `--providers` entry, default 2), in parallel, no implementer and no tester | `done` |

`spar plan -t` is `--workflow plan`. `spar implement -t` is `--workflow loop`: a fresh brief
straight into build/review with **no** planner, critic or test-contract, which is the cheap
middle setting between `--workflow review` and the full `plan → approve → implement` path.
`spar reconcile <id>` is an arena continuation, not a workflow: it adds a `reconciler` plus
two more `reviewer` slots on the reconciled tree.

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

# Implement continues THE SAME run id (plan → implement → ship). Each continuation is
# a new ROUND on that run, never a second run.
spar implement --run <run_id> --providers cli:claude,cli:grok,cli:agy [--dry-run] [--json] [--detach]
spar implement -t "small task" --providers cli:claude [--dry-run]     # a fresh brief
spar implement -t "small task" --select value --urgency high --dry-run

# Replan in place: a second plan round on the same run. `-t` is the directive for the
# round (it reaches the planner and critic), NOT a new task — the brief is identity.
spar plan --run <run_id> -t "narrow it to the email path" [--json]

# Named workflows
spar run --workflow plan|loop|arena|roles|peer|review -t "..." --providers cli:claude,cli:grok [--dry-run] [--big]
spar run --workflow arena -t "..." --select best --urgency normal --dry-run

# Independent concurrent multi-provider review (not split-stack peer):
spar run --workflow review -t "Review PR #12 for auth bugs" --providers cli:claude,cli:grok

# Cut the run from a specific branch/tag/sha instead of the invoking branch
spar plan -t "..." --providers cli:claude --base origin/main
spar run --workflow review -t "..." --providers cli:grok,cli:claude --base feat/checkout

spar confirm <run_id> [--winner <slot>]   # arena winner
spar reconcile <run_id>                  # arena merge-good-parts + review
spar ship <run_id> --confirm [--base <branch>]   # draft PR (never merges)
spar stop <run_id> [--json]              # halt dispatch, KEEP branch+worktree (resumable)
spar stop --abandoned [--json]           # reap every run nobody is driving any more
spar cleanup <run_id> [--purge]          # remove worktrees (and --purge run data)
spar cleanup <run_id> --force            # remove even if it holds unsaved work
spar cleanup --all [--older-than 7d]     # sweep finished runs project-wide
spar reclaim <run_id> | --all [--json]   # delete build output, KEEP the worktree
spar reconcile-state <run_id> | --all    # settle slots a dead orchestrator left `running`
spar reconcile-state <run_id> --apply    # …and write it (bare form only reports)
spar archive <run_id> [--undo] [--json]  # hide a finished run from listings
spar archive --all [--older-than 14d]    # hide every quiet finished run
spar archive --all --halted              # also stopped/failed/stuck/quota (never gates,
                                         # never plan_approved: that is work waiting on you)
spar link <run_id> --to <run_id>         # fold a stray leg into its unit of work
spar link <run_id> --undo                # and back out again
```

### A run is a unit of work, not an invocation

One run id covers an issue (or a bundle of them) from brief to draft PR. Everything
spawned for it — planner, critic, test author, implementer, tester, reviewers, a replan,
N fix rounds — is a **round inside that run**, and listing surfaces show it once.

So `implement` will not silently mint a second id for work that is already a run:

- `--plan <path>` at `.spar/runs/<id>/artifacts/*.md` **continues that run** (it says so
  on stderr). This is the common case: implementing a plan spar wrote. Any other path
  under a run dir — a log, a marker — is not a plan and is not traced.
- `--plan <path>` spar cannot trace to a run is **refused**, naming the runs you probably
  meant (`spar implement --run <id>`). Pass `--new` if it really is separate work.
- `-t "..."` with no run and no plan is a fresh brief and creates a run, as before.
- `--new` always forks, including from a plan spar could have traced.

`spar plan --run <id>` **refuses** `--providers` / `--select` / `--role` / `--base` /
`--big` / `--detach` / `--dry-run`: a replan inherits the run's fleet, base and frozen
config, so a flag that could only apply to a new run is an error rather than a silent
no-op. It also refuses a run that is mid-flight, and it moves the previous round's
`plan.md` and `test-contract.md` to `plan-round<N>.md` / `test-contract-round<N>.md` so a
round that writes nothing cannot present the old plan (or the old frozen contract) at the
approval gate.

`state.json`, `status --json` and the JSON that `plan` / `implement` emit all carry
`round` (which round the run is on), and each slot carries the round it last ran in. A
round is counted when work is actually dispatched, so an invocation that bounces off the
quota gate does not claim one; a fix pass is a round. A run continued after finishing reopens: `archived_at` clears
and the phase moves back into the pipeline.

### Rounds cost money, so they are bounded

A round is a **cold re-dispatch**: the slot's log is truncated, a fresh process starts
with an empty context, and it re-derives the repo before doing any new work. Measured
over 197 real runs, one fix round is a **6.6x** median run (10.8M → 70.9M tokens) and
runs with at least one account for the bulk of all spend.

So a run has a **round ceiling**, `[rounds] max` (default `8`, frozen at creation like
the rest of a run's config). Reaching it parks the run at `awaiting_round_extension` —
a human gate, **exit 2**, phase `awaiting_round_extension` in `status --json` alongside
`round` and `max_rounds`, with `escalation.md` written and the reason in `error`. It is a
gate and not `stuck`: nothing is broken, the run has simply spent the re-dispatch budget
it may spend on its own.

```bash
spar implement --run <id> --max-rounds 12   # buy more rounds; sticky on the run
```

- Re-entering `implement` at the ceiling with no flag gates again **before dispatching
  anything**, so a bounced re-entry costs nothing and claims no round.
- **`stuck` outranks the ceiling.** The rotate-implementer → widen-reviewers → `stuck`
  ladder is resolved first, so a run that genuinely cannot be fixed still exits `3`
  rather than presenting as a question. The full ladder costs about 13 rounds, so at the
  default ceiling you will be asked before it completes — that is the ceiling working,
  and lifting repeatedly does reach `stuck`.
- **A lift buys rounds, not a fresh ladder.** `rotated_implementer` / `widened_reviewers`
  / `fix_rounds` survive a lift, so an outer agent that lifts in a loop terminates.
- Lifting clears `error`, so a run that goes on to the ship gate does not still report
  the round gate's reason.
- `--max-rounds` must be `>= 1`. Turning the ceiling off entirely is `[rounds] max = 0`
  in `spar.toml`: a deliberate project setting, not a flag typed at a gate.
- **The gate does not re-freeze a tampered contract.** Re-entering `implement` re-freezes
  `test-contract.md` from disk (O43), and the ceiling makes that routine — so a re-entry
  whose incoming state has `contract_modified: true` **refuses** (exit `1`) rather than
  adopt an edit spar watched happen under the slot the contract bounds. Read the diff,
  then revert it or pass `--accept-contract`. Every re-freeze is announced on stderr.

### Carry-forward between rounds

At the end of a round the implementer writes `artifacts/carry-forward-<slot>.md`; the
next round's implementer prompt is seeded with it, so a cold dispatch starts with
knowledge instead of re-deriving it. It carries only what a fresh agent cannot cheaply
re-derive — files touched and why, what was tried and rejected, what the slot was stuck
on — never a restatement of the plan or contract, which the next round is handed anyway.

- **Bounded, not accumulating.** The whole section is hard-capped at
  `[rounds] carry_forward_chars` (default `4000`) and truncated with a visible marker.
  Orchestrator-known blockers (which `AC-n` failed and the reviewer's evidence, which
  review requested changes, whether the suite went red) come **first**, so a slot that
  writes an essay cannot squeeze them out, and each blocker is one bullet per criterion
  clamped to its own share of the budget.
- **Rebuilt from disk each time the loop starts**, not carried in memory, so the round an
  operator buys at the ceiling gate — a fresh process — still knows which `AC-n` failed.
- **Consumed on read.** A round whose implementer died without writing one inherits
  nothing rather than a brief describing a worktree two rounds stale.
- **Per slot, not per run.** `artifacts/` is shared and arena runs N implementers at
  once; rounds re-dispatch the *same* slot id (rotation changes the provider and keeps
  the id and worktree), so a slot always reads back its own last round.
- **Context, never a verdict.** No reviewer and no gate reads it. It cannot argue a
  failed `AC-n` past the acceptance gate.
- **Not session resume.** Resuming the vendor CLI session was considered and rejected:
  it carries the whole failed attempt's transcript, so round N+1 starts its context climb
  from a huge base. See DECISIONS O52.

For legs that already exist, `spar link <leg> --to <run>` records the grouping
(`parent_run`). spar never infers it — pairing runs by task text would merge unrelated
issues. The TUI then shows one row per unit of work; `status --json` still lists every
run and carries `parent_run`, so nothing is hidden from an outer agent.

**`spar stop`** halts a run without discarding work: it writes a `stopped` marker,
signals the orchestrator then the slot process groups (SIGTERM → grace → SIGKILL),
and sets `phase=stopped` (JSON `exit_code: 1`). It never removes the branch or the
worktree — that is `spar cleanup`'s job. A stopped run is **resumable**: rerun
`spar implement --run <id> --providers …` and it clears the marker and continues.
A **failed/stuck/quota** run is resumable the same way (its approved plan still counts):
resume resets the failed slots to pending and re-dispatches, so `status` reflects the
new attempt rather than the dead one's `failed` verdict. A provider that dies mid-dispatch
from a rate limit parks the run at `quota` (exit `4`), not `failed` (exit `1`) — the
discriminator is whether the dying slot's own log matched a quota/rate-limit signal;
resuming picks the same run back up, and if the provider is still on cooldown it re-parks
at `quota` immediately rather than dispatching into the same wall. Claude's CLI states its
own weekly-limit reset time in the rejection line; spar parses that into the cooldown
instead of using the generic ~30-minute default, so a weekly-window pause does not get
re-probed for days. The other adapters (codex, opencode, muse, grok) have no
provider-specific reset parser, so their rate limits fall back to the generic cooldown.
Use `stop` (not killing pids directly) so the orchestrator can't re-dispatch a slot
you just killed.

## Base ref — what the slots actually see

Every coding slot gets a fresh worktree cut from **one commit**, the run's *base*. It is
resolved once, when the run is created, and reused for every later phase of that run id:

1. `--base <ref>` if you pass it (branch, tag, sha, `origin/main`, `HEAD~2` — anything git
   resolves). It is evaluated **in your current directory** when that directory belongs to
   the project's repo (so `HEAD`-relative refs mean the worktree you are standing in), and
   against the main checkout otherwise. An unresolvable ref is a hard error, never a silent
   fallback.
2. Otherwise **the HEAD of the directory you invoked spar from**.

That second rule matters because `project_root` (what `status` prints, where `.spar/` lives)
is always the repo's **main checkout** — a linked worktree deliberately resolves to it so one
repo has one bus and one run store. Driving spar from a linked worktree therefore does **not**
mean the slots see that branch by accident; the base is what puts them there.

The base is a **commit**: uncommitted changes in your working tree are not in it (spar's own
`.spar/` doesn't count). spar warns on **stderr** when the invoking tree is dirty (and when it could not resolve a base at all and
is falling back to `project_root`'s HEAD) — those warnings are stderr-only, `--json` included,
so a headless driver has to read stderr to see them. Commit first if the slots need the work.

Assert it before you trust a run — every run reports it:

```bash
spar run --workflow review -t "..." --providers cli:grok,cli:claude --json | jq -r .base_commit
spar status <run_id> --json | jq -r '.base_ref, .base_commit'
```

`base_ref` / `base_commit` are `null` for runs created before spar recorded them and whenever
git could not answer — no commits yet, or a cwd that belongs to a different repo than
`project_root` (e.g. a stale `SPAR_PROJECT_ROOT`). That case is announced on stderr and the
run falls back to `project_root`'s HEAD.

`spar implement --run <id>` inherits the run's base and **cannot be re-based**: a run's base
is fixed when the run is created. Passing `--base` there with a different commit exits `1`
(re-basing mid-flight would drop the plan phase's frozen tests, which are overlaid wholesale
into the implementer, onto a different base). Plan a new run to change the base.

`spar ship` targets its draft PR at the run's base branch when a local remote-tracking ref
(`refs/remotes/origin/<branch>`) exists for it — ship never touches the network to decide
this, so run `git fetch --prune` if your mirror is stale. A tag, a sha, a detached base or an
unpushed branch falls through to the repo default, with a note on stderr. `spar ship <id>
--base <branch>` forces a target. The chosen target is recorded in `artifacts/ship.md`
(`PR base:`), on the dry-run path too.

## Abandoned runs

A run is **abandoned** when it is still in a non-resting phase but no live orchestrator
owns it: the process driving it died. Its slots do **not** die with it — they are spawned
into their own process groups so a slot timeout can reap nested `cargo test`/`pnpm build`
children — so they keep running and keep spending tokens with nobody collecting their work.

- `spar wait` returns **exit 3** with `error: run abandoned …` once a run has read
  abandoned for 15s (`SPAR_ABANDON_GRACE_SECS` overrides), instead of blocking to its
  timeout on a run that can never advance. The grace exists because a just-detached
  orchestrator has not taken the run lock yet.
- `spar status <id> --json` carries **`orphan_pids`** — the slot processes still alive on
  an abandoned run. Empty for every healthy run.
- `spar stop --abandoned` sweeps them: every abandoned run in the project is reaped and
  parked at `stopped` (resumable, worktrees kept). Runs at rest — terminal, a human gate,
  or already stopped — are **never** swept; a plan waiting for approval is meant to have
  no orchestrator.
- On **SIGINT/SIGTERM** an orchestrating `spar` signals its slot groups before it exits,
  so a polite kill no longer orphans anything. `SIGKILL` cannot be caught: that is what
  the three above are for.

### Budgets and nudges: nothing kills a slot on tokens (O50)

**`timeouts.slot_secs` changed meaning.** It used to be the wall clock a slot was killed
at. It is now the **soft** budget: past it spar asks the slot, every
`timeouts.nudge_every_secs` (10 minutes by default), whether it is still making progress and
tells it to land its work if it is not. The kill moved to
`timeouts.hard_ceiling_multiple` (3.0 by default, so 3x the soft budget), which exists only
as a backstop against a genuinely hung process. **The default also moved, 1800 → 5400**: at
1800 a slot was killed below the median implementer dispatch, which is why every real
project had already overridden it.

**Not every role draws `slot_secs`, which is the easy thing to get wrong.** `tester` draws
`[suite] timeout_secs` (7200) and `test_author` draws `[spec] timeout_secs` (**3600**,
also raised from 1800 in this change); `reviewer` draws `[timeouts] review_secs`, which
falls back to `slot_secs`. `hard_ceiling_multiple` multiplies whichever of those the role
actually drew, so raising `slot_secs` alone does not move the tester or the test author.

**Tokens never kill anything.** `[budget]` carries a per-role soft budget on one dispatch's
`billed_tokens`, sized at that role's measured p90. Crossing it tells the slot to write its
artifact now and say what it could not get to, and repeats every `nudge_fraction` of the
budget past it. There is deliberately no token cap: of the 21 dispatches over 100M tokens in
the corpus, 18 exited `0`.

Both nudges ask for the same artifact shape, and the role prompts carry it too, so an
incomplete summary has a defined form: **what was completed, what was not reached, what the
slot is stuck on.**

- Every nudge is an `Info` event tagged with the slot, so `wait --follow --json` and the TUI
  surface it.
- A slot the ceiling killed records `error: "hard ceiling: killed after …"` and
  **`"ceiling_kill": true`** in `status --json`, which is how you tell it from a crash
  (`exit 143`, a signal) without parsing prose. Exit codes are unchanged.
- Delivery is per-adapter and you never choose it. **claude** takes nudges through its
  inbox, which its `Stop` hook drains at the turn boundary. **grok** takes them on its
  native queue. **opencode, muse and codex** have no way to interrupt a working agent, so
  spar writes to `.spar/runs/<id>/logs/nudges-<slot>.md` and their role prompt tells them to
  read it before starting any new major step. Thresholds are checked every 30 seconds, so a
  nudge lands at the next 30s boundary rather than the instant a budget is crossed.
- **Live token visibility differs by adapter**, so token nudges are not uniformly prompt.
  **opencode** reports usage per step and is exact live. **muse** carries no tokens on
  stdout at all, so spar tails its session log (`~/.local/share/muse/sessions/…`), which is
  appended as the turn runs; that is exact live too. **claude** reports per-message usage
  whose input and cache-read arms are `max`ed until its terminal `result` lands, so a live
  reading runs low and its token nudge fires late rather than early. Not a categorical
  guarantee: the same live path *sums* `output_tokens` across the repeated per-content-block
  `assistant` events, which over-counts, and the under-count only dominates because
  cache-read dwarfs output on a real claude slot. It is a lower bound in practice, not by
  construction. **codex** reports
  usage only in `turn.completed`, so it gets **no** live token nudge at all and is
  time-nudged only. **grok**'s numbers are approximate (see below), so do not budget tightly
  against it.

### Slot status after an orchestrator dies

`slots[].status` is written by the orchestrator, so a run whose orchestrator was killed
mid-dispatch keeps a slot at `running` on disk with nothing behind it. Three things
settle that, and you do not have to do anything to get the first two:

- **`status --json` and `wait --json` report the reconciled view.** A slot's terminal
  verdict is written as a marker under `.spar/runs/<id>/markers/` the moment its process
  is reaped, before any state save, so it survives an orchestrator that does not. Read
  commands reconcile against those markers **in memory** and never rewrite the run. spar
  only ever *adds* a marker, so a `<slot>.failed` your agent wrote itself is never
  deleted by the CLI exiting 0, and it still outranks `<slot>.done`.
- **`spar stop`, a run-lock reclaim, and resume persist it.** A slot still `running` in a
  run no live orchestrator owns, with no live process behind it, is recorded as `failed`
  with an `error` naming what happened to its **supervisor**, not to the work:
  `"orchestrator died mid-dispatch"` for a crash, `"halted by operator (spar stop)"` when
  you stopped a run that was still being driven. Neither means the slot's code was bad,
  and the run stays resumable either way.
- **`spar reconcile-state`** backfills runs that were already left that way. It reports
  by default and writes only with `--apply`; `--all` scans the project. It **skips any
  run with a live orchestrator** (listed under `skipped` in `--json`) because that
  orchestrator is the authority on its own slots. It rewrites **only** `slots[].status`
  and `slots[].error` (never a worktree, branch, log or artifact) and is deliberately not
  part of `spar cleanup`, which does remove worktrees. It always exits `0`.

A slot's `exit_code` / `signal` / `pid` / `usage` describe its **last dispatch** and are
cleared when it is re-dispatched, so a `running` slot never carries a previous round's
exit code and a `done` slot never carries a previous round's error. The run's ledger is
`state.usage[]`, one entry per completed dispatch; `slots[].usage` is the latest only.

Prefer `--detach` + `spar wait` over a foreground run precisely so a command timeout in
your harness cannot orphan a fleet.

**Worktrees are not reclaimed on their own.** A successful run ends at the **ship gate**,
not at `done`, and `auto_cleanup` is off by default, so a project accumulates one worktree
per slot per run until you sweep. `spar cleanup --all` reaps every run nothing can resume
(`done`, `plan_rejected`). Resumable runs at rest — `stopped`, `failed`, `stuck`, `quota`,
and human gates — are spared unless you add `--older-than <dur>`, where the age is the
evidence nobody is coming back for them. A run **in flight is never swept**, abandoned or
not: park it first (`spar stop`, or `spar stop --abandoned`). Run data under
`.spar/runs/<id>` survives a sweep unless you pass `--purge`; only worktrees go.

Rejecting a plan (`spar reject`) reaps that run's worktrees immediately — nothing can
resume a rejected plan — while keeping `plan.md` and the critique that justified it.

**`spar cleanup`** reaps before it removes: for each of the run's own worktrees it kills
every process whose **cwd is inside that worktree** (SIGTERM → grace → SIGKILL — this is
how orphaned dev servers get collected), then removes the worktree, falling back to a
directory delete if git no longer tracks it. It never touches the project root or anything
outside the run's worktrees. `--json` reports `worktrees[]` with `killed` pids and `removed`.

**Archiving is the third state, between listed forever and deleted.** `cleanup` reclaims
worktrees and *keeps* the run record — that is where `plan.md`, `test-contract.md`, the
reviews, `suite.md` and `stats.json` live — so a project that drives spar from the CLI
accumulates one row per run forever. `spar archive <id>` hides a run from `status` and the
TUI rail while deleting nothing; `--undo` brings it back, `spar status --archived` lists
them, and the id stays addressable (`spar status <archived-id>` works). Only `--purge`
deletes anything.

Three rules make it safe to leave on:
- **Gates are never auto-archived.** Only `done` / `plan_rejected`. A run parked at
  `awaiting_plan_approval` is waiting on *you*, and hiding those is how the one listing
  that matters gets lost. `stopped` / `failed` / `stuck` / `quota` are ambiguous and stay
  visible until archived by hand.
- **A run stays archived only while it stays finished.** Any phase change to anything
  other than `done` / `plan_rejected` clears the flag — resumed, re-approved, or parked at
  a gate. (Keyed off the archivable set, not the sweep's notion of rest: `spar approve`
  accepts a `plan_rejected` run and moves it to `plan_approved`, which *is* at rest, so the
  narrower rule left an approved run hidden while it waited for `spar implement`.)
- **Reads never archive.** `auto_archive_after` (default `14d`) fires at *launch*
  (`plan` / `implement` / `run`), never from `status`, so observing can never be what hid
  a run from you. Set it `"off"` to disable.

Archiving preserves `updated_at`, so `cleanup --older-than` still sees a run's true age.

A sweep says what it **spared** and why (`spared <run_id>: <reason>`; `spared[]` under
`--json`), so "nothing to sweep" is never confused with a refusal when there are
gigabytes of resumable worktrees on disk.

**Cleanup never removes a worktree that still holds work.** Uncommitted changes, or
commits the run's base does not contain, and the worktree is skipped with a reason —
`remove_worktree` runs `git worktree remove --force` *and* `git branch -D`, so an unmerged
commit is as gone as an unsaved edit. The check lives at the removal itself, so every
evidence path inherits it. `spar cleanup <id> --force` overrides for a run you name;
there is deliberately no `--all --force`.

**Age is never evidence for a run parked at a gate.** `awaiting_plan_approval` and friends
are blocked on *you*: idle time there measures how busy you were, not whether the run was
abandoned, so no `--older-than` will ever sweep one. Resolve the gate, or reap it by run id.

**`spar reclaim` is not cleanup.** It deletes `target/` and `node_modules` *inside* a
finished run's worktrees and keeps the worktree, its branch, every commit and any
uncommitted changes. Because it destroys nothing a build cannot regenerate it needs no
evidence, no age threshold and no confirmation — which is exactly why it is a separate
command and not a `cleanup` flag. It skips any tree with a live process working in it.
`auto_reclaim` (default **true**) does the same for a run's own worktrees when its
orchestrator finishes. Measured: 457 GB of 587 GB under one projects dir was build output,
and the largest single object on the machine was a *stopped* run's target dir.

**`--merged` is evidence too, and stronger than age.** `spar cleanup --all --merged`
also reaps at-rest runs whose every slot branch is already contained in the run's own
`base_ref` — the work is in the base branch, so there is nothing left to lose. It still
cannot touch a run in flight. Containment is **ancestry**, so a **squash-merged** branch
reads as unmerged and needs `--older-than` or an explicit run id.

**This also runs on its own.** `[worktree] auto_cleanup_merged` (default **true**) reaps
merged at-rest runs project-wide just before a launch cuts new slot worktrees — the
moment landed worktrees stop being worth their disk. It reports what it reclaimed on
stderr and never fails a run. This is not `auto_cleanup` (still `false` by default):
that one deletes resumable work on a phase check, this one only deletes what git says is
already in the base branch. Set it `false` to keep every worktree until you sweep by hand.

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
spar wait <run_id> [--timeout 8h] [--follow] [--json]
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
event log. `--timeout` (default `8h`, above the implementer's 4.5h hard ceiling so a healthy
run is never reported stuck) caps the block and returns exit `3` if it
lapses. Poll `status --json` / `status --all` only when you genuinely can't block —
e.g. supervising several runs at once, where you background one `wait --follow` per
run and reconcile as each returns.

### TUI shape (humans)

A **rail** + **one main area**, with no pane borders: chrome bands (breadcrumb, run
stepper, labels + rule) sit above one body split by a single seam. Main always shows the
rail's selection.

- Header: brand + breadcrumb + phase, the `⚑N need you` roll-up, and gate buttons in a
  fixed right-hand zone on 80 columns and up (their x never moves between gates; below
  80 they right-align).
- Stepper: the pipeline **for that workflow** — loop/plan is
  `plan ─ critique ─ spec ─ build ─ tests ─ review ─ ship`, arena is
  `build ─ rank ─ reconcile ─ review ─ ship`, roles/peer is `peers ─ ship`. Read off the
  slots that actually ran: `●` done · `◐` live · `○` pending · `·` skipped (a disabled
  channel or unused role, i.e. never coming) · `✗` failed · `⏸` halted (stopped, quota
  or abandoned) · `⚑` on the step a gate is holding (plan gate on critique, winner gate
  on rank, reconcile gate on reconcile, ship gate on ship). Meters on the right read the
  run's usage ledger, the same numbers `status --json` reports. Folds away under 14 rows.
- The run list shows **units of work**: a run linked as a leg folds into its parent's
  row, which carries the parent's brief, the active leg's id and phase, and the group's
  loudest attention — including how many of its legs want you, so folding can never hide
  a gate. The band names the round and the leg count. Drilling in stays scoped to the leg
  the row acts on: its agents, its worktrees, its tmux panes.
- Rail rows lead with two fixed columns: the selection bar, then the attention flag.
  They are separate facts — on a project where every run wants you, one shared column
  meant the cursor was invisible. The phase is named for the width the column has
  (`ship gate`, `plan gate`, `running`), not truncated from a sentence.
- Rail: `Home ▸ runs ▸ agents` drill-down, rooted at **Home** — the cross-project
  landing view. `Enter` pushes a level, `Esc` pops one (never quits, and never past
  Home). `Enter` on an agent **takes it over** in the Shell tab. `/` filters the rail
  (Esc clears). The rail is **attention-sorted**: runs at a gate or broken fly a `⚑`
  and float to the top (and roll up to their project row).
- Home has four bands, always in this order and always present even when empty:
  **needs you** (runs at a gate, ranked by wait time), **running**, **finished since
  last look** (a watermark of what landed while you were away), **start something
  new**. `p` still opens the flat Projects list; `n` opens the new-run surface with a
  fleet picker over the provider roster (superseding the old "use the CLI" punt for a
  fresh fleet); `P` toggles Home's scope between the current project and everything
  registered.
- Main tabs: `Log · Activity · Diff · Shell` on the labels row, marked by an accent
  underline on the rule beneath them, switched with `[` / `]` (Activity carries the
  `@human` alert badge). Diff is the selected slot's real worktree diff.
- Focus: `1` rail · `2` main (Tab cycles the two). `+` / `_` zoom Main.
- `:` opens the **command palette** — `approve`/`reject`/`ship`/`confirm`/`reconcile`/
  `takeover`/`implement`/`plan`/`spawn`/`chat`, Tab-completes run ids.
- **`a` jumps to the next run that needs you** (or tap the `⚑N need you` status token);
  the status line rolls up how many runs want you across the fleet. `r`/`s` reject/ship
  at a gate; approve = tap the button or `:approve`.
- `p` = Projects · `n` new run · `P` toggle Home scope · `w` log wrap ·
  `g`/`G` top/bottom · `?` help · **`q` quits**.
- Shell tab = a real tmux client: **every key goes to the agent** (incl. `Ctrl+C`);
  `F12` (or `C-a d`) hands focus back to spar. Focusing it full-screen is **Driving
  mode** — green banner, rail and every band but the footer collapsed, pane edge to edge.
- Width bands: `<80` cols Main only (rail folds away, tappable tab strip — phone/SSH);
  `80–119` rail (26 cols) + Main; `>=120` rail 32, the rest of the extra width to Main.
- Colour: spar paints no page background — it composites onto the terminal's own theme
  (and its transparency). Backgrounds appear only on chips, gate/alert washes and
  overlays.

- `status --json` and every run JSON carry **`base_ref` / `base_commit`** — the ref and commit
  all of the run's slot worktrees were cut from (see **Base ref** above).
- **Tokens: `billed_tokens` is spend, `context_tokens` is a gauge.** Every entry in
  `"usage"` (the run's ledger, one entry per dispatch; sum it for the run total) and every
  `logs/<slot>.stats.json` carries both.
  - **`billed_tokens`**: cumulative tokens billed for that dispatch, exactly
    `input_tokens + cache_read_tokens + cache_write_tokens + output_tokens` as reported
    beside it, with reasoning tokens already folded into `output_tokens` (they bill at
    output rates and no provider counts them there). The identity holds for every adapter,
    including the two whose wire format reports a cached prompt as part of `input_tokens`
    (see below) — spar normalizes those before writing the fields, so the four components
    beside `billed_tokens` always add up to it. This is the number to budget on.
  - **`context_tokens`**: the peak prompt footprint of a *single* request
    (`input + cache read + cache write`), i.e. how full the agent's window got. It is a
    maximum, never a running total, so it stays comparable to the model's window and does
    not grow just because a run made more calls. Do **not** sum it and do not read it as
    spend.
  - Conventions per adapter, since the wire formats differ: **claude** is settled by the
    terminal `result` record, which supersedes the per-message ones; **codex** by
    `turn.completed` (its only usage record, so it also stands in for the gauge);
    **opencode** by summing its per-step deltas; **muse** from its session log after the
    slot exits. Those four reconcile against the provider's own session-level ledger, at
    the session level: a codex slot's `billed_tokens` equals its `token_count`
    `total_tokens`, a muse slot's equals the sum of its billed
    `goal_usage_attribution` records, an opencode slot's equals `opencode.db`'s per-step
    `tokens.total` summed. That verification is session-scoped and therefore cannot see
    spend that never appears in the session it checked; the known instance is opencode's
    `task` subagents (`roadmap/BACKLOG.md`).
  - **Two adapters report a cached prompt as a slice of `input_tokens` rather than a
    sibling of it**, the opposite of Anthropic's convention: codex's `cached_input_tokens`
    and muse's `cached_tokens`. spar normalizes both on the way in, storing the uncached
    remainder in `input_tokens`, so the identity above holds as written for every adapter
    and you never have to know which convention the provider used. The consequence to be
    aware of: `input_tokens` for a codex or muse slot is **not** the number that provider's
    own dashboard shows under "input"; add `cache_read_tokens` back to get it.
  - **`context_tokens` is a peak for every adapter except two.** `cli:grok` (below) reports
    a cumulative total. `cli:agy` reports the *latest* call's prompt rather than the largest
    one, because its statusline sink emits one snapshot and keeps no history; it is a real
    window reading, just not a maximum.
  - **`cli:grok` is the exception: treat its numbers as approximate.** spar runs grok on
    its native ACP stream, which reaches no terminal-record branch, so grok's figures come
    only from the per-request path. Against grok's own session store its cache-read and
    input were exact but its output read 2x the truth, and `context_tokens` for a grok slot
    is a cumulative total rather than a peak, so the 80k/150k gauge means nothing there.
    grok slots also never report a `model`, never report a `tool_errors` above zero, and
    never resolve tool *names* (the tool *count* is exact). Known defect, tracked
    separately (`DECISIONS.md` O48); do not budget tightly against a grok slot until it is
    fixed.
- Run state: `.spar/runs/<id>/state.json`
- Events (orchestrator): `.spar/runs/<id>/events.jsonl`
- Logs: `.spar/runs/<id>/logs/<slot>.log`
- `status --json` enriches each slot with `slot` (the slot id, mirroring `id`), `last_log_at`, `silent_for_secs`, `last_heartbeat_at`, `stalled`, `ceiling_kill`. `stalled` fires for a running slot that has been log-quiet past `timeouts.stall_warn_secs` **and** either has stopped heartbeating (process likely dead/gone) **or** has stayed silent for its entire role timeout (alive but hung too long). A slot that emits nothing loggable but is still heartbeating inside its role budget (e.g. a streaming-json agent mid tool-call) is working, not stalled. `stalled` is advisory (colouring/label only) — a hard hang still surfaces as `Phase::Stuck` / exit code 3 via the role's hard ceiling. Note the stall arm reads the **soft** budget, not the ceiling: a slot that has said nothing for its whole budget is hung whatever the ceiling allows. `ceiling_kill` is true only for a slot the hard ceiling ended, never for a crash or a signal.
- Slot status is reconciled against on-disk markers at read time: a slot recorded as `running` that has a `<slot>.done` / `<slot>.failed` marker is reported `done` / `failed`. `status` never rewrites `state.json`.
- `status --json` also carries **`"abandoned": true|false`** per run: the run is in a non-terminal phase but no live orchestrator owns it (the driving process died). Not an exit code — exit codes are unchanged. Resume with `spar implement --run <id> --providers …`, park it with `spar stop <id>`, or discard with `spar cleanup <id>`.

- `status --json` carries **`"roles"`** — the resolved `role=provider` assignment each
  role actually drew (`["planner=cli:grok", "reviewer=cli:grok+cli:claude@opus"]`), which
  is also what `plan` / `review` print at launch. `"providers"` is the run's *pool* and
  still lists refs no role ever drew; read `roles` to know what is running the work.
- **An implementer that exits clean with work in its tree but no artifact gets one
  recovery turn** instead of failing: spar re-prompts the same provider for that artifact
  alone (10 min budget, no new work), logging to `<slot>.recovery.log` so the original
  transcript survives. An implementer whose worktree holds nothing — no commits past the
  base, no dirty tree — still fails with `missing expected artifact <name>`.
  **Implementer only.** `tester` and `reviewer` slots run *in* the implementer's worktree,
  so "has work" is always true for them and a recovered `suite.md` could set the
  authoritative gate green with no suite having run; a recovered `test-contract.md` would
  carry no `AC-n` and make the ship gate vacuous. Those roles fail closed, as before.
- **The suite gate is checked for coverage.** When the tester's commands select specific
  targets (`--test foo`, `--lib`, `mod::case`, a named test file) and the implementer
  committed test files none of them name, spar appends a `## Coverage warning` to
  `suite.md` and broadcasts it on the bus. spar never edits the command list — a harness
  that rewrites its own gate is not a gate. Silent when the suite runs the project default
  (`cargo test`, `cargo test -p pkg`, `pytest tests/`, `go test ./...`), all of which
  compile or collect new test files on their own.

## Exit codes (stable)

| Code | Meaning |
|------|---------|
| 0 | Success / terminal ok (e.g. plan approved, done) |
| 1 | Failure / halted by operator (`spar stop`, phase=stopped) |
| 2 | Human gate (approve plan / winner / ship / raise the round ceiling) |
| 3 | Stuck / escalated / wait timeout |
| 4 | No usable providers (quota/pause) |

**`status` is observe-only:** process exit is always `0` if the run loads. Read JSON `exit_code` / `phase` for run state. Use `wait` (see **Subscribe, don't poll** above) when you want to block until the run needs you and get the process exit coded by gate/stuck/quota.

**`--dry-run`:** stubs agent processes only; writes `.spar/runs/<id>/`. Does **not** create real git worktrees (cwd under `.spar/…/cwd-*`). Live runs create sibling worktrees.

**Providers (four-tier precedence):** each slot's provider is resolved **explicit `--providers` (positional one-off) > `--role` > `[roles]` > `[providers].order`**. `--providers` still works exactly as before — a single name fills every slot, multiple names map positionally (impl at 0, then reviewers). If you set a `[roles]` block (see config knobs), it satisfies the requirement on its own: `spar plan`/`implement` run with **no** `--providers`, drawing planner/critic/implementer/tester/test_author and the reviewer list from `[roles]`. `--select <profile>` is another option. Explicit `--providers` always overrides `[roles]` positionally.

**`--role <role>=<provider>` assigns per role without touching `spar.toml`** — repeatable, and repeating `reviewer` builds the panel (replacing the file's, never appending). Like `[roles]`, it satisfies the "`--providers` or `--select` required" rule on its own. **Prefer it to editing the shared file:** `spar.toml` is one file per project, so parallel agents writing their own `[roles]` into it are writing over each other.

```bash
spar plan -t "…" --role planner=cli:grok --role plan_critic=cli:claude@opus --role reviewer=cli:grok
```

## A run is bound to the config it was created with

`spar.toml` lives at the project root (the repo's **main checkout**, same place `.spar/` does) and every spar process reads it. To keep concurrent runs from re-configuring each other, **each run freezes the merged config at creation** into `.spar/runs/<id>/config.json`, and every later phase of that run — `implement --run`, the detached orchestrator, `ship`, `reconcile`, `status` — reads that snapshot, never the live file.

So an agent editing `spar.toml` for its own run cannot change another run's fleet, timeouts, `[spec]`/`[suite]` channels, or ship-gate strictness (`[review].require_all_criteria`) mid-flight. New runs still pick up the current file.

```bash
spar implement --run <id> --reload-config   # deliberately re-read spar.toml and re-freeze
```

`--reload-config` is the only way to change an existing run's config; it replaces the snapshot, so later resumes keep the reloaded values. `--role` on a `--run <id>` resume without it exits `1` rather than silently re-fleeting the run. Runs created before snapshots existed fall back to the live file.

## Config knobs (`spar.toml`)

```toml
autonomy = "manual" | "semi" | "high" | "full"
message_budget = "none" | "lean" | "normal" | "chatty"
auto_cleanup = false
auto_reclaim = true    # drop a run's target/ + node_modules when it finishes
# Auto-archive finished runs idle this long, at launch. "off" / "0" disables.
auto_archive_after = "14d"
[worktree]
auto_cleanup_merged = true   # reap at-rest runs already contained in their base, at launch
[gates]
plan = true
winner = true
ship = true
[timeouts]
slot_secs = 5400            # SOFT per-slot wall clock: nudges start here, nothing is killed
# review_secs = 5400        # optional; defaults to slot_secs
hard_ceiling_multiple = 3.0 # the only kill, as a multiple of the role's soft budget
nudge_every_secs = 600      # re-ask a slot past its soft clock whether it is progressing
stall_warn_secs = 300  # running slot silent this long ⇒ stalled in status/TUI (0 = off)
wait = "8h"
# Per-role SOFT budget on one dispatch's billed tokens. Tokens never kill a slot.
[budget]
enabled = true
nudge_fraction = 0.2   # renudge every 20% of the role budget past it
planner = 8000000
plan_critic = 6000000
test_author = 20000000
implementer = 60000000
reviewer = 12000000
tester = 6000000
other = 12000000       # ranker / peer / reconciler
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
# Round-loop economy. A round is a cold re-dispatch; measured, one fix round is 6.6x the
# median run cost. `max` is the highest round a run may reach before it parks at the
# awaiting_round_extension gate (exit 2); 0 disables. `carry_forward_chars` caps the brief
# seeded into the next round's implementer.
[rounds]
max = 8
carry_forward_chars = 4000
# Pre-coding acceptance tests (plan). Separate test-author agent; not planner/critic.
[spec]
enabled = true
timeout_secs = 3600    # test_author's SOFT clock; hard_ceiling_multiple applies to it
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
- **Spec channel (plan):** after planner+critic, a `test-author` freezes acceptance tests (`artifacts/test-contract.md` + worktree tests) from plan/critique (bus is audit trail), **before** the plan approval gate. Implement brings those tests into the impl worktree (fail closed if author ran) by **merging the author branch**, then overlaying only the author's **uncommitted** work — tracked-but-modified plus untracked-not-ignored — on top. Committed author work is the merge's job, so the overlay never copies the author branch's revision of a file the implementer is working on, and build output never crosses between worktrees. A merge that fails is aborted **and fails the dispatch** (exit 1), because it is the only path committed acceptance tests take. Anything git ignores stays behind and is named on stderr, in the event log, on the bus, and in the implementer's own prompt, with the author worktree path: if an acceptance test needs an ignored fixture (`.env.test`, an ignored `tests/data/`), the implementer copies it across itself. Its provider comes from `[roles].test_author` (falls through to the fleet if unset/unusable). Disable with `[spec] enabled = false`.
- **Criterion ids:** scenarios in `artifacts/test-contract.md` carry stable `AC-<n>` ids (numbered from 1, contiguous, never renumbered) plus a `verify:` hint naming a command, `file:line` + assertion, or observable behavior.
- **What declares a criterion:** only a checklist/bare item (`- [ ] AC-1: ...`, `AC-1: ...`, `**AC-1:**` / `**AC-1**:`, bullet or ordered markers, with the colon required) or a heading (`### AC-1`, `### AC-1: text`, `### AC-1 - text`) counts. A mention anywhere else — prose in Notes/Non-goals, mid-sentence, or inside a fenced code block — never becomes a criterion, so an aside like "later rounds append AC-17 onward" cannot wedge the ship gate on an id no reviewer can report. If a contract mentions `AC-n` ids but declares none, spar warns loudly (stderr + event log, "declares no criteria") rather than silently disarming the gate.
- **Frozen at round-loop entry:** `implement` reads `test-contract.md` once, when it starts the round loop, and every round for that `implement` invocation judges reviewers against that frozen list. An edit to the file mid-run does **not** take effect — it is not silently ignored either: it is detected each round and reported (`contract_modified: true` in `state.json` / `status --json`, an event, a bus broadcast, stderr, and a note in the reviewer prompt), but the gate still judges the frozen version. This is deliberate: the contract lives outside the repo (`.spar/runs/<id>/artifacts/`, invisible to `git status`/diff), and re-reading it live would let the slot under test edit the gate that bounds it. **To amend a contract, stop the run, edit `test-contract.md`, and re-run `spar implement --run <id>`** — re-entering `implement` re-reads and re-freezes against the file on disk, moving `contract_fingerprint` and clearing `contract_modified`. There is no separate amend command.
- **Reviewer context:** reviewers get the full `plan.md` and `test-contract.md` in their prompt, so they can check the change against the agreed plan and each `AC-n` criterion rather than guessing intent.
- **Review artifact schema (enforced):** each `artifacts/review-<slot>.md` is `## Verdict` / `## Acceptance` / `## Findings` / `## Tests`. The verdict is read as an **anchored header** — the first non-blank line under the first `## Verdict` must be `approve` or `request_changes`; missing or unparseable is treated as `request_changes`. `## Acceptance` carries one `AC-n: pass|fail|unverified — evidence` line per criterion in `test-contract.md`.
- **Acceptance gate:** a run cannot reach `awaiting_ship_confirm` while any contract `AC-n` is `fail`, is `unverified` (default; relax with `[review] require_all_criteria = false`), or is simply **absent** from a review — an unmentioned criterion always blocks. With no contract at all (`[spec] enabled = false`) the verdict alone gates.
- **Suite channel (implement/loop):** a dedicated `tester` slot runs full test suites; impl/review stay smoke/diff-only when it runs. Its provider comes from `[roles].tester` (falls through to model-select/fleet if unset/unusable). Artifact: `artifacts/suite.md`. Independent `review` workflow does not spawn a tester by default.
- **Round ceiling:** `state.round` is bounded by `[rounds] max` (default 8). Hitting it is a **gate** (exit 2, phase `awaiting_round_extension`), lifted with `implement --run <id> --max-rounds <N>` (>= 1; sticky; clears `error`). The rotate/widen/`stuck` ladder resolves **first**, so exit 3 always beats the gate, and a lift preserves the ladder's progress rather than re-buying it.
- **Contract re-freeze is guarded:** a re-entry that would adopt a `test-contract.md` spar saw drift mid-round refuses with exit `1` unless `--accept-contract` is passed. Re-freezes are announced on stderr, not just in `events.jsonl`.
- **Carry-forward:** the implementer writes `artifacts/carry-forward-<slot>.md`, which seeds the next round's implementer prompt (blockers first, capped at `[rounds] carry_forward_chars`, consumed on read). It never reaches a reviewer or the acceptance gate.
- **Human TUI `/spawn`:** `/spawn <cli:provider> <prompt>` launches an agent into a pane on spar's own `tmux -L spar` socket, joined to the selected run's bus — watch and steer it in Main's **Shell** tab without leaving spar.
