# Roadmap

## Milestone 0 — Skeleton (done)

- [x] Rust CLI crate, doctor, providers detect  
- [x] Dry-run workflows: plan / implement / arena / roles / peer  
- [x] Worktrees, ship gate helpers, thin TUI stub  
- [x] Dual-backend architecture doc  

## Milestone 1 — Product shell (TUI-first)

- [x] `spar` with no subcommand opens full TUI in repo  
- [x] Fleet panel, phase/gates, live log pane, basic actions  
- [x] Event stream file + follow  
- [x] Skills: `skills list` / `skills get core`  
- [x] AGENTS.md blurb for outer agents  

## Milestone 2 — Swarm bus (A2A)

- [x] Run-scoped bus layout (`events.jsonl`, presence, inbox)  
- [x] Typed send/broadcast; human peer in TUI / CLI  
- [x] Path reserve/release  
- [x] Wire roles/peer to bus (replace thin mailbox as primary)  

## Milestone 3 — Workflow hardening (native-cli)

- [x] One run id plan→implement (no child run)  
- [x] Gate knobs + autonomy levels  
- [x] Arena winner **and** reconcile path  
- [x] Safe auto-cleanup (config `auto_cleanup`, fail-closed default off)  
- [x] Quota scrape (Claude five_hour JSON + log phrases)  
- [x] Live headless path retained (provider adapters + doctor)

## Milestone 4 — API backend v0

- [x] In-tree thin agent runtime  
- [x] First OpenAI-compatible SDK lane (`api:openai`, `api:xai`, …) + usage on run  
- [x] Same workflows on api-sdk slots (provider refs + executor branch)

## Milestone 5 — Fleet excellence

- [x] Mixed CLI+API runs (per-slot `cli:` / `api:` provider refs)  
- [x] Task DAG waves for `--big`  
- [x] Message budgets, bus presence/heartbeat  
- [x] TUI product shell (M1) + bus/events visibility  

## Milestone 6: Operator model (session disposable, TUI primary)

- [x] **Durable run ownership** (setsid detach, per-project daemon, lifecycle notifications, brief on disk, concurrency cap) - see `features/003-durable-run-ownership.md`  
- [x] **TUI information architecture** (noun set, Home landing view, new-run flow, off-thread snapshot scans) - see `features/004-tui-information-architecture.md`  
- [x] **One run per unit of work** (rounds, attach-by-default, `spar link`, folded listings, bulk archive) - see `features/007-one-run-per-unit-of-work.md`  
- [x] **Gate evidence** (Plan tab, Review tab with AC-n status, diff and review verdicts) - see `features/005-gate-evidence.md` — stopped at run `abd35a54` round 11 (13/17, 14/17). The criteria still open are Main-content rendering, which feature 010 rebuilds; remainder folded there rather than patched onto the string log viewer  
- [x] **Orchestrator conversation** (resident conversation surface, native-cli turn loop, intake to brief + fleet + launch, gate consultation) - see `features/008-orchestrator-conversation.md`  
- [ ] **Run composition** (workflow choice, role-by-role fleet with models and backups, operator defaults, the conversation completes the form) - see `features/009-run-composition.md`  
- [x] **Structured views** (records not lines: columns, timestamps, folding, navigation by structure) - see `features/010-structured-views.md`  
- [x] **Motion and visual identity** — chrome rebuild, tokens and the snapshot harness (C+D, U14); time-based motion, 60fps frame ramp with synchronized output and focus gating, spar own page and palette (A, U29/U30/U31); reserved-space layout, animated rail re-sort, tab-strip glide and skeletons (B, U39-U43) - see `features/006-motion-and-identity.md`  
- [x] **Fleet shaping** (honest provider precedence, a pinned reviewer panel as an exact
  panel not a padded one, the resolved fleet visible at gates, `--without` and `--fleet`
  presets) - see `features/011-fleet-shaping.md`  

## Later

- Multi-machine / remote workers  
- More native API SDKs (Anthropic messages API, Google, Meta)  
- bwrap profiles per untrusted model  
- Streaming token SSE into TUI  
- **Dynamic model select** (vals-backed) — see `features/001-model-select.md`
