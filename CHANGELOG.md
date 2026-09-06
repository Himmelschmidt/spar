# Changelog

All notable changes to spar are recorded here.

## [0.0.3] - 2026-09-04

### Added

- **The TUI now opens on a Home screen** that shows what needs you across every
  project, what's running, what finished since you last looked, and a way to start
  something new — instead of dropping you straight into one project's run list.
- **You can start a new run from inside the TUI**, picking which providers to use
  from a list, instead of being sent to the command line whenever no run was already
  selected.
- **Running a project's test suite no longer requires starting a separate agent for it.**

### Added

- **The TUI now opens on a Home screen** that shows what needs you across every
  project, what's running, what finished since you last looked, and a way to start
  something new — instead of dropping you straight into one project's run list.
- **You can start a new run from inside the TUI**, picking which providers to use
  from a list, instead of being sent to the command line whenever no run was already
  selected.

### Fixed

- **The help screen cut its own text off mid-word** and could not be scrolled, so several
  keyboard shortcuts were unreadable. It now sizes itself to fit, wraps instead of
  chopping, and scrolls when the window is short.
- **The command palette never showed its hint line, and four of its twelve commands could
  not be reached** by scrolling the list. Every command is now reachable and the list
  shows where you are in it.
- **A scrollbar appeared on panels that had nothing to scroll.**
- **The gaps between tabs were uneven**, and on narrow windows the labels ran together
  with no space between them.
- **Opening a project with no runs showed a broken heading and leftover text from
  whatever was on screen before.** It now shows a single clear message and how to start.
- **A run waiting on you could vanish from the "needs you" list.** When a run's plan had
  been approved while another part of the same work was still running, the list showed
  nothing while the counter beside it still said one — so the two disagreed and the
  handoff was easy to miss. The list and the count now always agree.
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
