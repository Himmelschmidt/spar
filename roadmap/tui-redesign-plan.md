# TUI redesign — rail + one main area

Staged rebuild of the product shell. Decisions: `DECISIONS.md` U1 / U2.

## Why

The old shell had **six co-equal Tab-cycled focus targets** (`Runs / Agents / Log /
Activity / Terminal / Composer`) over a 3-column body, and the Terminal panel
*replaced a different region* of the layout when it took focus. Every Tab press
therefore mutated the screen's spatial map, which is exactly what destroys location
memory: you can no longer point at where a thing lives, only cycle until it appears.

Chrome ate the screen before content did: 3 header rows + 2 action rows + 4 composer
rows + 1 footer = **10 rows**, i.e. 40% of an 80×24 terminal.

Two independent studies of comparable tools converged on the same shape. **k9s**,
**lazygit**, **lazydocker**, **herdr** and **claude-squad** all ship a **rail
(drill-down list) + ONE main area whose content is a function of (selection × tab)**,
with `Enter` to push, `Esc` to pop, a breadcrumb, and direct number keys for panels.
None of them ships an N-way focus ring.

Supporting principles:

- **Raskin, *The Humane Interface*** — modes are fine when they are *visible* and have
  a *reliable* exit. The old Terminal panel was a hidden mode with an unclear exit;
  the Shell tab is a labelled one with `F12` / `C-a d`.
- **zellij locked mode** — a full-keyboard-passthrough surface needs exactly one
  escape key, advertised on screen. We use `F12` + the `C-a` prefix (never `Esc` or
  `Tab`: the agent needs `Esc`, and Shift+Tab is Claude Code's permission toggle).
- **NN/g progressive disclosure** — show the run list first, the run's agents on
  demand, the agent's log/diff on demand. A drill-down rail *is* progressive
  disclosure; six co-visible panels is the opposite.

## Stage A — the spine (this PR)

- `Focus` 6 → 3: `Rail | Main | Composer`, direct keys `1` / `2` / `3` (Tab still
  cycles the three).
- `BrowseLevel` becomes a 3-level drill-down: `Projects ▸ Runs ▸ Agents`. `Enter`
  pushes, `Esc` pops (never exits at the root). The old Agents/fleet panel is gone —
  it is the rail's deepest level.
- `MainTab` = `Log | Activity | Diff | Shell`, switched with `[` / `]` or a click on
  the tab strip painted into Main's top border. Activity carries the unread-alert
  badge. `+` / `_` zoom Main in place (rail hidden).
- Chrome: one status line with a breadcrumb
  (`spar · acme/api ▸ run 3f2a ▸ impl#2 · implement (2/3) · ⚠2 · ABANDONED`), gate cues
  and tappable gate buttons; a 1-row contextual footer; a 3-row composer.
- The embedded terminal keeps every capability (PTY passthrough, `C-a` prefix,
  bracketed paste, tmux mouse, agent takeover) and simply lives in the Shell tab.
  Takeover = `Enter` on a slot → attach + switch Main to Shell.
- Narrow (<90 cols): no rail, Main only, the same `MainTab` strip on its own tappable
  row — a tap on it is the escape from the Shell tab on a phone.

## Stage B — the palette + Driving mode

- `:` opens a command palette (the composer collapses into it); `q` becomes quit
  (double-`Ctrl+C` retires).
- The Shell tab promotes to a **full-screen Driving mode**: no rail, no chrome but a
  one-line mode banner. Same escapes (`F12`, `C-a d`).
- Diff renders the worktree diff for real (not just artifacts).

## Stage C — the attention model

- Status roll-up: one line that answers "what needs me?" across every run.
- Attention-sorted rail (gates and stalls float to the top).
- `a` = jump to the next alert; toasts for gate transitions.
- Width breakpoints beyond the single narrow/wide split.

## Sources

- k9s — resource list + one main view, `:` palette, drill-down with `Enter`/`Esc`.
- lazygit — numbered side panels (`1`-`5`), one main area, `+`/`_` zoom.
- lazydocker — same shape; tabs over the main pane.
- herdr — agent-fleet TUI: rail of agents, one detail area, per-agent log/diff tabs.
- claude-squad — session list + one pane hosting the agent's terminal as a *mode*.
- zellij — locked mode: full passthrough with a single advertised escape.
- Raskin, *The Humane Interface* — visible modes, reliable exits.
- Nielsen Norman Group — progressive disclosure.
