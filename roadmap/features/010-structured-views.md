---
id: 10
title: Structured views
status: backlog
milestone: 6
effort: L
priority: high
dependencies: [5]
---

# 010: Structured views

## Summary

Replaces the one string-based log viewer that every Main tab currently renders through
with record-oriented views: fields in columns, timestamps, folding, and navigation that
moves by structure rather than by line. This is the fix for "I can't see where data lives,
it's walls of text, and there's no easy way to move around" — a legibility problem, not a
palette one.

## Problem

Every tab in Main except Shell renders the same way: build one `String`, hand it to
`render_scrollable_log`. `draw_activity_body` literally does `activity.join("\n")` and
passes it to the Log tab's renderer. The Diff tab does the same. Nothing in Main is a table,
a record list, or anything but text in a scroller.

Worse, structure that *does* arrive is destroyed on the way to the screen.
`compact_log_line` runs `collapse_ws`, so column-aligned input renders as prose. Feeding the
Activity tab three deliberately aligned rows produces:

```
19:04 impl done 4.2M tokens
19:11 suite pass
19:12 review-0 request_changes
```

The alignment was in the input. The renderer squashed it.

Measured against a realistic agent stream at 120x30, the Log tab shows:

- **No timestamps at all.** For a product whose whole value is watching work that runs for
  hours, nothing on screen says when a step happened or how long it took.
- **No field boundaries.** `▸ Bash cargo test --bin spar render_stability > /tmp/x.log 2>&1`
  puts the tool name, its argument and its redirection in one run of text at one weight,
  clipped at the right edge with no indication that it was clipped.
- **Prose indistinguishable from tool calls.** The agent's reasoning sits at the same indent
  and weight as its actions, so there is no visual rhythm to skim.
- **Paths at full length, repeatedly.** `/home/sholom/projects/spar-spar-abc/src/tui.rs`
  costs ~50 of 87 available columns and appears twice in ten lines. `shorten_paths` exists
  and is not doing enough.
- **The right third is dead** even with content, because extra width buys nothing: lines are
  short and left-aligned, so a wider terminal adds emptiness rather than information.

## Goals

- **Records, not lines.** Each tab renders typed records with real fields. The Log tab's
  record is (time, direction, tool, argument, result); Activity's is (time, actor, event,
  detail); the criteria grid from feature 005 is already the right shape and should be the
  model for the rest.
- **Columns that hold.** Fields occupy fixed columns that do not move as content changes —
  the same reserve-from-the-layout rule U11 applies to chrome, applied to content.
- **Time is always present.** Every record carries its timestamp, and elapsed time where it
  is meaningful (how long a tool call took, how long a phase has run).
- **Prose is visually distinct from actions**, so a screen can be skimmed for what happened
  without reading it.
- **Navigation by structure**: jump to the next tool call, the next error, the next phase
  boundary; fold a tool call's output; filter to one slot. Today the only movement is
  line-by-line scrolling through everything.
- **Width earns something.** Wider terminals reveal more fields, not more whitespace.

## Non-goals

- No change to the rail + main-area shape (X2, U1) or to the noun set (U6). This is about
  what fills Main, not what Main is.
- No motion work (feature 006).
- Not a palette change. Colour may help mark record kinds, but recolouring the current
  string blob would not address any symptom above.

## Note on U14

U14's chrome rebuild replaced borders with bands and rules and introduced the token set.
That was real, and none of it is being undone. But it changed the *frame* and left the
*contents* as undifferentiated text, which is why the TUI still reads as walls of text after
it. This feature is the other half of that work.

## Phases

### Phase A: a record type and one columnar view

Introduce the record representation and convert the Activity tab first — it is the smallest
surface, its data is already structured before `collapse_ws` destroys it, and it proves the
widget.

### Phase B: the Log tab as records

Parse the provider stream into (time, direction, tool, argument, result) rather than lines.
Distinguish prose from tool calls. Fold long results. Keep the raw text reachable, since a
parse that hides something the operator needs is worse than the wall of text.

### Phase C: navigation by structure

Jump between tool calls, errors and phase boundaries; fold and unfold; filter to a slot.

### Phase D: width and path treatment

Reveal additional fields as width allows, and shorten paths against the run's own worktree
root so a repeated project path stops costing half the line.
