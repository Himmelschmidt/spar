---
id: 5
title: Gate evidence
status: backlog
milestone: 6
effort: M
priority: high
dependencies: [4]
---

# 005: Gate evidence

## Summary

Makes the plan and ship gates show the evidence they decide on: `plan.md`, the critique and
`test-contract.md` as real documents in the Plan tab, and per-`AC-n` pass/fail/unverified
status alongside reviewer verdicts and the diff in the Review tab. Decisions:
`DECISIONS.md` U9, extending U4. Anchored on O15 (`test_author` freezes acceptance criteria
before code exists) and `require_all_criteria`.

Depends on feature 004: evidence rendered into a confusing layout is still confusing.

## Problem

The Diff tab falls back to dumping the artifacts directory, commented "the run's artifacts
for now (no new plumbing in Stage A)" (`src/tui.rs:3208`). Nothing renders `plan.md` as a
document, nothing shows the critique, nothing shows `test-contract.md`, and
`grep -n "criteria\|verdict" src/tui.rs` returns nothing. The ship gate's whole premise is
the acceptance predicate (O19): a run cannot pass while any `AC-n` is fail or unmentioned,
and `unverified` blocks too unless `[review].require_all_criteria = false` relaxes it. A gate button with no way to see the criteria it enforces is a rubber stamp.

## Goals

- Plan tab renders `plan.md`, the critique and `test-contract.md` as documents.
- Review tab renders a per-`AC-n` criteria grid: pass / fail / unverified, plus reviewer
  verdicts.
- Diff tab keeps its real `git diff HEAD` (U4) and gains a "since you last looked" watermark
  so a returning operator sees what changed, not the whole history again.

## Non-goals

- No new acceptance-criteria semantics; the acceptance predicate (O19) is unchanged, only
  made visible.
- No motion or visual-identity work (that is 006).

## Phases

### Phase A: Plan tab document rendering

Render `plan.md`, the plan critique and `test-contract.md` as formatted documents rather
than the current artifacts-directory dump (`src/tui.rs:3208`).

### Phase B: Review tab criteria grid

Parse and render per-`AC-n` pass/fail/unverified status plus reviewer verdicts, sourced from
the same contract `require_all_criteria` already enforces.

### Phase C: diff integration and watermark state

Wire the diff view and reviewer verdicts into the Review tab alongside the criteria grid,
and add "since you last looked" state so a returning operator can distinguish new activity
from what they already reviewed.
