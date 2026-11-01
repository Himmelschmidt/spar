# spar-pi

spar's human front end as a [pi](https://pi.dev) package (DECISIONS.md O145, roadmap
feature 012). This is layer 1: read-only visibility. Gates (layer 2) and model-callable
tools (layer 3) come later.

It is a thin client. Everything it shows comes from `spar <cmd> --json` child processes run
in the session's working directory; it never reads or writes `.spar/` itself. The `spar`
binary must be on `PATH` (or set `SPAR_PI_BIN` to a different binary, such as a dev build).

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

Installing straight from git does not work yet. pi 1.4.0 clones a git source whole and
reads the package at the repository root, with no way to point it at a subdirectory, and
spar's root is not a pi package. Clone the repo and install `pi/` as a local path.

## What it shows

### Footer

A status line, for example `spar | 1 running | 3 need you | paused codex | $12.40 today`:

- **running**: runs whose orchestrator holds a seat (`holds_seat`).
- **need you**: runs waiting on the operator (`needs_human`).
- **paused**: providers whose `quota_status` is not `available` in `spar provider list --json`.
- **today**: summed `cost_usd` from `spar stats --since <minutes since local midnight>m --json`.
  That counts the whole cost of every run that ended or changed today, not only what was
  spent today.

It starts in `session_start` and is stopped in `session_shutdown`. A long-lived
`spar events --follow --json` child (O146) drives it: each event triggers a debounced re-read
of `spar status --json` and `spar stats`. If that child exits, it is restarted with
`--since <last cursor>` after a backoff of 2 s doubling to 60 s, and each restart also
re-reads status, so the footer keeps updating (about once a minute) even on a `spar` that
has no `events` command yet. Provider pauses are not events, so `spar provider list --json`
is polled every 60 s.

Outside a spar project (`spar stats` fails), or when `spar` is not installed, the footer
shows nothing and logs nothing. It runs only in modes with a UI (interactive and RPC).

### `/spar`

The run list, as an overlay: id, phase, round, age since the last change, a `NEEDS YOU` or
`running` flag, cost, workflow and the first line of the task. Runs needing you come
first, then running ones, then the rest by most recent change. A cost ending in `+` is a
floor: some dispatches reported no cost. Enter opens the run's detail.

Outside a project, `spar status --json` lists runs from every registered project, and so
does `/spar`.

### `/spar <id>`

The run detail, as a scrollable overlay (up/down, pgup/pgdn, `b` back to the list, esc to
close):

- phase, round of max rounds, the needs-you or running flag, task and error;
- cost, total and per provider (`spar status <id> --json`);
- seats: role, slot, provider and model, status, round, and liveness flags;
- open review findings from the last review panel, grouped critical, major, minor
  (`spar brief <id> --json`), with follow-up and possibly-addressed notes;
- the last 10 run events and the last 8 bus messages (`spar bus log --run <id> --json`);
- the exact next command, as `spar brief` computes it.

Run ids complete after `/spar `, runs needing you first.

Outside the interactive terminal (`pi -p "/spar"`, RPC, JSON mode) both commands print a
plain-text version instead: to stdout in print mode, as a notification over RPC, to stderr
in JSON mode.

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
- **`questionnaire.ts`, `notify.ts`, `todo.ts` examples**: their jobs (composing a run,
  alerting at a gate, branch-aware state) belong to layer 2, where they get a second look.

The scrollable detail view is about 40 lines written here. pi-tui's `ScrollView` needs
layout integration that an overlay component doesn't get.
