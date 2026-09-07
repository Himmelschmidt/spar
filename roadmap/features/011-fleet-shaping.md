---
id: 11
title: Fleet shaping
status: done
milestone: 6
effort: M
priority: high
dependencies: []
---

# 011 — Fleet shaping

## Summary

Makes the resolved fleet honest, visible, and shapeable per run: two bugs where a pinned
reviewer panel or a `--role` assignment silently lost to `[providers].order` or an
explicit `--providers` pool, plus three features (the resolved fleet visible at gates,
`--without` to drop seats per run, `--fleet` presets) that make the fleet something an
operator can see and shape without editing the shared `spar.toml`.

Decisions: `DECISIONS.md` **O56**, **O57**.

## Problem

**Bug A.** `[roles].reviewer` was read as a floor, not an exclusion list: a one-entry pin
still got padded to `DEFAULT_REVIEWERS` (2) out of `[providers].order`, handing a review
seat to a provider nobody named. The resolution pool that fed this was sized off
`max_agents.max(3)`, unrelated to how many seats the panel actually had. Evidence: mdEST
run `1d8d2a25`'s pool was `[claude@sonnet, codex@terra, claude@opus, cli:agy]` where
`cli:agy` was nobody's pin. Widening a stuck run made it worse: `try_widen_reviewers` drew
its extra seat from `[roles].reviewer` then `[providers].order` then the live pool, so a
pinned panel widened onto a provider the operator had deliberately excluded (md-auth
`45df0392`, mdEST `18fb30e1` carrying a third `review-*-wide` seat this way).

**Bug B.** A non-empty positional `--providers` short-circuited the resolver before
`[roles]` was even consulted, so `--role reviewer=X` alongside an explicit pool was
silently dropped — one flag stopped meaning one seat the moment `--providers` was also on
the command line.

**Feature C.** The reviewer panel a run would dispatch was invisible at the one gate where
a human decides whether to pay for it (`awaiting_plan_approval`): `emit_run_json`'s `roles`
key is derived from slots that already exist, and at the plan gate the implement panel has
not been created yet.

**Feature D / E.** There was no way to drop a seat (the plan critic, the pre-coding spec
author, the agent tester) for one run without editing the shared `spar.toml`, which
parallel worktrees cannot safely share anyway, and no ready-made small/standard sizing.

## What shipped

- `roles_resolve::resolve_seat` is the one function every fleet-sizing call site goes
  through (`plan::plan_slot_specs`, `implement::prepare_implement_slots`,
  `model_select::synthesize_from_roles`), replacing the old `provider_for`. Precedence:
  CLI `--role` for that role > CLI `--providers`/`--select` pool > `[roles]` >
  `[providers].order`. A non-empty reviewer list (CLI or file) is exact: past its end the
  answer is `None`, never the next `[providers].order` entry.
- `Config.cli_role_keys` records which role config keys a run's `--role` flags actually
  assigned, frozen into `config.json` at run creation (O27) so a later round with no
  `--reload-config` keeps the same precedence.
- `roles_resolve::panel_size`/`pool_width` replace `max_agents.max(3)`: panel size is
  `[roles].reviewer.len()` (or the CLI list's length) when non-empty, else
  `DEFAULT_REVIEWERS`; pool width is always `1 + panel_size`.
- `try_widen_reviewers` duplicates an already-dispatched pin (matched by
  `ProviderRef::storage_key()`) when the panel is pinned, instead of drawing from
  `[providers].order`.
- Run JSON carries a `fleet` array — `{seat, role, provider, model, source, projected}` —
  covering actual slots and, at the plan gate, the projected implement panel, resolved
  through the same `roles_resolve::build_implement_seats` slot creation uses so a
  projected seat's id always equals the id later created. `print_run_human` prints the
  same data as a `fleet:` table at gate phases.
- `--without critic,spec,suite` on `plan`/`implement`/`run` drops seats for one run:
  `[critic].enabled = false` (new config knob — before this there was no way to drop the
  critic without an absent `[roles].plan_critic` silently falling back to
  `[providers].order`), `[spec].enabled = false`, `[suite].enabled = false`. Dropping the
  critic also fixed the spec bus protocol to take `Option<&str>` for the critic id, so it
  never addresses a bus message to a critic that does not exist.
- `--fleet small|standard` presets, composing with `--without` and `--role` (preset, then
  `--without`, then `--role` — explicit flags win). `small` is one reviewer, no critic, no
  spec test-author, no agent tester; a configured deterministic `[suite].command` still
  runs.
- `--help` on `plan`/`implement`/`run` now states `--providers` is positional (index 0 is
  the planner/implementer, the rest reviewers) and overrides `[roles]` for the positions
  it covers, that a `--role reviewer` list sets the panel size exactly, and documents
  `--fleet`/`--without`.

## Non-goals

- No `--fleet deep` or other presets beyond `small`/`standard`.
- `--select`-chosen and `[tester]` preference-chosen seats report a documented source
  value (`model-select` / `suite-preferences`) but not a more granular one.
- Reaping an already-dispatched tester slot on `--without suite` mid-round; covered for
  fresh runs and frozen continuations, not a live resume.

## Verification

`tests/scenarios/fleet_shaping.rs`, 34 criteria (`ac1`–`ac34`), all green; full suite
(`cargo test --no-fail-fast`) green; `cargo fmt` / `cargo clippy --all-targets -- -D
warnings` clean.
