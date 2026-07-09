# agent-swarm

Orchestrate **native subscription AI CLIs** (Claude Code, Grok Build, Antigravity/`agy`) for multi-provider planning, implementation, and review — without provider API billing or third-party API harnesses.

Designed so **another AI agent** can drive it: stable subcommands, `--json`, detach/wait, exit codes, artifacts under `.swarm/`.

## Status

M0–M6 implemented:

- Headless process runner + provider spawn adapters (`claude` / `grok` / `agy`)
- Tmux backend (`--backend tmux`), attach
- Git worktree isolation + optional dbiso seed + optional bwrap
- Plan → approve → implement, arena, loop, roles, peer
- Ship gate (confirm + push `--force-with-lease` + `gh pr create`, never merge)
- Provider pause/resume + quota hints
- Minimal ratatui dashboard
- **Dry-run mode** for CI / demos without live LLM sessions

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
