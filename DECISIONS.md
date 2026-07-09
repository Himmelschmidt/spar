# DECISIONS

Project-level product and architecture decisions. Status: `OPEN` | `LEANING` | `DECIDED`.

## Product

| ID | Decision | Status |
|----|----------|--------|
| P0 | Product name is **spar** (binary `spar`) | DECIDED |
| P1 | spar is a **first-class agent product** (TUI-first), not a Pi/Claude plugin | DECIDED |
| P2 | Default human surface is a **nice fleet TUI** (Claude/Grok/agy class); CLI/JSON for outer agents | DECIDED |
| P3 | Discovery via **built-in skills + AGENTS.md** (agent-browser pattern) | DECIDED |
| P4 | Dual execution: **native-cli** + **api-sdk**, one orchestrator | DECIDED |
| P5 | First-class **swarm bus** (A2A), not dumb mailbox only | DECIDED |

## Orchestration

| ID | Decision | Status |
|----|----------|--------|
| O1 | **One run id** plan → implement → ship | DECIDED |
| O2 | Always **worktrees** for coding slots | DECIDED |
| O3 | Cleanup **auto when fully done**, fail-closed if still needed | DECIDED |
| O4 | bwrap **per-provider optional**; none enabled by default yet | DECIDED |
| O5 | Default implement: **1 impl + ≥2 reviewers**; multi-impl in **arena** | DECIDED |
| O6 | Arena finish: **winner** or **reconcile** then review | DECIDED |
| O7 | Roles + peer are **v1 first-class** | DECIDED |
| O8 | Plan: multi-provider + critic; **structured/big** for large features | DECIDED |
| O9 | Gates **configurable**; lean high autonomy | DECIDED |
| O10 | Ship: **draft PR**, never merge; force-with-lease only swarm branches | DECIDED |
| O11 | Stream **everything** (logs/events) | DECIDED |
| O12 | Headless default; tmux **namespaced opt-in only** | DECIDED |
| O13 | Quota: **best-effort auto** + manual; Claude rate_limits-shaped signals | DECIDED |
| O14 | **Suite channel** (implement/loop): dedicated cheap `tester` slot runs full suites; impl/review smoke/diff-only when suite ran; long `suite.timeout_secs`; salvage partial review/suite artifacts on timeout; fail closed if enabled but no tester/provider. Independent `review` workflow may still run its own tests (no suite slot by default) | DECIDED |

## Open

| ID | Topic | Status |
|----|-------|--------|
| X1 | First API provider to spike (xAI / Anthropic / OpenAI) | DECIDED — OpenAI-compatible (`api:openai`, `api:xai`) via ureq |
| X2 | TUI keymap / layout (mimic which product most?) | LEANING — j/k Tab a/r/s /commands (M1 shell) |
| X3 | Project template overrides day one vs later | OPEN |
| X4 | Bus steer reliability for native-cli headless | OPEN — inbox + best-effort; full inject later |
