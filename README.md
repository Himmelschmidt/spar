# agent-swarm

A **first-class multi-agent coding product** for the terminal — fleet-native TUI (Claude / Grok / agy class), multi-provider orchestration, dual backends (subscription CLIs **and** API SDKs), and a real agent-to-agent bus.

Not a plugin for Pi or any other harness. Outer agents drive it headlessly (`--json`, skills).

## Status

**On `main`:** orchestrator skeleton (dry-run workflows, worktrees, ship helpers, provider detect). Product direction: **TUI-first**, swarm bus, dual-backend — see docs.

- [docs/PRODUCT.md](docs/PRODUCT.md) — product vision  
- [docs/architecture-dual-backend.md](docs/architecture-dual-backend.md) — CLI + API backends  
- [docs/architecture-a2a.md](docs/architecture-a2a.md) — swarm bus  
- [roadmap/ROADMAP.md](roadmap/ROADMAP.md) — milestones  
- [DECISIONS.md](DECISIONS.md) — locked decisions

## Install

```bash
cargo install --path .
# or
cargo build --release
```

## Quick start

```bash
agent-swarm doctor
agent-swarm doctor --json
agent-swarm provider list
agent-swarm status
```

## Dry-run demos (no live providers)

```bash
# Path A
agent-swarm plan --task "add retry to the payment client" --dry-run --json
# exit 2 = awaiting approval; note plan run_id
agent-swarm approve <plan-run-id>
agent-swarm implement --run <plan-run-id> --dry-run --json
# implement returns a *new* run_id — wait/status that one (or read plan.child_run)
agent-swarm status <impl-run-id> --json

# Path B
agent-swarm implement --task "fix the flaky test" --dry-run --json

# Arena / peer / roles
agent-swarm run --workflow arena --task "feature X" --dry-run --json
agent-swarm run --workflow peer --task "split stack" --dry-run --json
agent-swarm run --workflow roles --task "fe/be feature" --dry-run --json

# Or: AGENT_SWARM_DRY_RUN=1
```

## Live Path A — plan then implement

```bash
agent-swarm plan --task "add retry to the payment client" --detach --json
agent-swarm wait <run-id> --json          # exit 2 = awaiting your approval
# review .swarm/runs/<id>/artifacts/plan.md
agent-swarm approve <run-id>
agent-swarm implement --run <run-id> --detach
agent-swarm wait <run-id>
# after human confirm:
agent-swarm ship <run-id> --confirm
```

## Path B — just do it

```bash
agent-swarm implement --task "fix the flaky test in foo_test.rs" --detach
agent-swarm wait <run-id>
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
.swarm/runs/<run-id>/
  state.json
  artifacts/
  mailbox/
  markers/
  logs/
```

Worktrees (default isolation): `../<repo>-swarm-<run>-<slot>` on branch `swarm/<run>/<slot>`.

## Backend policy

- `auto` (default): prefer headless when the adapter supports it; else tmux
- `headless`: process spawn + log capture
- `tmux`: session windows + markers; `agent-swarm attach <run-id>`

## License

MIT
