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

## Later

- Multi-machine / remote workers  
- More native API SDKs (Anthropic messages API, Google, Meta)  
- bwrap profiles per untrusted model  
- Streaming token SSE into TUI  
- **Dynamic model select** (vals-backed) — see `features/001-model-select.md`
