# spar monitor for herdr

A [herdr](https://herdr.dev) plugin that makes herdr a standalone monitor for spar runs.
Each spar run that matters gets its own herdr pane, reported to herdr as an agent, so
herdr's sidebar, rollups and notifications tell you which run needs you. A popup clears
human gates; another action opens the seat logs.

It is optional and independent of the pi front end (`../pi`): both are thin clients over
`spar ... --json`, and neither needs the other. The plugin only reports state to herdr
and shells out to spar; it never reads or writes `.spar/` itself.

Requires herdr 0.9.5 or later, node 22.18 or later (TypeScript runs directly, no build
step, no npm dependencies at runtime) and a spar new enough to have `spar events`.

## Install

From a checkout:

```bash
herdr plugin link /path/to/spar/herdr
```

From GitHub:

```bash
herdr plugin install Himmelschmidt/spar/herdr
```

The startup hook runs at the next herdr server start. To start now without restarting,
run the `spar: sync runs` and `spar: open board` actions (see below).

## Config

Optional. `config.json` in the plugin's config directory
(`herdr plugin config-dir spar.monitor`, usually
`~/.config/herdr/plugins/config/spar.monitor/config.json`):

```json
{
  "spar_bin": "/home/me/.cargo/bin/spar",
  "projects": ["/home/me/projects/app", "other-project"],
  "recent_hours": 24,
  "board_on_startup": true,
  "sync_interval_secs": 60
}
```

| Key | Default | Meaning |
|---|---|---|
| `spar_bin` | `spar` | The spar binary. The `SPAR_BIN` environment variable overrides it. |
| `projects` | every project | Allow-list of project roots or project names (as `spar status --all --json` reports them). |
| `recent_hours` | `24` | How long a run that no longer needs you or holds a seat keeps a pane. |
| `board_on_startup` | `true` | Open the board pane from the startup hook. |
| `sync_interval_secs` | `60` | How often the board re-syncs when no event arrives. |

## What it does

**Run panes.** A run is relevant while it needs you (spar's `needs_human`, not driven by
anyone, and moved in the last 7 days), holds a seat, or moved within `recent_hours`. Each
relevant run gets one pane in a `spar` tab of a workspace for its project. The workspace
is one already open on the project root if there is one (any pane whose cwd is the root),
otherwise a new one labelled with the project name. The pane follows
`spar events --follow --run <id>` and re-reads `spar status <id> --json` and
`spar brief <id> --json` on every event and every 30 s. It shows phase, round, gate and the
verbs that apply, seats with status first, open findings, recent events and the next
command, wrapped to the pane width. It reports to herdr:

| Run | herdr state |
|---|---|
| needs you and not stale | `blocked` |
| holds a seat | `working` |
| anything else | `idle` (herdr shows `done` until you look at it) |

plus metadata: display name `spar <id> <workflow>` and the phase as the `summary` token.
The pane exits, and herdr closes it, when the run is archived or gone. spar errors are
shown in the pane and retried with backoff from 2 s to 60 s.

**Sync.** `spar: sync runs` brings herdr in line with `spar status --all --json`: one pane
per relevant run, never two (the mapping lives in the plugin state directory, and panes
are checked against `herdr api snapshot`; a pane carrying a run's tokens is adopted). A run
pane whose run stopped mattering is closed once it is seen (herdr `idle`, not the unseen
`done`) and not focused.

**Board.** herdr startup hooks are one-shot, so something long-lived has to notice new
runs. The board pane (a `spar board` tab) follows `spar events --follow` in every project
spar knows about and re-syncs on every phase, gate, abandonment or archive event, and on a
timer for projects that appear later. It also lists the relevant runs, needing you first.
Without the board, new runs only appear when you run the sync action. The startup hook
opens it unless `board_on_startup` is false.

**Restarts.** herdr restores panes after a server restart as plain shells under the same
id and label. The startup hook closes this plugin's restored panes (titled `spar run`,
`spar board`, `spar seat log`, without the tokens their processes report) and reopens
the run panes and the board.

## Actions

| Action | Context | What it does |
|---|---|---|
| `spar.monitor.sync` (`spar: sync runs`) | any | One sync now. |
| `spar.monitor.board` (`spar: open board`) | any | Opens the board if it is not open. |
| `spar.monitor.act` (`spar: act on run`) | pane | A popup for the run in the focused pane: what the gate is about (findings, arena candidates, artifacts, with the full brief on `b`), a numbered menu of only the verbs valid for the run's phase, then the exact command and its effect, and it runs only after you type `yes`. Ship runs `spar ship <id> --confirm`; your typed yes is the confirmation. Ship, override, reconcile and cleanup run detached, so closing the popup does not stop them. From a pane that is not a run pane, it lists the runs that need you. |
| `spar.monitor.logs` (`spar: seat logs`) | pane | A `logs <id>` tab with one pane per seat of the current round (at most 6), each following `spar logs <id> <seat> --follow`. |

The verbs per phase mirror `pi/extensions/spar/gates.ts`. spar is the authority: it
refuses anything the menu gets wrong, and the popup shows the refusal.

## Keybindings

Add to `~/.config/herdr/config.toml`, then `herdr server reload-config`. These keys are
unbound in herdr's defaults (`herdr --default-config`):

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "spar.monitor.act"
description = "spar: act on run"

[[keys.command]]
key = "prefix+shift+l"
type = "plugin_action"
command = "spar.monitor.logs"
description = "spar: seat logs"

[[keys.command]]
key = "prefix+shift+s"
type = "plugin_action"
command = "spar.monitor.sync"
description = "spar: sync runs"
```

## Not in v1

Runs on other hosts (`spar status --all-hosts`). herdr's own saved SSH machines cover
remote boxes: install the plugin on the remote host and add the machine to herdr.

## Development

```bash
npm install
npm run typecheck
npm test
```

Tests use a fake `spar` (`test/fake-spar.cjs`) and a fake `herdr` (`test/fake-herdr.cjs`)
that records argv and keeps a small workspace, tab and pane world, so they need neither a
herdr server nor real runs.
