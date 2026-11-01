# spar-pi

spar's human front end as a [pi](https://pi.dev) package (DECISIONS.md O145, roadmap
feature 012). Layer 1 is visibility (footer, run list, run detail); layer 2 is the human
gates as pi dialogs. Model-callable tools (layer 3) come later.

It is a thin client. Everything it shows comes from `spar <cmd> --json` child processes run
in the session's working directory; it never reads or writes `.spar/` itself. The `spar`
binary must be on `PATH` (or set `SPAR_PI_BIN` to a different binary, such as a dev build). It needs
a spar with `spar events --tail-cursor` and `--exit-on-stdin-eof`; an older one leaves the
footer on its 60 s status poll.

## Load it

One session, nothing written to settings:

```bash
pi -e /path/to/spar/pi
```

Install from a local checkout (written to `~/.pi/agent/settings.json`, or to
`.pi/settings.json` with `-l`). Local packages load in place and are not copied:

```bash
pi install /path/to/spar/pi
```

Installing from git is not supported: pi 1.4.0 clones a git source whole and reads the
package at the repository root, with no way to point it at a subdirectory, and spar's
root is deliberately not a pi package. Clone the repo and install `pi/` as a local path.

Requires Node 22.18 or newer (type stripping) for the tests; pi itself loads the
TypeScript directly.

## What it shows

### Footer

A status line, for example `spar | 1 running | 3 need you | paused codex | $12.40 today`:

- **running**: runs whose orchestrator holds a seat (`holds_seat`).
- **need you**: runs waiting on the operator (`needs_human`) that changed in the last 7
  days. Older ones still show in `/spar`, marked `stale`.
- **paused**: providers whose `quota_status` is not `available` in `spar provider list --json`.
- **today**: summed `cost_usd` from `spar stats --since <minutes since local midnight>m --json`.
  That counts the whole cost of every run that ended or changed today, not only what was
  spent today.

It starts in `session_start` and is stopped in `session_shutdown`. First it asks
`spar status --here --json` whether this directory is a project spar has initialized (has
a `.spar/`). If not, or if `spar` is missing, it shows nothing and asks again after 30 s,
doubling to 10 minutes, so a project initialized mid-session lights up without a restart.

