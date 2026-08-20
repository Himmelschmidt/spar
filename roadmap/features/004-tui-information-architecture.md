---
id: 4
title: TUI information architecture
status: backlog
milestone: 6
effort: L
priority: high
dependencies: [3]
---

# 004: TUI information architecture

## Summary

Settles the TUI's noun vocabulary to Project / Run / Agent / Shell, retires "session" as a
user-facing term, replaces the project-browser front door with a Home landing view, and
adds the new-run intake flow. Decisions: `DECISIONS.md` U6, U7, U8, U13. See
`docs/architecture-tui-ia.md` for the full information architecture.

Depends on feature 003: the target flow needs a run worth landing on before the landing
view matters.

## Problem

"Session" is overloaded four live ways (`spar-<run_id>` tmux session, the Shell tab's
workspace fallback, `/spawn` agents, provider session logs), and `docs/PRODUCT.md:43` bakes
the conflation into the product's own pillar list. There is matching code drift:
`src/tui.rs:55` still describes a three-target focus model (`Focus` has two variants, `3`
is unbound, `App` has no composer field) and `src/tui.rs:4161` still calls itself "resolve a
composer mention." There is also no landing view: the TUI opens onto
`BrowseLevel::Projects`, a file browser, even though the attention roll-up
(`runs_needing_attention`) already computes what needs the operator. And there is no way to
start a run without one already selected, per U3's deliberate CLI punt.

## Goals

- Noun set fixed at Project / Run / Agent / Shell; "session" survives only as the tmux
  backend's internal name.
- Stale composer-focus comments at `src/tui.rs:55` and `src/tui.rs:4161` corrected to match
  the real two-target focus model.
- Home replaces `BrowseLevel::Projects` as the landing view: what needs me (ranked by wait
  time), running, finished since last look, start something new.
- New-run flow (`n` on Home): brief field plus fleet picker, superseding U3's punt.
- The render-path directory scan in `rail_project_items` (`src/tui.rs:2830`) moves onto
  the off-thread `Snapshot` the refresher already builds correctly (`src/tui.rs:873`).

## Non-goals

- No gate-evidence rendering (that is 005).
- No motion or visual-identity work (that is 006).
- No conversation surface inside the TUI (X10, left open).

## Phases

### Phase A: noun vocabulary purge and comment cleanup

Replace user-facing "session" language across the TUI with Project / Run / Agent / Shell.
Fix the stale focus-model comments at `src/tui.rs:55` and `src/tui.rs:4161`.

### Phase B: off-thread `Snapshot` directory scan migration

Move `rail_project_items`'s `registry::list_visible_project_runs` call
(`src/tui.rs:2830`) onto `Snapshot`, matching the refresher's existing off-thread path
(`src/tui.rs:873`), so `draw` never touches disk. `project_overview`'s call
(`src/tui.rs:1312`) already runs inside `build_snapshot` and needs no change.

### Phase C: Home landing view and wait-time ranking

Build the four Home bands and the wait-time ranking for "what needs me," promoting U5's
attention level from rail decoration to organizing principle.

### Phase D: new-run intake modal

Brief-entry field writing `.spar/briefs/<slug>.md` plus a fleet picker over the roster,
reachable with `n` from Home.
