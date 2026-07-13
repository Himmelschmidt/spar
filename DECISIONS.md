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
| P6 | **Dynamic model select** (vals benchmarks + prefs/urgency) is a first-class product path alongside explicit `--providers` | DECIDED — see MS* |

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
| O15 | **Spec / test-author** (plan flow): after planner+critic write `plan.md`, a separate `TestAuthor` slot freezes acceptance tests **before** coding and before the plan gate. Not planner, not critic, not suite `tester`. **Artifact-first** (plan + critique); bus is audit trail (no live multi-turn with finished planner/critic). Outputs `artifacts/test-contract.md` + tests in worktree; implement **always overlays** author working tree into impl (plus best-effort branch merge; abort on conflict). Fail closed if author fails, contract missing, or apply fails when TestAuthor ran. Config `[spec]` (`enabled` default true, optional `provider`, `timeout_secs`). Human gate is plan + contract | DECIDED |

## Model select (vals-backed)

Opt-in path: resolve fleet slots from benchmark data + user profiles + per-run urgency, instead of only manual `--providers`. Explicit `--providers` remains valid and default-required until select is opted in.

| ID | Decision | Status |
|----|----------|--------|
| MS0 | Data source: **vals.ai** coding benches; abstract behind `BenchmarkSource` so an official API can replace HTML scrape later | DECIDED |
| MS1 | Per-model fields used: **accuracy**, **latency**, **cost_per_test** (from vals payload) | DECIDED |
| MS2 | Phase A primary bench: **SWE-bench Verified** only; later optional blend (Terminal-Bench, LiveCodeBench, Vibe Code, Vals Index) | DECIDED |
| MS3 | Selection is **opt-in**: keep `--providers` required unless `--select <profile>` and/or `[model_select]` enables auto | DECIDED |
| MS4 | Named **profiles** in config (`best` / `value` / `fast` or custom) with weights for quality / cost / speed + optional `min_accuracy` | DECIDED |
| MS5 | **Urgency** is a per-run multiplier on the chosen profile (high → speed↑ cost↓; low → cost↑), not a separate parallel system | DECIDED |
| MS6 | Score: normalize metrics in candidate set; `score = w_q·acc − w_c·cost − w_s·latency`; apply floors/allowlists before rank | DECIDED |
| MS7 | **CLI economics**: treat `cli:*` cost as **0** for scoring (flat sub); do not use vals $ against subscription CLIs | DECIDED |
| MS8 | Resolve to **assignable slots** (`ProviderRef` + optional model), not abstract model names only; filter by doctor-available backends | DECIDED |
| MS9 | Multi-slot fleets (plan/arena/review ≥2): prefer **provider-family diversity**, then next-best score | DECIDED |
| MS10 | Always write **`artifacts/model-select.json`** (candidates, weights, urgency, winners, why) into the run | DECIDED |
| MS11 | Cache: `~/.spar/cache/vals/…` + TTL; `spar model refresh`; stale cache usable within grace window; fail closed with clear error if no cache and fetch fails | DECIDED |
| MS12 | Role defaults in config (`planner`/`implementer`/`reviewer`/`tester`/`critic` → profile names); suite `tester` defaults toward **fast/value** | DECIDED |
| MS13 | CLI surface: `spar model list|pick|refresh`; doctor reports cache age | DECIDED |
| MS14 | Ship phases: **A** cache+parser+list/pick · **B** `--select` into plan/implement · **C** roles/urgency/diversity · **D** per-adapter CLI model flags + richer vals→spar map | DECIDED |

## Workspace + bus delivery

Workspace initiative (2026-07-13). See `roadmap/workspace-initiative-plan.md` for the staged build.

| ID | Decision | Status |
|----|----------|--------|
| W1 | spar owns its **own tmux server socket** (`tmux -L spar`); the user's personal tmux sessions are never touched | DECIDED |
| W2 | Live pane output via tmux **control mode** (`tmux -C` `%output` stream), not `capture-pane` polling | DECIDED |
| W3 | Embedded terminal rendering via `vt100::Parser` + `tui-term` as a new TUI focus target | DECIDED |
| W4 | "Own coding UI over direct API" **deferred** — use opencode as the harness for API-only providers for now | DECIDED (deferred) |
| W5 | Workspace bus keyed by `agent_id`; `run_id` demoted to an optional message tag; `SPAR_AGENT_ID` gives bare agents identity. Lands last | DECIDED |
| B1 | `@human` routing = always-on TUI sink (baseline, zero config) + opt-in generic `[notify]` command/webhook; **no hardcoded personal integrations** in source | DECIDED |
| W6 | **Image paste over SSH = local-companion bridge (option A).** A local `spar clip` reads the OS clipboard image (`arboard` / `wl-paste` / `pbpaste`), ships raw bytes to the remote spar over the existing SSH connection (ControlMaster-exec or a forwarded socket); the remote stages a `0600` temp file and injects its **path** into the agent pane via Track A `send-keys` (agents accept image file paths). Additive; fits the ssh-then-run + `tmux -L spar` model. Thin-client alternative deferred — see X7 | DECIDED — backlog feature 002 |

## Open

| ID | Topic | Status |
|----|-------|--------|
| X1 | First API provider to spike (xAI / Anthropic / OpenAI) | DECIDED — OpenAI-compatible (`api:openai`, `api:xai`) via ureq |
| X2 | TUI keymap / layout (mimic which product most?) | LEANING — j/k Tab a/r/s /commands (M1 shell) |
| X3 | Project template overrides day one vs later | OPEN |
| X4 | Bus steer reliability for native-cli headless | DECIDED — adapter-dispatched turn-boundary delivery (Stop-hook inject / native queue / SDK prompt / inbox-on-next-turn per adapter) |
| X5 | vals scrape parser brittleness / grace TTL days / ship in-repo snapshot | OPEN — decide at MS phase A impl |
| X6 | Exact vals model id → `cli:`/`api:` + model string mapping table | OPEN — phase A/B |
| X7 | **Thin-client split** (`spar --remote`: local TUI ↔ remote orchestrator over spar's own protocol stream) as an alternative to ssh-then-run — would make image paste fall out as one message type (herdr's model) and give attach/persistence without tmux. Reopens the tmux-vs-own-protocol call (W1/W2) | OPEN — backlog; revisit after the workspace initiative lands |
