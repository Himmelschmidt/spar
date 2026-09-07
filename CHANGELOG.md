# Changelog

All notable changes to spar are recorded here.

## [Unreleased]

### Added

- **You can see the reviewers, planner and other seats a run will actually use before
  it spends anything on them.** The run's status now lists every seat, where each one's
  provider came from, and marks the ones that haven't been dispatched yet — including the
  review panel, visible at the point you're deciding whether to approve a plan.
- **You can drop seats for a single run without editing shared settings.** A new option
  turns off the plan critic, the pre-coding test writer, or the automatic test runner for
  just the run you're starting.
- **Two ready-made fleet sizes.** One option gives you today's defaults; the other gives
  you the smallest useful setup — one reviewer, no critic, no test writer, no automatic
  tester — while still running your project's own test command if you've configured one.

### Fixed

- **Watching or checking on a run no longer fails at random while the run is writing.**
  A run's status file was rewritten in place, so anything reading it at the wrong moment
  could see half a file and give up with a parse error mid-run.
- **Pinning a single reviewer no longer quietly adds a second one you didn't ask for.**
  Naming exactly one reviewer now means exactly one reviewer, instead of getting padded
  out with an extra, unrequested provider.
- **Assigning a role to a specific provider no longer gets silently overridden** when you
  also pass a general provider list. The specific assignment now wins, and it keeps
  winning if you come back to the same run later without repeating yourself.
- **A run stopped by your provider's rate limit is no longer reported as a failure.**
  It now stops with the status that means "out of tokens for now" rather than the one
  that means "the work broke", so anything reading that status can tell the two apart.
- **A run stopped that way can be picked up again.** Previously it was stuck: the only
  way forward was to start a second run for the same work, which threw away its plan,
  its agreed test criteria and everything it had already done.
- **Waiting now lasts as long as the provider says it will.** When the provider states
  when your access comes back, spar waits until then instead of guessing, and stops
  retrying into the same wall in the meantime.
- **Rate limits are noticed everywhere runs happen.** Reviews and paired runs, which run
  several agents at once, previously missed them entirely.
- **A quick review run with one pinned reviewer no longer tries to fill a second seat
  from your default provider list.** It now runs exactly the panel you pinned.
- **Continuing an approved plan into implementation no longer refuses when the plan
  itself only ever needed one or two providers.** It picks up where the plan left off
  instead of demanding you repeat the provider list.
- **A pinned test writer no longer wins over a provider list you passed for this run.**
  The provider list now wins for that seat too, matching every other role.
- **The "where did this seat come from" label is accurate for review, paired, and
  competing-agent runs**, not just the main implement flow.
- **The smallest fleet size no longer disables retrying a failed reviewer.** Narrowing
  the review panel to one seat still lets that seat be retried on a different provider
  if it fails, the same as it would with the default panel size.
- **The "where did this seat come from" label is also accurate when a failed
  implementer or reviewer gets rotated or an extra reviewer gets added**, not just on
  first dispatch.
- **The plan approval screen now shows the model an already-chosen provider will
  actually use**, when one was picked earlier in the run, instead of always showing
  none until after you approved.
- **Applying the "today's defaults" fleet size to a run already in progress no longer
  fails.** It changes nothing, so it no longer needs the flag that reloads settings
  from disk.
- **The plan approval screen no longer hides reviewer seats when you gave it a shorter
  provider list than the review panel needs.** It now shows the full panel implementation
  will actually dispatch, cycled from the providers you gave it, instead of silently
  dropping the seats past your list's own length.
- **The "where did this seat come from" label is accurate for paired and competing-agent
  runs given a shorter provider list than they need**, not just their first seat.

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
