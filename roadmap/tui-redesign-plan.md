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

## Stage B — the palette + Driving mode (done)

- `:` opens a command palette (the composer is gone; focus is 2-wide, keys `1`/`2`);
  `q` becomes quit (double-`Ctrl+C` retired). Verbs: approve/reject/ship/confirm/
  reconcile/takeover/spawn/chat + implement/plan (reuse the run's recorded fleet), all
  with run-id completion. `/` filters the rail.
- The Shell tab promotes to a **full-screen Driving mode**: rail collapsed, structural
  signalling (green status banner + green pane border), one-line banner. Same escapes
  (`F12`, `C-a d`).
- Diff renders the selected slot's real `git diff HEAD` (capped), falling back to
  artifacts when a slot has no worktree. Decisions U3/U4.

## Stage C — the attention model (done)

- Status roll-up: a status-line `⚑N need you` count across every run, plus a `⚑`
  marker that rolls a wanting run up to its project row. Answers "what needs me?".
- Attention-sorted rail: gates and broken runs float to the top; selection stays glued
  to the run id as the order shifts.
- `a` = jump to the next run that needs you (also the tappable roll-up token); toasts
  when a run crosses into a gate / breakage.
- Width bands: `<80` Main only · `80–119` rail + Main · `>=120` extra width to Main
  (never a fourth box). Decision U5.

## Stage D — the chrome rebuild (done)

Stages A–C fixed *where things are*; the shell still read like a 2015 curses app.
Boxes were the problem: rail and Main each drew a full border, so the middle of the
screen was a two-column wall (`┐┌`), the tab strip lived inside Main's top border
mixed with slot identity and mode flags, and a painted `rgb(12,14,18)` slab fought
every terminal theme it landed in. The 24-column fixed rail elided the one field that
identifies an agent: two rows both read `✓ review-… done`.

- **Bands, not boxes.** Header · stepper · labels · rule · body · footer, with one
  1-column seam between rail and Main. The rule under the labels row doubles as the
  active tab's underline, so tab indication costs no row. Bands fold in order of what
  the operator can most afford to lose: the stepper below 14 rows, then labels+rule
  below 9, leaving header + body + footer at the 20x5 floor.
- **The run is a stepper.** `plan ─ critique ─ spec ─ build ─ tests ─ review ─ ship`,
  read off the slots that actually ran (they accumulate on the run) rather than
  guessed from the phase name, with `⚑` on the step a gate is holding. Degrades in
  three tiers before it clips.
- **The terminal owns the background.** No page fill; only chips, the gate/alert
  washes and overlays paint one. Tokens live in `src/theme.rs`.
- **Identity over ids.** The rail shows an agent's *role* (`review 0`, `builder`) and
  its model, never `review-0-cli-opencode`; the log drops the provider's opaque
  `toolu_…` ids, ~30 columns of noise per result row.
- **Reserved space.** Gate buttons live in a fixed 23-column zone (80 columns and up)
  and are left-aligned inside it, so swapping gates cannot slide a button out from
  under a click; the Activity tab's alert badge has a reserved 4-column slot for the
  same reason; the rail width comes from the terminal width alone, never from the data.
  Two of U11's four shift sources remain: the narrow tab strip still flips mode at the
  80-column breakpoint, and the attention re-sort needs the motion engine (006 A/B).
- Asserted by `mod render_stability`: a swept render over widths 1-200 x heights 1-60
  plus every band breakpoint, the projects level, an empty project list, 400 runs, and
  "this region does not move when that value changes." Decision U14.

## Sources

- k9s — resource list + one main view, `:` palette, drill-down with `Enter`/`Esc`.
- lazygit — numbered side panels (`1`-`5`), one main area, `+`/`_` zoom.
- lazydocker — same shape; tabs over the main pane.
- herdr — agent-fleet TUI: rail of agents, one detail area, per-agent log/diff tabs.
- claude-squad — session list + one pane hosting the agent's terminal as a *mode*.
- zellij — locked mode: full passthrough with a single advertised escape.
- Raskin, *The Humane Interface* — visible modes, reliable exits.
- Nielsen Norman Group — progressive disclosure.
