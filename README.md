# spar

A **first-class multi-agent coding product** for the terminal — fleet-native TUI (Claude / Grok / agy class), multi-provider orchestration, dual backends (subscription CLIs **and** API SDKs), and a real agent-to-agent bus.

Not a plugin for Pi or any other harness. Outer agents drive it headlessly (`--json`, skills).

## Status

**Product v1 (M0–M5):** fleet TUI, swarm bus, one-run plan→implement→ship, arena reconcile, dual backend (`cli:*` + `api:*`), autonomy gates, skills discovery. Dry-run suites green; live CLI/API depend on installed providers / API keys.

- [docs/PRODUCT.md](docs/PRODUCT.md) — product vision  
- [docs/architecture-dual-backend.md](docs/architecture-dual-backend.md) — CLI + API backends  
- [docs/architecture-a2a.md](docs/architecture-a2a.md) — swarm bus  
- [roadmap/ROADMAP.md](roadmap/ROADMAP.md) — milestones  
- [DECISIONS.md](DECISIONS.md) — locked decisions  
- [AGENTS.md](AGENTS.md) / `spar skills get core` — outer agents

## Install

```bash
cargo install --path .
# or
cargo build --release
```

## Quick start

```bash
spar                         # product TUI in current git repo
spar doctor
spar doctor --json
spar provider list
spar skills get core         # outer-agent skill
spar status
```

## Dry-run demos (no live providers)

```bash
# --providers is required (no silent multi-agent default)

# Path A
spar plan --task "add retry to the payment client" --providers cli:claude,cli:grok --dry-run --json
# exit 2 = awaiting approval; note run_id
spar approve <run-id>
spar implement --run <run-id> --providers cli:claude,cli:grok,cli:agy --dry-run --json
# same run_id through plan → implement → ship
spar status <run-id> --json

# Path B
spar implement --task "fix the flaky test" --providers cli:claude --dry-run --json

# Arena / peer / roles
spar run --workflow arena --task "feature X" --providers cli:claude,cli:grok,cli:agy --dry-run --json
spar run --workflow peer --task "split stack" --providers cli:claude,cli:grok --dry-run --json
spar run --workflow roles --task "fe/be feature" --providers cli:claude,cli:grok --dry-run --json

# Or: SPAR_DRY_RUN=1
```

## Live Path A — plan then implement

```bash
spar plan --task "add retry to the payment client" --providers cli:claude,cli:grok --detach --json
spar wait <run-id> --json          # exit 2 = awaiting your approval
# review .spar/runs/<id>/artifacts/plan.md
spar approve <run-id>
spar implement --run <run-id> --providers cli:claude,cli:grok,cli:agy --detach
spar wait <run-id>
# after human confirm:
spar ship <run-id> --confirm
```

## Path B — just do it

```bash
spar implement --task "fix the flaky test in foo_test.rs" --providers cli:claude --detach
spar wait <run-id>
```

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success / idle ok |
| 1 | Failure |
| 2 | Waiting on human gate |
| 3 | Stuck / escalated |
| 4 | Provider quota |

See [docs/agent-operator.md](docs/agent-operator.md).

## Layout

```
.spar/runs/<run-id>/
  state.json
  artifacts/
  mailbox/
  markers/
  logs/
```

Worktrees (default isolation): `../<repo>-spar-<run>-<slot>` on branch `spar/<run>/<slot>`.

## Backend policy

- `auto` (default): prefer headless when the adapter supports it; else tmux
- `headless`: process spawn + log capture
- `tmux`: session windows + markers; `spar attach <run-id>`

## License

MIT
