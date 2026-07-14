# spar

Multi-agent coding orchestrator. Rust, single binary, TUI-first. Runs fleets of
coding agents (plan / implement / review / arena / reconcile / ship) across multiple
providers, isolating each coding slot in its own git worktree.

**This file is for agents working ON spar.** If you are an outer agent trying to
*use* spar as a tool, read `spar skills get core` (canonical, always current) or
`docs/agent-operator.md`. Do not learn the CLI surface from this file.

## Workflow

- **Confirm before coding.** Propose the approach (or options with tradeoffs) and wait
  for approval before editing files. Answer questions as questions — don't start coding.
- **Worktrees.** All work happens in a sibling worktree (`../spar-feat-...`,
  `../spar-fix-...`). Never work in the primary checkout and never switch its branch —
  other agents may be running there.
- **There is no CI.** No `.github/workflows`, no justfile, no Makefile. The gates are
  local and you have to run them yourself:
  ```bash
  cargo fmt
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```
- **Record decisions.** Product and architecture decisions go in `DECISIONS.md` as
  `ID | Decision | Status` rows, status `OPEN` | `LEANING` | `DECIDED`. Match the
  existing table format. If you make a call a future agent could reasonably reverse,
  write it down.
- **Keep the skill in sync.** When you add features or change agent-facing behavior
  (CLI surface, flags, config, exit codes), update the embedded operator skill
  (`skills/core.md`, served by `spar skills`) in the same change so it never drifts.

## Architecture — the non-negotiable split

From `docs/architecture-dual-backend.md`. **Workflows do not fork per backend.**

| Layer | Owns | Does *not* own |
|---|---|---|
| **Orchestrator** | Run lifecycle, phases, gates, worktrees, bus, review policy, arena/reconcile, ship, TUI events | Provider wire protocols |
| **Backend** | How a slot thinks and acts — `native-cli` (spawn the vendor CLI) vs `api-sdk` (in-tree agent loop) | Whether to arena vs implement; worktree policy |
| **Adapter** | One provider on one backend (`claude` on native-cli, `anthropic` on api-sdk) | Cross-run scheduling |

If you find yourself branching on backend inside a workflow, you are in the wrong
layer. The orchestrator stays backend-agnostic; `.spar/runs/<id>/` has the same layout
either way.

## Module map

| File | Role |
|---|---|
| `main.rs` | CLI entry, command dispatch |
| `cli.rs` | clap definitions |
| `executor.rs` | Run execution: phases, slots, dispatch |
| `process.rs` | Headless provider spawn — the **default** execution path |
| `tmux.rs` | Optional tmux backend (dedicated session per run). Small, opt-in |
| `tui.rs` | ratatui TUI. Focus: Runs / Agents / Log / Activity / Composer |
| `bus.rs` | Swarm bus (A2A): presence, typed messages, inbox, path reserves |
| `mailbox.rs` | **Legacy.** Superseded by `bus.rs`. Don't build on it |
| `worktree.rs` | Git worktree lifecycle for coding slots |
| `state.rs` | Run state (`state.json`) |
| `events.rs` / `liveness.rs` | Event stream; stuck/stall detection |
| `registry.rs` / `provider_ref.rs` | Provider registry, `cli:` / `api:` refs |
| `quota.rs` | Provider quota pause / resume / cooldown |
| `runlock.rs` | Per-run locking |
| `ship.rs` | Draft PR creation |
| `skills.rs` | Serves `spar skills`; `skills/core.md` is `include_str!`'d into the binary |
| `config.rs` / `paths.rs` / `templates.rs` / `markers.rs` / `tasks.rs` | Support |

## Invariants — do not break these

- **One run id** threads plan → implement → ship.
- **Coding slots always get a worktree.** No exceptions.
- **`ship` opens a draft PR and never merges.** Force-push only ever to swarm branches.
- **`--providers` or `--select` is required** on `plan` / `implement` / `run`.
- **Exit codes are a public contract** for outer agents — `0` ok, `1` fail, `2` human
  gate, `3` stuck, `4` quota. Never repurpose them.
- **Completion means process exit plus expected artifacts/markers.** Never treat a
  timeout alone as success.
- **`.spar/runs/<id>/`** (`state.json`, `events.jsonl`, `logs/`, plus a run-scoped
  `bus/` for tasks/reserves and a back-compat event/presence mirror) has the same
  shape for every backend.
- **The swarm bus is workspace-scoped and `agent_id`-keyed** (W5): it lives at
  `.spar/bus/`, not under a run. `run` is an optional message/presence tag; bare
  Composer agents and run slots share one bus and address each other by id. Run views
  filter by the tag (`spar bus … --run <id>`).

## Build & test

```bash
cargo build
cargo test                  # unit tests across ~29 files + 5 scenario targets
cargo run -- --dry-run …    # or SPAR_DRY_RUN=1
```

**Scenario tests are declared explicitly.** They live at `tests/scenarios/*.rs`, not
`tests/*.rs`, so Cargo only sees them because `Cargo.toml` declares a `[[test]]` block
per file with an explicit `path`. **Adding a new scenario file does nothing until you
add its `[[test]]` block** — it will silently never run.

**`--dry-run` / `SPAR_DRY_RUN=1` is the test backend:** real `.spar/` layout, no
provider spawn, no tokens burned. Use it for anything touching run lifecycle.

## Docs map

| Doc | What |
|---|---|
| `DECISIONS.md` | Product + architecture decisions, with status |
| `docs/PRODUCT.md` | What spar is |
| `docs/architecture-dual-backend.md` | native-cli vs api-sdk; the layer split |
| `docs/architecture-a2a.md` | Swarm bus design |
| `docs/agent-operator.md` | How an outer agent calls spar |
| `skills/core.md` | Canonical outer-agent skill (embedded in the binary) |

## Traps

- **Don't extend `mailbox.rs`.** `bus.rs` replaced it; `send()` only mirrors to it for
  legacy readers.
- **You cannot interrupt a working CLI agent.** Text sent to a busy agent's TTY queues
  unsubmitted in its input box. Deliver only at turn boundaries, when it's idle.
