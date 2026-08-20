# Operator model: the session is disposable

**Status:** DECIDED (product planning)  
**Decisions:** P7, O38-O42, X9, X10  
**Supersedes:** nothing. Extends B1 (notify sink), Z2 (abandoned is computed), O1 (one run id).

---

## The symptom

The outer agent driving a spar session reaps its own monitor or gets reaped, and the run
then sits idle for a long time after a stage finishes or while waiting at a gate. Nobody is
told, so the operator finds out only by checking.

---

## Root causes

### Cause 1: `--detach` does not detach

`detach_self` (`src/workflow/plan.rs:523`) and `detach_implement`
(`src/workflow/implement.rs:1446`) spawn `__internal_continue` with a plain
`Command::spawn()`: no `setsid`, no `process_group(0)`. Compare `src/process.rs:243`, where
slots do get their own group. The "detached" orchestrator stays in the calling shell's
process group and session. When the harness kills that group (a Bash tool timeout, an
aborted turn, session teardown) or the tty hangs up, the orchestrator takes the signal, and
because `install_shutdown_handler()` is armed for `InternalContinue` (`src/main.rs:74-81`)
it reaps its own slots on the way out. `skills/core.md:280` currently promises the opposite:
"Prefer `--detach` + `spar wait` ... so a command timeout in your harness cannot orphan a
fleet." It can, and the correction is scoped into feature 003.

### Cause 2: nothing pushes when a run reaches a gate

`RunState::save` already detects phase transitions and appends `phase` plus `gate` events
(`src/state.rs:441-454`), but the only route to the external `[notify]` sink is `bus::send`
routing an `@human`-addressed message (`src/bus.rs:451-454`). A run arriving at
`awaiting_plan_approval` or `awaiting_ship_confirm` never reaches the notify sink: nothing
sends a message that would trigger it. The operator finds out only if a live `spar wait`
happens to be attached.

### Cause 3: the monitor is single-shot and fragile

`spar wait --follow` is a foreground blocking process with a default cap. Whatever kills the
shell kills it, and re-establishing it is something the outer agent has to remember to do.
No supervisor exists.

---

## The reframe

The chat session is doing two jobs with incompatible lifetimes: thinking partner (bursty,
minutes) and run supervisor (hours). Every symptom above is downstream of welding those
together. The fix is to make the session disposable, not to make the wire more robust.

The session's job ends at launch. It converts a conversation into a written brief, launches
a detached run, and is free to die. spar owns everything after: a per-project daemon drives
phases, notifies the human at gates, and detects abandonment. When the operator wants to
talk about a run again, any session reads `.spar/runs/<id>/` and is instantly current.
Sessions become interchangeable clients of run state instead of stateful owners of it.

---

## Ownership table

| Owner | Lifetime | Responsibility |
|---|---|---|
| Chat session | minutes, disposable | Brief generation, launch, status inspection |
| Detached orchestrator (`__internal_continue`) | one run | Advances phases, holds `RunLock` |
| Per-project daemon | as long as the project has live runs | Gate notifications, abandonment detection, cross-run queue |
| `.spar/runs/<id>/` | permanent | System of record |

---

## The per-project daemon (O40)

Supervision responsibilities:

- Restart a dead orchestrator on a run that still holds work worth resuming.
- Push lifecycle notifications (gate, stuck, quota, abandoned, terminal failure) into the
  `[notify]` sink (`src/notify.rs:24`), the same sink B1 already defines.
- Compute abandonment, extending Z2's read-time flag into an active check rather than one
  that waits for a `status` call to notice.
- Hold the cross-run concurrency queue (see below) and release queued runs as capacity
  frees up.

Strict prohibitions, because a supervisor is not an operator:

- Never approve a plan.
- Never confirm ship.
- Never merge.
- Never sweep `cleanup`.
- Never choose a fleet.

Every one of those stays a human decision, made through the TUI or CLI. The daemon's whole
job is to make sure the human is told when a decision is waiting, not to make it for them.

---

## The notification path

Today `RunState::save` (`src/state.rs:441-454`) detects a phase transition and writes
`phase`/`gate` events to the run's event log, but nothing downstream of that turns the event
into a push. O39 fires lifecycle notification from that same choke point: gate, stuck,
quota, abandoned and terminal failure all route into the existing B1 notify sink
(`src/notify.rs:24`), which already defines `command`/`webhook` config and already no-ops
safely with neither configured. Silence means healthy: the operator does not poll, they wait
to be told.

