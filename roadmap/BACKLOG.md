# Backlog

Unscheduled ideas, grouped by theme. Promote to `roadmap/features/NNN-*.md` when picked up.

## Remote / persistence architecture

- **Thin-client split (`spar --remote`)** — a local spar TUI talking to a remote spar
  orchestrator over spar's own protocol stream, instead of ssh-then-run-server-side.
  Would give attach/persistence without leaning on tmux, and native image paste falls out
  as a single message type (herdr's model). Big: it reopens the tmux-vs-own-protocol
  decision the workspace initiative settled (`DECISIONS.md` W1/W2). Tracked as **X7**;
  the additive local-companion image bridge (feature 002, decision W6) ships first and
  does not depend on this. Revisit after the workspace initiative lands.

## Run store hygiene

- **Widen `spar archive --all` to the ambiguous at-rest phases** — archiving (O36) ships
  with one predicate doing two jobs: `auto_archivable` (`done` / `plan_rejected`) gates
  both the automatic launch sweep *and* the explicit `--all`. Excluding gates from the
  automatic path is the point of the feature; excluding `failed` / `stuck` / `escalated` /
  `quota` / `stopped` from an *explicitly requested* `--all` was a silent narrowing of the
  scope this was approved under ("auto-archive on terminal phases past some age"). On
  payforge that leaves 27 of 76 rows archivable one id at a time, so the listing settles
  around 42 rather than somewhere tight.
  Preferred shape: keep the launch sweep on `auto_archivable`, and let explicit `--all`
  take anything `archivable_by_hand` (already exists, already excludes in-flight). Gates
  stay opt-in via an id or a flag. Touches `state.rs` + `main.rs`, so it must land *after*
  the cleanup-safety fix, which owns those files.
- **TUI has no archive affordance** — all three rail surfaces filter through
  `registry::list_visible_project_runs`, but there is no key to show archived runs, no
  archive/undo action and no hidden count, all of which the CLI has. On a TUI-first
  product the TUI can now hide runs it offers no way to recover.
