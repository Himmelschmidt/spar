---
id: 6
title: Motion and visual identity
status: backlog
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

Depends on feature 005: animating a confusing layout gives you a confusing layout at 60fps.

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

### Phase A: time-based motion engine

`Instant`-based motion module with easing curves and `Tween<T>`; frame clock ramps to
~16ms while animating, idles low otherwise. Replaces the tick-modulo spinner and cursor
blink (`src/tui.rs:3746`).

### Phase B: reserved-space layout widgets

Fixed-slot layout primitives and skeleton placeholders that fix the gate-button
(`src/tui.rs:2740-2745`), attention-resort, list-growth and tab-breakpoint shift sources.

### Phase C: design tokens and density pass

Token system replacing scattered color consts; consistent border and weight language;
braille/half-block density pass.

### Phase D: `TestBackend` snapshot assertion suite

Snapshot tests covering 20x5 rendering, empty project list, 400 runs, and layout-stability
assertions per region.
