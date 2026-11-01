# spar-pi

spar's front end as a [pi](https://pi.dev) package (DECISIONS.md O145, roadmap feature
012). Layer 1 is read-only visibility for the operator; layer 3 is model-callable tools and
a skill, so the pi session can drive spar the way an outer agent does (O147).

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

Once initialized, a long-lived `spar events --follow --json` child (O146) is started
before the first status read, so nothing that happens in between is missed. Each event
triggers a debounced re-read of `spar status --json` and `spar stats`; `archived` and
`abandoned` events also update the counts at once (an archived run leaves them, an
abandoned one stops counting as running). If the child exits it is restarted with
`--since <last cursor>` after a backoff of 2 s doubling to 60 s. Status is also re-read
every 60 s regardless, so a dead orchestrator and the midnight rollover of "today" show
up without an event. Provider pauses are not events, and `spar provider list --json`
probes every CLI, so it is polled every 5 minutes.

The events child is killed on `session_shutdown` and, as a backstop, on pi's process
`exit`. Its stdin is a pipe, so it would also die with a SIGKILLed pi if `spar events`
exited on stdin EOF; it does not yet. It runs only in modes with a UI (interactive and
RPC).

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

## Tools for the model

Each tool runs `spar <cmd> --json` in the session's directory with an argv array (no shell;
free text goes as `--flag=value`, and run and slot ids that could read as flags are
refused). The model gets concise text; the transcript renders the same data from the
result's `details`: runs as a table, findings grouped by severity, events and bus messages
one per line, long tables cut at the edge rather than wrapped.

| Tool | spar command | Notes |
|---|---|---|
| `spar_status` | `status [<id>] [--all] --json` | list (50 most relevant) or one run with seats and projected seats |
| `spar_brief` | `brief <id> --json` | findings by severity, recent events, the exact next command |
| `spar_events` | `events --run <id> --json` | backlog only, last 30 (at most 200) |
| `spar_logs` | `logs <id> [slot]` | last 80 lines (at most 400), long lines cut |
| `spar_providers` | `provider list --json [--usage]` | |
| `spar_stats` | `stats --json [--since] [--by]` | |
| `spar_plan` | `plan --task=... --detach --json` | confirmed launch |
| `spar_implement` | `implement [--run=<id>] [--task=...] --detach --json` | confirmed launch |
| `spar_review` | `run --workflow=review --task=... --detach --json` | confirmed launch |
| `spar_wait` | `wait <id> --timeout=<n>m --json` | 10 minutes by default, 120 at most; abort kills it |
| `spar_bus_send` | `bus send --message=... --to=... [--run] --json` | no `--persist` |
| `spar_bus_log` | `bus log [--run] --json` | last 20 |
| `spar_skill` | `skills get core` | section list, or one section by heading |

Launches take `providers`, `roles`, `backups`, `effort`, `fleet`, `without`, `base` and
`dry_run`, mapped to the flags of the same names.

Exit codes are reported, not hidden: 2 (a human gate), 3 (stuck) and 4 (no usable
providers) are results with the meaning spelled out and `next_command` from `spar brief`
attached, not tool failures. A wait that runs out is reported as "still working". A
refused launch, a declined one, a missing binary or a failing read is a tool error.

### What the model cannot do

Approve, reject, ship (including `--override-gate`), confirm, reconcile, round extensions,
`rule add`, `cleanup`, `--reassign`, `amend` and `bus send --persist` are not tools, with or
without a dialog. They are the operator's: a confirm dialog the model can raise whenever it
likes teaches the operator to click yes. When a run waits on one, the tools say so and hand
over the command.

### Launch confirmation

`spar_plan`, `spar_implement` and `spar_review` spend money, so each one:

1. runs the same argv with `--validate-only`; a refusal is returned to the model and the
   operator is never asked;
2. asks the operator with `ctx.ui.confirm`: task, providers, roles, backups, effort, fleet
   preset, `--without`, base, the full command, and for `implement --run` the run's own
   seats; for a new run, seats the flags do not pin come from `spar.toml`, which the
   dialog says (`--validate-only` does not report a fleet yet);
3. fails closed when there is no dialog-capable UI (print and JSON mode);
4. launches detached and returns the run id, phase, exit meaning, the resolved fleet and
   the next command.

To let the model launch without asking, for one session only:

```bash
pi -e /path/to/spar/pi --spar-unattended-launch
# or
SPAR_PI_UNATTENDED_LAUNCH=1 pi -e /path/to/spar/pi
```

### Skill

`skills/spar/SKILL.md` tells the model when to reach for spar, that gates are the
operator's, and to read the rest of the CLI from `spar_skill`, which serves
`spar skills get core` from the installed binary so the skill cannot drift from it.

## Develop

```bash
cd pi
npm install
npm run typecheck
npm test
```

End to end, in real pi with a scripted model that bills nothing
(`test/e2e/scripted-provider.ts`, built on pi-ai's faux provider; `rpc-confirm.ts` plays
the operator over RPC):

```bash
test/e2e/run.sh /path/to/a/spar/project /path/to/new/scratch-dir
```

It runs only read-only spar commands in the project, and launches only in a scratch git
repo it creates and deletes, under `SPAR_DRY_RUN=1`. `SPAR_PI_BIN` picks the spar binary.

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
