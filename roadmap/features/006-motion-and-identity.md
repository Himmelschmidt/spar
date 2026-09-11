---
id: 6
title: Motion and visual identity
status: done
milestone: 6
effort: M
priority: medium
dependencies: [5]
---

# 006: Motion and visual identity

## Summary

Replaces tick-modulo animation with time-based motion, fixes the four layout-shift sources
by reserving space from the layout instead of the content, and adds a design-token system
plus a `TestBackend` snapshot harness so stability is asserted rather than eyeballed.
Decisions: `DECISIONS.md` U10, U11, U12.

Phases C and D **landed first**, ahead of features 004 and 005 (decision U14). The
dependency argument — "animating a confusing layout gives you a confusing layout at
60fps" — holds for motion (phases A and B), not for chrome and tokens: Home and the
criteria grid would otherwise have been built into the old bordered shell and then
rebuilt. Phases A and B still depend on feature 005, which is what the `dependencies`
field above refers to.

## Problem

`FRAME = Duration::from_millis(100)` reads as a slideshow (10fps), even though
`app.animated = animating(&app, &snap)` already exists to gate frame-rate ramping.
Animation itself is tick-modulo, not time-based: the spinner is `SPINNER[tick % len]` and
the cursor blink is `(app.tick / 6).is_multiple_of(2)` (`src/tui.rs:3746`), so both change
speed the moment the frame rate varies. Layout shifts from four sources: gate buttons
right-align from summed label widths (`src/tui.rs:2740-2745`), the rail re-sorts by
attention as data arrives, lists grow as runs and slots appear, and the narrow-tab strip
flips mode at a width breakpoint. And the palette already reads as GitHub-dark
(`ACCENT` rgb 88/166/255) without a token system behind it, one accent and one alert colour
scattered across consts rather than held to strictly.

## Goals

- Time-based motion module: `Instant`-driven, easing curves, `Tween<T>`, enter/exit
  transitions, replacing every tick-modulo animation.
- Frame clock ramps to ~16ms (~60fps) while `app.animated` is set, idles low otherwise.
- Layout reserves space from the layout, never the content: fixed slots, skeleton
  placeholders, animated reorder transitions. Fixes all four shift sources at once.
- Design token system: one accent, one alert colour, consistent border and weight language,
  braille/half-block density where it earns its place.
- `TestBackend` snapshot harness asserting render-without-panic at 20x5, an empty project
  list, 400 runs, and "this region does not move when that value changes."

## Non-goals

- No new information architecture (that is 004) or gate content (that is 005); this feature
  only changes how existing content moves and renders.

## Phases

### Phase A: time-based motion engine — DONE

`src/motion.rs`: one `Clock` (an `Instant` origin on `App`), `cycle`, `breathe`,
`sweep`, `frame`. The frame clock ramps between `FRAME_ANIMATING` (16ms) and
`FRAME_IDLE` (250ms); the spinner and the palette cursor blink both moved off
tick-modulo. Frames are wrapped in DECSET 2026 so a wide repaint at 60fps cannot
tear, and DECSET 1004 gates `animating()` so an unfocused window costs nothing.

The motion *vocabulary* landed with it, measured off grok and muse frame by frame
(U30): a breathing gutter for work in flight, a travelling sweep for work
dispatched but not yet producing, ranked under the attention flag.

`Tween` and its easing curves are deliberately **not** here. They have no caller
until phase B's reorder transitions, and a helper with no caller is a guess about
its caller.

Landed alongside: spar's own page and retuned palette (U29), and the Home chrome
de-duplication and detail pane (U31), which are not in this feature's original
scope but were the visible defects blocking any judgement of the motion work.

### Phase B: reserved-space layout widgets — DONE

Fixed-slot layout primitives and skeleton placeholders that fix the gate-button
(`src/tui.rs:2740-2745`), attention-resort, list-growth and tab-breakpoint shift sources.
Brings `Tween` and the easing curves with it. `Tween` is `Tween<T>` with `f32` operations only (U40); `HomeRow::Skeleton` reserves rows under the three scan-backed bands while the first cross-project scan is in flight; `gate_zone` is now affordable from 35 columns (`GATE_ZONE_W`+12) instead of 80; the context band's `METER_ZONE_W`/`STEPPER_MIN_W` makes the stepper width a pure function of layout; the rail re-sorts by adjacent-swap travel over `REORDER_PERIOD` (U39) and the tab strip glides over `STRIP_PERIOD` with per-tab `Rect` interpolation (U41). All four U11 shift sources are now closed.

Now queued behind **010** rather than 005: 010 rebuilds what Main renders, and
animating a reorder inside a string log viewer would be written twice.

### Phase C: design tokens and density pass — DONE

Landed as the chrome rebuild. `src/theme.rs` holds the token set (one accent, one alert,
three text weights, `INK` for chip text); the TUI paints no page background, so it
composites onto the host terminal theme. Pane borders are replaced by chrome bands over
one seam, the active tab is an accent underline on the band rule, the rail carries roles
instead of provider-suffixed slot ids, the run stepper is new, and the log drops the
provider's opaque tool-call ids. Recorded as U14, which also records the U1 and U5 rows
it amends.

Not done here: U12's "braille/half-block density where it earns its place" — nothing in
this pass earned it, so no density glyph was added. Of U11's four shift sources, the
gate buttons and the alert badge are now reserved from the layout and lists no longer
grow into their neighbours; the attention re-sort waits on phase A's motion engine and
the narrow tab strip still flips mode at the 80-column breakpoint.

### Phase D: `TestBackend` snapshot assertion suite — DONE

`mod render_stability` in `src/tui.rs`: a swept render over widths 1-200 x heights 1-60
plus each band breakpoint, with and without a run; the projects level; an empty project
list; 400 runs; and the layout-stability assertions (every tab holds its column across
tab switches and alert-badge counts 0-99; gate buttons start at a fixed x across every
gate set, down to the 80-column breakpoint).
