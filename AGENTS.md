# spar

Multi-agent coding orchestrator. Rust, single binary, TUI-first.

**This file is for agents working ON spar.** To *use* spar, read
`spar skills get core` or `docs/agent-operator.md`. Do not learn the CLI
from this file.

## Workflow

All work in a sibling worktree. Never switch the primary checkout's
branch. There is no CI. Gates are local:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Product/architecture calls go in `DECISIONS.md` (`OPEN` / `LEANING` /
`DECIDED`). Keep `skills/core.md` in sync in the same change when the
agent-facing surface moves (CLI, flags, config, exit codes).

## Architecture

From `docs/architecture-dual-backend.md`. **Workflows do not fork per
backend.**

| Layer | Owns | Does not own |
|---|---|---|
| Orchestrator | Run lifecycle, phases, gates, worktrees, bus, review, ship | Provider wire protocols |
| Backend | How a slot thinks (`native-cli` vs `api-sdk`) | Whether to arena vs implement |
| Adapter | One provider on one backend | Cross-run scheduling |

If you branch on backend inside a workflow, you are in the wrong layer.
`.spar/runs/<id>/` has the same layout either way.

## Invariants

- One run id threads plan → implement → ship. Continuing is a new
  **round** on the same id. `implement` refuses to mint a second id for a
  plan it can trace (O45). Legs are linked by hand (`spar link`), never
  inferred (O46).
- Coding slots always get a worktree.
- Worktrees are cut from `state.base_commit` whenever recorded, never from
  `project_root`'s HEAD (O26).
- A run reads `.spar/runs/<id>/config.json`, not live `spar.toml` (O27).
  Use `Config::for_run`, not `Config::load`.
- `ship` opens a draft PR and never merges. Force-push only to swarm
  branches.
- `--providers` or `--select` is required on `plan` / `implement` / `run`.
- Exit codes are a public contract: `0` ok, `1` fail, `2` human gate,
  `3` stuck, `4` quota. Never repurpose them.
- Completion is process exit plus expected artifacts. Timeout alone is
  not success.
- Swarm bus is workspace-scoped and `agent_id`-keyed at `.spar/bus/`, not
  under a run (W5).

## Tests

Scenario tests live at `tests/scenarios/*.rs` and only run because
`Cargo.toml` has a `[[test]]` block per file. Adding a file does nothing
until you add the block.

`--dry-run` / `SPAR_DRY_RUN=1` is the test backend: real `.spar/` layout,
no provider spawn.

Tests that spawn the `spar` binary must `env_remove` `SPAR_PROJECT_ROOT` /
`SPAR_RUN_ID` / `SPAR_AGENT_ID`. Without that, a child inside a spar
worktree writes real runs into the primary checkout.

Don't extend `mailbox.rs`. `bus.rs` replaced it.

You cannot interrupt a working CLI agent. Deliver only at turn
boundaries, when it's idle.

## Docs

`DECISIONS.md`, `docs/PRODUCT.md`, `docs/architecture-dual-backend.md`,
`docs/architecture-a2a.md`, `docs/agent-operator.md`, `skills/core.md`.
