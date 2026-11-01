---
id: 13
title: herdr monitor
status: in-progress
milestone: 8
effort: M
priority: medium
dependencies: [12]
---

# 013 - herdr monitor

## Summary

An optional herdr plugin under `herdr/` that makes herdr a standalone monitor for spar
runs: one herdr agent pane per run that matters, state reported to herdr's sidebar, a gate
popup and seat log panes, all through `spar ... --json`. pi stays a separate, also
optional front end.

Decision: `DECISIONS.md` **O150**.

## Phases

- **A. Run panes.** A run view per run following `spar events --follow --run`, reporting
  blocked / working / idle and metadata to herdr; exits when the run is archived or gone.
- **B. Sync and board.** Sync action and startup hook reconcile panes with
  `spar status --all --json`; a board pane re-syncs on lifecycle events; restored panes are
  replaced after a herdr restart.
- **C. Gates and logs.** Gate popup with the verbs valid for the phase and a typed
  confirmation; seat log panes on `spar logs --follow`.
- **D. Later.** Cross-host runs (`--all-hosts`), publishing to the herdr marketplace.

## Prerequisites

- herdr 0.9.5 or later, node 22.18 or later.
