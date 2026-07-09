# Roadmap

## Milestone 0 — Skeleton (done)

- [x] Rust CLI crate, doctor, providers detect  
- [x] Dry-run workflows: plan / implement / arena / roles / peer  
- [x] Worktrees, ship gate helpers, thin TUI stub  
- [x] Dual-backend architecture doc  
- [x] Merged to `main` (`3a39c10`)

## Milestone 1 — Product shell (TUI-first)

- [x] `spar` with no subcommand opens full TUI in repo  
- [x] Fleet panel, phase/gates, live log pane, basic actions  
- [x] Event stream file + follow  
- [x] Skills: `skills list` / `skills get core`  
- [x] AGENTS.md blurb for outer agents  

## Milestone 2 — Swarm bus (A2A)

- [ ] Run-scoped bus layout (`events.jsonl`, presence, inbox)  
- [ ] Typed send/broadcast; human peer in TUI  
- [ ] Path reserve/release  
- [ ] Wire roles/peer to bus (replace thin mailbox)  

## Milestone 3 — Workflow hardening (native-cli)

- [ ] One run id plan→implement (no child run)  
- [ ] Gate knobs + autonomy levels  
- [ ] Arena winner **and** reconcile path  
- [ ] Safe auto-cleanup  
- [ ] Quota scrape (Claude five_hour + log phrases)  
- [ ] Live headless spikes per provider  

## Milestone 4 — API backend v0

- [ ] In-tree thin agent runtime  
- [ ] First SDK provider + streaming + usage in TUI  
- [ ] Same workflows on api-sdk slots  

## Milestone 5 — Fleet excellence

- [ ] Mixed CLI+API runs  
- [ ] Task DAG waves for `--big`  
- [ ] Message budgets, stuck detection on bus  
- [ ] Polish TUI to “daily driver” quality  

## Later

- Multi-machine / remote workers  
- More API providers  
- bwrap profiles per untrusted model  