Once initialized, it takes `spar events --tail-cursor --json` (the cursor for the current
end of every run's log) and starts a long-lived `spar events --follow --json --since
<that cursor>` child (O146) before the first status read, so nothing that happens in
between is missed, and a child that dies before its first event resumes from that cursor
rather than from a later "now". Each event
triggers a debounced re-read of `spar status --json` and `spar stats`; `archived` and
`abandoned` events also update the counts at once (an archived run leaves them, an
abandoned one stops counting as running). If the child exits it is restarted with
`--since <last cursor>` after a backoff of 2 s doubling to 60 s. Status is also re-read
every 60 s regardless, so a dead orchestrator and the midnight rollover of "today" show
up without an event. Provider pauses are not events, and `spar provider list --json`
probes every CLI, so it is polled every 5 minutes.

The events child is killed on `session_shutdown` and, as a backstop, on pi's process
`exit`. It runs with `--exit-on-stdin-eof` and a stdin pipe only pi holds, so it also
exits when pi is SIGKILLed. It runs only in modes with a UI (interactive and RPC).

### Alerts

The same feed raises one `ctx.ui.notify` when a run newly enters a state that needs you:
a gate (plan approval, winner confirm, ship confirm, round extension; a warning), or
stuck, escalated, quota, failed or abandoned (an error). Each run alerts once per state it
enters; the `gate` event that follows a phase event, or a restarted daemon reporting the
same abandonment again, is no repeat, and leaving for a state that needs nobody re-arms
it. Runs already waiting when pi starts stay quiet. `awaiting_reconcile` never alerts:
only `spar reconcile` sets it, as it starts.

### `/spar`

The run list, as an overlay: id, phase, round, age since the last change, a flag
(`NEEDS YOU`, `stale` for a run needing you untouched for over 7 days, or `running`),
cost (the row's `cost_usd`), workflow and the first line of the task. Runs needing you
come first, then running ones, then stale ones, then the rest by most recent change.
Enter opens the run's detail. One `spar status --json` call fills the whole list.

Outside a project, `spar status --json` lists runs from every registered project, and so
does `/spar`.

### `/spar <id>`

The run detail, as a scrollable overlay (up/down, pgup/pgdn, `b` back to the list, esc to
close):

- phase, round of max rounds, the needs-you or running flag, task and error;
- cost, total and per provider (`spar status <id> --json`); a trailing `+` marks a floor,
  some dispatches reported no cost;
- seats: role, slot, provider and model, status, round, and liveness flags;
- open review findings from the last review panel, grouped critical, major, minor
  (`spar brief <id> --json`), with follow-up and possibly-addressed notes;
- the last 10 run events and the last 8 bus messages (`spar bus log --run <id> --json`);
- the exact next command, as `spar brief` computes it.

Run ids complete after `/spar `, runs needing you first. An argument that is not a run
id (`/spar --all`) is refused rather than passed to spar.

Outside the interactive terminal (`pi -p "/spar"`, RPC, JSON mode) both commands print a
plain-text version instead: to stdout in print mode, as a notification over RPC, to stderr
in JSON mode.

## Gates

The run detail shows the actions valid for the run on an `actions:` line, one key each.
Every action opens a dialog that states the exact command it will run, runs it only after
you confirm, and shows the result: `done` with the new phase, or the exit code, its
meaning (`1` failed, `2` human gate, `3` stuck, `4` quota) and spar's stderr or JSON
`error` in full. Ids and text go to spar as separate arguments, never through a shell.

| Key | Verb | Offered when | Runs |
| --- | --- | --- | --- |
| `a` | approve | `awaiting_plan_approval` | `spar approve <id> --json` |
| `x` | reject | `awaiting_plan_approval` | `spar reject <id> [--reason=<text>] --json` (reason optional) |
| `s` | ship | `awaiting_ship_confirm` | `spar ship <id> --confirm --json`, in the background |
| `o` | override | ship gate, winner confirm, round extension, stuck, failed, quota | `spar ship <id> --override-gate --reason=<text> --json` (reason required), in the background |
| `c` | confirm | `awaiting_winner_confirm`, `awaiting_reconcile` | `spar confirm <id> [--winner <slot>] --json`: the ranked default or a pick among the implementers |
| `m` | reconcile | `awaiting_winner_confirm`, `awaiting_reconcile` | `spar reconcile <id> --json`, in the background |
| `e` | extend | `awaiting_round_extension` | `spar implement --run <id> --max-rounds +N --detach --json` (N defaults to 1) |
| `r` | resume | `resumable`, not at a gate | `spar resume <id> --detach --json` |
| `k` | stop | in flight (not at rest) | `spar stop <id> --json` |
| `u` | rule | any run | `spar rule add --run <id> [--waive AC-n]... --json -- <text>` |
| `n` | reassign | not holding a seat, not finished | `spar implement --run <id> --reassign <seat>=<provider> --detach --json` |
| `d` | cleanup | `done` | `spar cleanup <id> --if-merged --json`, in the background |

The mapping is a hint, not a rule: spar decides, and a refusal is shown as it came.
`ship` on a run whose review never cleared (round extension, stuck, failed, quota) says the
gate refuses it and offers the override instead. After an action the detail reloads.

Nothing blocks pi for long. `extend`, `resume` and `reassign` use spar's own `--detach` and
return once the orchestrator is up. `ship`, `override`, `reconcile` and `cleanup` run as a
detached child in a session of its own, with output in a temp directory, so they finish
even if pi exits; the result arrives as a notification, and on failure names the
directory with the full output.

The same actions are subcommands: `/spar <verb> <id> [text]`, for example
`/spar approve 3d3d6f59`, `/spar reject 3d3d6f59 scope is wrong`,
`/spar extend 3d3d6f59 2`, `/spar rule 3d3d6f59 keep the API`,
`/spar reassign 3d3d6f59 impl=cli:claude@sonnet`. Text after the id fills the dialog's
input; the confirmation is still asked. Without an id it asks which run, among those the
verb applies to. Verbs complete after `/spar `, and after a verb, the ids it applies to.
They need a dialog: over RPC they use pi's dialog protocol; in print and JSON mode they
refuse and run nothing.

## Develop

```bash
cd pi
npm install
npm run typecheck
npm test
```

Tests use node's built-in runner and a fake `spar` (`test/fake-spar.cjs`) put first on
`PATH`, which answers from canned JSON and canned event streams, so no test needs a real
project. Everything that depends on the `spar events` line shape lives in
`extensions/spar/feed.ts`.

## Survey of existing pi packages

Before building, npm was searched for `pi-package` packages (the pi gallery's discovery
keyword) and read the bundled extension examples.

Reused:

- **pi's own components.** The run list is `SelectList` with pi's select-list theme and
  `DynamicBorder`, as `examples/extensions/preset.ts` does; completion uses
  `getArgumentCompletions`; the footer is `ctx.ui.setStatus`, as `status-line.ts` does,
  so it sits beside other extensions' statuses instead of replacing the footer.
- **The example lifecycle patterns**: resources started in `session_start` and released
  idempotently in `session_shutdown`, and `ctx.mode === "tui"` guarding custom UI.

Not reused, and why:

- **Footer packages** (`pi-powerline-footer`, `pi-minimal-footer`,
  `@firstpick/pi-extension-git-footer-status`, `pi-quota-status`): each replaces pi's
  footer with its own content. spar needs one status segment, and `setStatus` gives that
  and composes with whichever footer the user runs.
- **Dashboard and overlay packages** (`@tmustier/pi-usage-extension`,
  `@moguw/pi-stats-dashboard`, `pi-files-widget-overlay`, `@juicesharp/rpiv-todo`): views
  over pi's own session data or files, not a table fed by an external CLI.
- **UI component libraries** (`@agimon-ai/po-ui`): alpha, and pulls in a runtime spar-pi
  does not need. pi-tui already has the list and text components.
- **`questionnaire.ts`, `notify.ts`, `todo.ts` examples**: looked at again for layer 2.
  The gates need one confirm, sometimes one input or one pick, which pi's built-in
  `ctx.ui.confirm` / `input` / `select` already are, and those also work over RPC, where a
  custom questionnaire component would not. `notify.ts` sends desktop notifications from
  the agent loop; spar's alerts come from the events feed and go through
  `ctx.ui.notify`. `todo.ts`'s branch-aware state has no counterpart: gate state lives in
  spar.

The scrollable detail view is about 40 lines written here. pi-tui's `ScrollView` needs
layout integration that an overlay component doesn't get.
