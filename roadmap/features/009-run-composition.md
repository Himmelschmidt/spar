---
id: 9
title: Run composition
status: backlog
milestone: 6
effort: L
priority: high
dependencies: [4, 8]
---

# 009: Run composition

## Summary

Turns the new-run surface into the run's full specification — name, workflow, and a fleet
assigned role by role with model, CLI and backup — and makes feature 008's orchestrator
conversation one way of *completing that specification* rather than a separate mode. Adds
operator-level defaults, editable from the TUI. Extends U8's picker and amends U7's Home
ranking.

Depends on feature 004 for the surface and feature 008 for the conversation that fills it.

## The organizing idea

**The form is the run's specification. The conversation is one way to complete it.**

Everything the operator can pick by hand, the orchestrator can propose in conversation, and
both produce the same object. That is what keeps this one feature instead of two competing
front doors: a half-filled form handed to the orchestrator is a briefing, and a fully
filled form is a launch. Nothing in the launch path needs to know which way it was filled.

## Problem

What shipped in 004 Phase D is a fleet *pool*, not a fleet. `new_run_launch` builds exactly
`spar plan -t <task> --providers <picked>` and nothing else, so:

- **The workflow is not a choice.** Every TUI-started run is `--workflow plan`. There is no
  way to start `implement -t`, `review`, or `arena` from the surface that exists to start
  runs.
- **Roles are not assignable.** The picker sets which providers are in play; roles are
  resolved afterwards from `[roles]` or positionally from `[providers] order`. The
  operator's actual CLI habit — `--role planner=cli:claude@opus --role
  reviewer=cli:codex@gpt-5.6-terra --role reviewer=cli:claude@opus` — has no UI equivalent.
- **Models cannot be pinned.** Roster rows are bare provider names. The only path to an
  `@model` ref is the recent-fleet row, which carries whatever the last run recorded. A bare
  `cli:claude` takes whatever the CLI defaults to that week, which is exactly what pinning
  exists to prevent.
- **There are no defaults to reuse or edit**, so every run is composed from scratch.

## Goals

- **Workflow selection, optional.** Pick `plan` / `implement` / `review` / `arena` up front,
  or leave it unset and let the orchestrator propose one once it understands the scope.
- **Role-by-role fleet assignment**: for each role the workflow actually uses, a provider, a
  model, and an optional backup. The roles are the six assignable ones; `ranker`,
  `reconciler` and `peer` keep taking fleet positions.
- **Declared per-role backups** (see below).
- **Operator-level defaults**, editable in the TUI, that pre-fill the form.
- **Home leads with creation** (see below).
- The conversation completes any part left blank, and the operator confirms before launch —
  U17's "proposes, disposes" is unchanged.

## Decisions taken here

Rows to be added to `DECISIONS.md` when this is scheduled; numbers are deliberately not
assigned yet, because three runs are in flight against this file and two `U`/`O` collisions
have already happened this cycle.

### Backups fire on environmental stops, and replace the blind rotation

A role's backup takes over when the primary is stopped by something that is not the work:
quota pause, provider unavailable, a rate limit. It does **not** fire on a genuine slot
failure — falling back there would silently change who answered for the work, and the value
of a two-vendor panel comes from knowing which vendor said what.

Additionally, a declared backup **replaces the stuck ladder's blind implementer rotation**
(O52's rotate → widen → stuck). Today a rotation moves to the next provider in the fleet,
which the operator never chose for that role. With backups declared, a rotation goes to the
provider they picked for exactly this case.

This interacts directly with the quota routing work: a backup can only be trusted to fire
on an environmental stop if the discriminator that classifies one is sound, so this feature
depends on that landing first.

### Operator defaults live under `spar_home()`, never the project's `spar.toml`

Defaults edited in the TUI are **operator-level**, stored under `registry::spar_home()`
alongside the registry and the evidence watermark. They are never written into the project's
`spar.toml`: one such file serves every worktree, so per-run role pins written there bleed
across parallel runs. A run still freezes its own resolved fleet into
`.spar/runs/<id>/config.json` (O27), so what a run reads is unchanged.

### Home leads with creation

Home's first element is the create prompt — "what would you like to build" — with running
and waiting runs alongside it. This amends U7, which made attention the organizing
principle.

**The risk this takes on, and the mitigation, are part of the decision.** U7 ranked
attention first because a gate the operator cannot see is the failure O36 exists to prevent.
Demoting the bands means the `⚑N need you` roll-up, the rail flags and the toast on
crossing into Gate/Broken become the *only* escalation on Home, so they stop being
decoration and become load-bearing. They must be impossible to miss, must agree with each
other (the roll-up and the bands disagreeing is exactly the U28 bug), and the count must
never be zero while something waits.

## Non-goals

- No change to the rail + main-area shape (X2, U1). The surface stays `f(rail selection)`.
- No new workflows. Selection chooses among the ones that exist.
- No autonomy change: the orchestrator proposes, the operator confirms at launch (U17).

## Phases

### Phase A: workflow selection and role-by-role assignment

Extend the new-run surface from a provider pool to a specification: workflow, then a row per
role the chosen workflow uses, each with provider and model. Reuse the existing roster for
availability and reasons.

### Phase B: operator defaults

A defaults store under `spar_home()`, read to pre-fill the form and editable from the TUI.
"Use defaults" becomes a pre-filled form rather than a separate path.

### Phase C: declared backups

Per-role backup assignment in the form, environmental-stop failover, and replacement of the
stuck ladder's blind rotation. Depends on the quota routing discriminator.

### Phase D: conversation completes the form

Wire feature 008's orchestrator to read and write the same specification object: it fills
what the operator left blank, including proposing a workflow, and hands back a completed
form for confirmation.
