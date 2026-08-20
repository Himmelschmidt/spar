---
id: 3
title: Durable run ownership
status: backlog
milestone: 6
effort: L
priority: high
dependencies: []
---

# 003: Durable run ownership

## Summary

Makes a spar run survive the chat session that launched it. `--detach` gets a real
`setsid` handshake, lifecycle notifications fire from the phase-transition choke point, a
per-project daemon supervises runs between gates, and task intake moves onto a written
brief. Decisions: `DECISIONS.md` P7, O38, O39, O40, O41, O42. See
`docs/architecture-operator-model.md` for the full "why".

The target flow lands with no TUI work: this feature is entirely orchestrator-side, gated
through the TUI as it exists today.

## Problem

The outer agent driving a spar session reaps its own monitor or gets reaped, and a run then
sits idle after a stage finishes or at a gate with nobody told. Three root causes, all
verified against the code: `--detach` spawns `__internal_continue` with a plain
`Command::spawn()` (`src/workflow/plan.rs:523`, `src/workflow/implement.rs:1446`), no
`setsid`, so it dies with the launching harness; `RunState::save` (`src/state.rs:441-454`)
records phase and gate transitions but nothing routes them to the external notify sink; and
`spar wait --follow` is a foreground process with no supervisor of its own.

## Goals

- `--detach` actually detaches: `setsid` / `process_group(0)`, plus a startup handshake so
  the parent only reports success once the child holds the `RunLock`.
- Lifecycle notification (gate, stuck, quota, abandoned, terminal failure) fires from
  `RunState::save`'s phase-transition point into the existing `[notify]` sink
  (`src/notify.rs:24`).
- `spar plan --spec <file>` / stdin writes a brief to `.spar/briefs/<slug>.md`; `spar brief
  <id>` re-hydrates a fresh session; `spar resume <id>` recovers a stopped or abandoned run.
- A per-project daemon restarts a dead orchestrator, pushes notifications, computes
  abandonment, and holds a cross-run concurrency queue keyed on the quota bucket
  (`storage_key()`, `src/provider_ref.rs:94`).
- Correct the false claim at `skills/core.md:280` ("a command timeout in your harness
  cannot orphan a fleet").

## Non-goals

- No TUI work. Gate approval, ship confirm and fleet selection stay human decisions made
  through the TUI or CLI exactly as today.
- The daemon never approves a plan, confirms ship, merges, sweeps `cleanup`, or picks a
  fleet.
- No conversation surface in the TUI (X10, left open).

## Phases

### Phase A: `setsid` detach and startup handshake

`detach_self` and `detach_implement` spawn the `__internal_continue` child with `setsid` /
`process_group(0)` (compare `src/process.rs:243`, where slots already get their own group).
The parent waits for the child to confirm it holds the run's `RunLock` before printing
"detached" and returning, so a child that dies at startup is visible immediately instead of
for the whole `abandon_grace` window (`src/executor.rs:1874-1880`).

### Phase B: lifecycle notification routing

Fire notification from the single `RunState::save` choke point (`src/state.rs:441-454`)
into the existing B1 sink (`src/notify.rs:24`), covering gate, stuck, quota, abandoned and
terminal failure. Silence means healthy.

### Phase C: CLI surface

`spar plan --spec <file>` and stdin intake, writing `.spar/briefs/<slug>.md`. `spar brief
<id>` to re-hydrate a fresh session from a run's state. `spar resume <id>` to recover a
stopped or abandoned run. Correction of `skills/core.md:280`.

### Phase D: per-project daemon

Supervision loop: restart a dead orchestrator holding resumable work, push lifecycle
notifications, compute abandonment (extends Z2), and enforce the cross-run concurrency cap,
releasing queued runs as capacity frees up. Strict prohibitions carried over from the
architecture doc: never approve, confirm, merge, sweep, or pick a fleet.
