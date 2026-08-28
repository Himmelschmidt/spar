# Changelog

All notable changes to spar are recorded here.

## [Unreleased]

### Fixed

- **Dispatching acceptance tests to a coding agent could wipe out the work it had already
  done.** Every dispatch after the first quietly reverted source files in the agent's
  working copy to the versions on the test author's branch, so agents redid work they had
  finished, or handed back a branch that had silently lost a feature. Only the test
  author's unsaved work is copied across now; everything else arrives the way ordinary
  changes do, without overwriting anyone. Committing before a dispatch was never a way
  around this, and is no longer needed.
- **A dispatch that could not deliver the acceptance tests now stops and says so** instead
  of continuing quietly and grading the coding agent against tests that never reached it.

## [0.0.2] - 2026-08-27

### Fixed

- **Token counts were wrong, in both directions.** Spend reported for a run could be
  understated several times over for some agents and roughly doubled for others, so the
  totals spar showed were not a reliable basis for comparing models or deciding what a
  run cost. Every agent's numbers now reconcile against what the provider itself reports.
- **Slots could get stuck showing "running" forever.** A slot whose supervisor was
  stopped or crashed kept that status permanently, even after it had finished. Stopping a
  run now settles its slots, and a run whose supervisor died is settled the next time it
  is picked up. A slot stopped by hand is now recorded as halted rather than crashed.
- **A slot's own report of failure could be overwritten with success.** An agent that
  finished cleanly but reported that it had failed could have that report discarded.
- **Repeat attempts started from scratch.** When a review sent work back, the next
  attempt was given no idea what had been rejected, so it rediscovered the problem before
  it could fix it. It is now told which checks failed and why, including after a run is
  paused and resumed.
- **Default time budgets were too short.** On a fresh project the default cut off a
  substantial share of longer jobs partway through. Defaults are now sized against real
  run times.

### Added

- **Long-running work gets a nudge instead of a kill.** A slot that passes its time or
  spend budget is asked, repeatedly, to save what it has and say plainly what it did not
  get to and what it is stuck on, rather than being cut off. Budgets are per role. A much
  higher limit still exists as a backstop against something genuinely hung.
- **Runs stop asking for more attempts forever.** A run that keeps failing now pauses and
  asks you whether to continue instead of retrying indefinitely. Work that genuinely
  cannot be fixed is still reported as such rather than presented as a question.
- **Notes carried between attempts.** Each attempt can leave a short brief for the next
  one covering what it changed, what it tried and rejected, and where it got stuck.
- **A repair command for older runs**, which settles runs left in a stuck state by
  earlier versions. It reports what it would change and does nothing until you tell it to
  proceed, and it never touches your working copies.

### Changed

- **Agents keep long build and test output out of their working memory.** Output is saved
  to a file and read back as needed, which was previously re-read in full on every
  subsequent step and made long jobs progressively more expensive.
- **A paused run can no longer have its acceptance criteria quietly rewritten.** Resuming
  refuses to adopt a changed set of criteria unless you say so explicitly, and every
  change to them is now announced.

## [0.0.1]

Initial release.
