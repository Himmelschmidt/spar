# spar fleet skill — cheap coders, smart judgment, over OpenRouter

How to run spar so **cheap OpenRouter models do the coding grunt work** while **smart
models plan, review, and verify** — assigned by role, once, in config. For the full CLI
surface and exit-code contract, read `spar skills get core`; this skill is the workflow on
top of it.

## The idea

Every slot in a run has a **role** (`planner`, `plan_critic`, `test_author`,
`implementer`, `reviewer`, `tester`). You bind each role to a provider — and optionally a
specific model — in a `[roles]` block. Then every run draws its fleet from that block; no
per-run `--providers` needed. Cheap where it's grunt work, expensive where it's judgment.

This is safe because the cheap implementer is **bounded, not trusted**: a `test_author`
freezes acceptance tests before any code is written, reviewers see the plan and contract,
and the run **cannot ship** while any acceptance criterion is failed, unverified, or
unmentioned (the acceptance gate). Smart reviewers + frozen tests catch grunt-model slips.

## 1. Prerequisites (one-time, machine-level)

- `spar doctor --json` reports `ok: true`.
- The provider CLIs you name are installed and authed. `spar provider list` shows which resolve.
- **For the muse-spark family** (the cheapest coder path): `muse` installed and logged in
  (`muse login`). No API key to export; the CLI carries its own Meta account session.
- **For OpenRouter models** (the other cheap-coder path): an OpenRouter-capable CLI installed
  (`opencode` recommended, `codex` supported) and **`OPENROUTER_API_KEY` exported** in the
  environment spar launches from. spar does not proxy the key — the CLI reads it.

## 2. The `[roles]` config (project `spar.toml`)

```toml
[roles]
planner      = "cli:claude"                       # smart: architecture + plan
plan_critic  = "cli:grok"                          # smart: tighten the plan
test_author  = "cli:claude"                        # smart: freeze acceptance tests
implementer  = "cli:muse"                          # cheap grunt: writes the code
reviewer     = ["cli:grok", "cli:claude"]          # smart panel: adversarial review
tester       = "cli:muse"                          # cheap: runs the full suite

[spec]
enabled = true                 # freeze acceptance tests before coding (recommended)

[review]
require_all_criteria = true    # a run can't ship with an unverified acceptance criterion
```

Precedence: an explicit `--providers` (positional, one-off) **>** `[roles]` **>**
`[providers].order`. A populated `[roles]` satisfies the "`--providers` or `--select`
required" rule, so `spar implement --run <id>` needs no providers flag.

## 3. Provider + model syntax

- `cli:<name>` — a subscription CLI (`claude`, `grok`, `agy`). Flat-rate, unmetered.
- `cli:muse` — Muse Code on its own Meta account. **The cheapest coder slot**: the
  contributor model bills ~0.10 in / 0.20 out per M tokens, roughly a twelfth of what the
  same family costs through OpenRouter. Its discount is paid for with "your content may be
  used for product improvement", so on a repo where that matters pin the full-price model
  (`cli:muse@muse-spark-1.2`) instead. Model ids are plain (`muse-spark-1.2`), no vendor
  prefix; with no `@model` at all, muse's own `settings.json` decides.
- `cli:opencode` / `cli:codex` — CLIs that reach **OpenRouter**. `cli:api` refs also exist
  but use spar's thin in-tree loop — fine for judgment, weak for coding.
- `…@<model>` — pin a model to that slot; different slots can run different models in one run.
  - **`cli:opencode` is the recommended OpenRouter coder** — ~half codex's per-turn token
    overhead (measured ~14.6k vs ~29.5k input on the same trivial task + model) with working
    token tracking. A bare slug auto-routes through OpenRouter:
    `cli:opencode@meta/muse-spark-1.1` → `-m openrouter/meta/muse-spark-1.1`.
  - `cli:codex@<slug>` is the documented alternative (same routing, heavier).

**Discover models** — only tool-capable ones can act as agents:
```bash
spar model list --provider openrouter          # tool-capable only (the default)
spar model list --provider openrouter --all     # include non-agentic models
```

## 4. The loop (outer agent)

With `[roles]` set, no `--providers` needed:

```bash
spar plan -t "Add rate limiting to the API" --json     # exit 2 = plan gate; capture run_id
spar approve <run_id> --json                            # or: spar reject <run_id>
spar implement --run <run_id> --json                    # same run id; exit 2 = ship gate
spar wait <run_id> --follow --json                      # blocks; releases at terminal OR gate
spar ship <run_id> --confirm                            # draft PR, never merges
```

**Exit codes are the contract:** `0` ok · `1` fail · `2` human gate · `3` stuck · `4`
quota. There is **no auto-resolver** — a gate is a deliberate decision point. Resolve with
`approve`/`reject` (plan), `confirm` (arena winner), `ship --confirm` (ship).

## 5. Cost caveat (read this before picking models)

`cli:*` is normally flat-rate. **But `cli:muse`, `cli:opencode@<slug>` and
`cli:codex@<slug>` bill per token** — a coding agent re-sends its system prompt + tool
schemas + history every turn (opencode ~14.6k input/turn, codex ~29.5k on a trivial task;
muse ~46k across two model calls plus ~29k more in the subagents it fans out per turn).
Cheap ≠ free for the metered slots. muse is the one whose *price* makes that overhead
irrelevant: that whole trivial task cost under a cent on the contributor model. It is also
the only adapter that counts its subagent spend, so its numbers are the honest ones. Put genuinely cheap slugs on the grunt roles; the spend
concentrates on the smart judgment models. codex reports real per-run token cost, so
whether this actually beats your subscriptions is measurable, not a guess.

## 6. Per-run override

To deviate once without touching config, pass explicit providers — it wins positionally
(slot 0 = implementer, 1+ = reviewers):
```bash
spar implement --run <id> --providers cli:claude,cli:grok,cli:agy --json
```

Canonical, always-current mechanics: `spar skills get core`.
