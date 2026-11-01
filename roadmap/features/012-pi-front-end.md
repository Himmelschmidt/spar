---
id: 12
title: pi front end
status: planned
milestone: 7
effort: L
priority: high
dependencies: []
---

# 012 — pi front end

## Summary

Replaces the TUI with `spar-pi`, a pi package under `pi/` that drives the CLI through
`--json` and gives the operator visibility and gates inside a pi session.

Decisions: `DECISIONS.md` **O145**, **O146**.

## Phases

- **A. Event feed.** `spar events [--follow] [--since <cursor>] --json` over the per-run
  `events.jsonl` logs; daemon abandonment appended to the log (O146).
- **B. Visibility.** Footer (runs in flight, runs waiting on you, paused providers, cost
  today) fed by phase A; `/spar` run list and `/spar <id>` run detail. Read-only.
- **C. Gates.** Approve/reject, ship (with override and reason), round extensions,
  reassign and rulings as pi dialogs. TUI deleted when this lands.
- **D. Agent tools.** Model-callable spar tools, result renderers, and the core skill
  shipped as a pi skill.

## Prerequisites

- pi 1.4.0 installed on the dev box (`~/.local/bin/pi`).
- A survey of existing pi packages before building any widget by hand.
