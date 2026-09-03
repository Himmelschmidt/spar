---
id: 8
title: Orchestrator conversation
status: backlog
milestone: 6
effort: L
priority: high
dependencies: [4, 5]
---

# 008: Orchestrator conversation

## Summary

Moves the disposable briefing session inside spar. The TUI gains a resident orchestrator
you talk to: it interviews you into a brief, proposes a fleet and launches the run, and
later argues a gate with you against the evidence 005 renders. Decisions: `DECISIONS.md`
U16, U17, settling X10 and superseding U8's brief field.

Depends on feature 004 for a Home to launch from, and on feature 005 because a
conversation about a gate is only as good as the criteria grid behind it.

## Problem

`docs/architecture-operator-model.md` already assigns this job to something: "The session's
job ends at launch. It converts a conversation into a written brief, launches a detached
run, and is free to die." That session is an outer coding CLI the operator has to open
separately, brief by hand, and keep in sync. Every run therefore costs two conversations:
one with the agent that writes the brief, and one with spar.

The pieces to close that gap already exist and are not wired together. The core skill is
embedded in the binary (`src/skills.rs`) and is exactly the document an outer agent reads
to drive spar. Every slot already carries `SPAR_PROJECT_ROOT` / `SPAR_RUN_ID` /
`SPAR_AGENT_ID` (`providers/presence.rs`). The CLI's exit codes are already a public
contract. `bus::chat` and `is_human_alert` already route `@human`-addressed messages, X4
already settles turn-boundary delivery, and W5 already makes a bare spawned agent
addressable by id. What is missing is a conversation *surface* that belongs to spar rather
than a tmux pane passthrough: today `:spawn` plus a Shell takeover gives you a raw coding
CLI in a box, which knows nothing about spar and cannot launch anything.

X10 held this open on the grounds that the target flow does not require a conversation.
That is still true, and the fleet picker and the CLI stay as paths that cost no tokens.
It is no longer the question being asked.

## Goals

- A conversation surface as a fifth Main tab, content `f(rail selection)` like every other
  tab: a selected run scopes the conversation to that run, Home scopes it to a new one.
  No fifth noun and no new focus target — the orchestrator is an Agent under U6.
- A turn model owned by the backend, not the workflow (U17). `native-cli` ships first:
  dispatch headless with the transcript in the prompt, address the question to `@human`,
  exit. `api-sdk` is the better long-term host and does not block this.
- Intake: interview, write `.spar/briefs/<slug>.md` (the path O41 already defines), propose
  a fleet, launch. Replaces U8's brief field; U8's picker survives as the manual path.
- Gate consultation and status, grounded in the documents and the `AC-n` grid feature 005
  renders. The orchestrator reads that evidence; it does not replace it.
- The orchestrator proposes and the operator disposes (U17). It never approves a plan,
  confirms ship, merges, sweeps `cleanup`, or picks a fleet on its own.

## Non-goals

- No supervision. The per-project daemon (feature 003) is a different actor with a
  different lifetime; a conversation is not a monitor.
- No new execution machinery for the native-cli turn model. A turn is a dispatch.
- No conversation transcript as a run artifact of record. `.spar/runs/<id>/` stays the
  system of record (P7); a transcript is a convenience, and losing one loses nothing.

## Phases

### Phase A: conversation surface

The fifth Main tab: transcript view, input line, scoping from the rail selection, and the
scroll/follow behaviour the Log tab already has. Rename the `:chat` palette verb (a raw bus
message) off the collision.

### Phase B: native-cli turn loop

Dispatch-per-turn against the embedded core skill as the prompt, with the transcript
appended and the agent's question addressed to `@human` over the bus. Transcript
persistence, turn-boundary delivery via X4, and the token accounting every other slot
already reports.

### Phase C: intake

Interview to `.spar/briefs/<slug>.md`, fleet proposal, and launch through the same CLI
contract an outer agent drives. Wires into 004 Phase D's new-run surface.

### Phase D: gate consultation and status

Scope the conversation to a selected run: read that run's plan, critique, contract, `AC-n`
grid and reviewer verdicts, and argue the gate either way. The commit stays the operator's,
through the gate buttons and palette verbs that already exist.
