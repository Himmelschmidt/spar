---
id: 7
title: One run per unit of work
status: in-progress
milestone: 6
effort: L
priority: high
dependencies: []
---

# 007: One run per unit of work

## Summary

A run is a **unit of work** — an issue, or a bundle of them — from "we want X" to "X is
in a draft PR". It appears in a listing exactly once, for its whole life, however many
agents and rounds it takes. Planner, critic, test author, implementer, tester, reviewers,
a replan after a rejection, three fix rounds: those are steps *inside* a run, visible when
you open it, never rows beside it.

The invariant already says this ("**One run id** threads plan → implement → ship",
`AGENTS.md`). Nothing enforces it, and the CLI offers a bypass that outer agents take by
default. Decisions: `DECISIONS.md` O45, O46, U15.

## Problem

biddesk has 106 visible runs. They are not 106 units of work:

| runs | slots present | what it really is |
|---|---|---|
| 24 | planner, critic, spec, impl, tests, review | the intended full pipeline on one id |
| 35 | impl, tests, review — **no planner** | implement legs started as fresh runs |
| 31 | planner, critic (±spec) — **no implementer** | plan legs, stranded |
| 13 | reviewer only | `spar review`, legitimately its own workflow |
| 3 | peer / empty | misc |

So ~66 of 106 rows are halves of ~33 units of work, and nothing on disk links them:
`parent_run` is set on **zero** runs. The split is not spar forking a run — it is the call
site. `run_from_cli` (`src/workflow/implement.rs:27-32`) accepts `--plan <path>` with no
`--run` and mints a fresh id, auto-titling it `Implement approved plan from <path>`. Three
dozen of biddesk's rows carry exactly that title. `spar plan` has no `--run` at all, so a
replan after a rejection can only be a new run.

The cost is not tidiness. biddesk's 9 real gates — work actually waiting on the operator,
oldest 4 days — sit buried among legs of work already dealt with.

## Goals

- A command that is plainly continuing an existing unit of work attaches to it as a new
  **round**; minting a new id takes an explicit `--new` or a genuinely new brief.
- `spar plan --run <id>` replans in place: a second plan round on the same run.
- Rounds are first class in state, so "15 agents" reads as "round 1: plan + critique ·
  round 2: spec, build, tests, review ×2 · round 3: fix, review ×2".
- Legs that already exist can be linked by hand (`spar link`), and the TUI folds a linked
  leg into its parent's row, rolling its attention up.
- `spar archive --all` can reach the hand-archivable phases in bulk, so 52 stopped runs do
  not have to be archived one at a time.

## Non-goals

- **No fifth noun.** Project / Run / Agent / Shell (U6) stands; the run *is* the issue.
  A "thread" or "issue" object above the run would re-split the vocabulary U6 settled.
- **`spar review` stays its own workflow.** Pointing a review panel at a branch spar did
  not build is a real use, and it gets its own run and its own row.
- No automatic merging of the existing 66 half-runs. Nothing links them on disk and
  pairing them by task text would be guesswork; linking is an operator action.

## Phases

### Phase A: rounds in state

`RunState.round` and `SlotState.round` (both defaulting to 1, so pre-existing runs read as
round 1). A helper that opens a new round on an existing run: bump the counter, clear the
terminal verdict, stamp the slots it dispatches. Reopening an archived run already clears
`archived_at` (`src/state.rs:389`) and that behaviour carries.

### Phase B: attach by default

`implement` refuses to mint a new id when it is plainly continuing one: a `--plan` path
inside `.spar/runs/<id>/artifacts/` resolves to that run, and a bare `-t`/`--plan` errors
with the candidate runs named (approved, awaiting approval, or stopped, most recent
first). `--new` is the explicit escape. `plan --run <id>` opens a replan round.

### Known limitation

A folded row drills into the leg it acts on, not into the union of the unit's legs. An
earlier build merged every leg's slots into one view; it put agents, worktrees and tmux
windows from one run under another run's id, so a takeover attached to the wrong pane and
two implement legs with identical slot ids hid each other. Merging needs slots keyed by
`(run_id, slot_id)` and per-slot worktree/heartbeat resolution — a follow-up, not a
rename.

### Phase C: linking and folding

`spar link <run> --to <parent>` writes `parent_run`, `--undo` clears it. The TUI folds a
linked leg into its parent's row: one row per unit of work, showing the most recently
updated leg's phase and rolling its attention flag up. `status --json` keeps listing every
run and carries the link, so outer agents lose nothing.

### Phase D: bulk archive

`spar archive --all` gains a flag for the hand-archivable phases (`stopped` / `failed` /
`stuck` / `quota`), which auto-archiving deliberately never touches (`src/state.rs:645`).
Still non-destructive and still reversible with `--undo`.
