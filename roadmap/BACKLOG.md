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