---

## Task intake and recovery (O41)

Task intake is a written brief on disk: `spar plan --spec <file>` or stdin, stored at
`.spar/briefs/<slug>.md`. `spar brief <id>` re-hydrates a fresh session in one call, and
`spar resume <id>` recovers a stopped or abandoned run. Both give a disposable session a
way to get current without having lived through the run.

---

## The target flow

1. Operator talks a task through with a chat session, exactly as today.
2. "do it, use spar." The session writes a brief to `.spar/briefs/<slug>.md`, shows the
   fleet, and on approval runs `spar plan --spec <brief> --detach`. A run id returns in
   under a second.
3. Repeat for further tasks. Then the session is finished: closing, clearing or compacting
   it holds nothing.
4. Silence while runs proceed. Silence means healthy.
5. Push at each gate. Operator opens spar, lands on Home, sees what needs them ranked by
   wait time, reads the plan and critique in a real Plan tab, approves.
6. Silence again. Stuck / quota / abandoned all push, and `spar resume <id>` recovers.
7. Push at ship. Review tab shows per-`AC-n` pass/fail/unverified, reviewer verdicts, the
   diff. Confirm opens a draft PR and never merges.
8. `spar cleanup <named ids>`. Never a sweep.

A second entry path starts from the TUI (`n` on Home) for tasks that need no conversation.
Path 1 remains correct whenever a task needs thinking through. The flow deliberately does
not require the TUI to host a conversation; that question stays open under X10.

---

## Making `--detach` detach (O38)

O38 makes `--detach` actually detach: `setsid` / `process_group(0)`, plus a startup
handshake where the parent confirms the child holds the `RunLock`
(`src/runlock.rs`) before reporting success. Today it prints "detached" and a child that
dies at startup is invisible for the whole `abandon_grace` window (`src/executor.rs:1874-1880`,
15s default). Once the child has its own session, killing the harness that launched it no
longer signals it, and the daemon becomes the thing that notices if it dies anyway.

---

## Cross-run concurrency cap (O42)

`max_agents` (`src/config.rs:10`) is per-run fleet width. There is no cross-run concurrency
cap anywhere. Under the standard fleet, planner, test_author, implementer, tester and one
reviewer all resolve to `cli:claude`, and `@model` variants share a quota bucket with the
bare provider (`storage_key()`, `src/provider_ref.rs:94`, X8) so that is five of seven seats.
Three runs launched together is fifteen Claude-bucketed slots contending, and a single quota
limit exits `4` in all three at once, costing three plans, three test contracts and half of
three review panels simultaneously. Disk compounds it: one worktree per slot per run, each
with its own `target/`.

The cap belongs in the daemon, not in a single run's config, because the contention is
across runs: a run cannot see the other runs it is competing with, but the daemon supervises
every run in the project and is the only thing positioned to queue one behind another.

---

## Rejected alternatives

**Walking `/proc` ancestry for tmux pane resolution.** An earlier design resolved the
driving agent's tmux pane by walking `/proc` ancestry (verified working:
`bash -> claude 1505900 -> fish 7283 -> tmux`, and `tmux list-panes -F '#{pane_id} #{pane_pid}'`
matches `7283` to pane `%42`). Rejected: it exists only to compensate for a session being
load-bearing, which the disposable-session model removes entirely. If an agent must mind a
run between gates, spar should spawn it on spar's own socket (`/spawn`, W1), where spar is
the parent and pane resolution is definitional rather than reconstructed.

**Supervising `spar wait`.** Wrapping the existing foreground `spar wait --follow` in
another layer of process supervision was considered and rejected: it preserves the brittle
session lifecycle instead of removing it. A daemon that restarts a `wait` process is still a
daemon that depends on some session having started that `wait` process in the first place.

---

## Sequencing and open questions

Feature 003 (durable run ownership) unblocks the target flow entirely on its own, with no
TUI work: `setsid` detach, the notification path, `.spar/briefs/`, `spar resume`, and the
daemon are all orchestrator-side. The flow works, gated through the TUI as it exists today,
the day 003 lands.

X10 is open: whether the TUI ever hosts a conversation. The target flow above deliberately
does not require it, and this doc does not design one in.
