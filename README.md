# spar

**spar** runs a *fleet* of coding agents against a git repo — plan, implement, review, compete (arena), and ship a draft PR — from a terminal TUI or a headless CLI.

You keep using Claude Code, Grok Build, Antigravity (`agy`), and/or API models as **workers**. spar is the orchestrator and control room.

Humans: open `spar` (TUI).  
Outer agents (Claude, Grok, …): drive it with CLI + `--json` after reading the built-in skill.

> **v0.0.1 — early release.** Dry-run workflows are solid for learning the flow. Live runs need real provider CLIs and/or API keys. APIs and UX may still change.

---

## Requirements

| Need | Notes |
|------|--------|
| **Rust** (stable) | `rustup` / `cargo` to build |
| **git** | Required |
| **A project git repo** | spar runs *inside* the repo you want worked on |
| **At least one provider** | See [Providers](#providers) |
| **tmux** (optional) | Interactive backend / `spar attach` |
| **bwrap** (optional) | Bubblewrap sandbox isolation |

Supported native CLIs on `PATH` today: `claude`, `grok`, `agy`.

---

## Install

From source (recommended):

```bash
cargo install --git https://github.com/Himmelschmidt/spar.git --locked
```

Or clone and install locally:

```bash
git clone https://github.com/Himmelschmidt/spar.git
cd spar
cargo install --path . --locked
```

Build without installing:

```bash
cargo build --release
# binary: ./target/release/spar
```

> The crates.io package name `spar` is taken by an unrelated project. Install from this repository.

Check the install:

```bash
spar --version
spar doctor
```

`spar doctor` should show `git` ok and at least one provider available (or set API keys for `api:*` backends).

---

## Teach your coding agent about spar

If you use Claude Code, Grok, Antigravity, or similar with a **global** instruction file (`~/.claude/CLAUDE.md`, `~/.grok/AGENTS.md`, project `AGENTS.md`, etc.), paste a short block so the outer agent knows to *drive spar* instead of reinventing multi-agent orchestration.

**Minimal (recommended)** — always load the live contract first. Paste into
`~/.claude/CLAUDE.md`, `~/.grok/AGENTS.md`, a project `AGENTS.md`, or equivalent:

~~~~markdown
## spar (multi-agent coding)

Use **spar** for multi-provider plan / implement / review / arena fleets.
Humans: `spar` (TUI). Outer agents: CLI + `--json`.

```bash
spar skills get core          # full operator skill — read this first
spar doctor --json
spar plan -t "…" --providers cli:claude,cli:grok --json [--dry-run]
spar approve <run_id> --json  # exit 2 = human gate (plan/wait/ship; not status)
spar implement --run <run_id> --providers cli:claude,cli:grok,cli:agy --json
spar wait <run_id> --follow --json
spar status <run_id> --json   # process exit always 0; use phase / exit_code
spar ship <run_id> --confirm  # draft PR only, never merge
spar run --workflow review -t "…" --providers cli:claude,cli:grok
```

`--providers` is required on plan / implement / run (`cli:name` or `api:name` only).
Ship never merges. Coding slots use worktrees only.
~~~~

**Even shorter** if you only want the skill pointer:

~~~~markdown
## spar
For multi-agent coding fleets, run `spar skills get core` and follow that contract.
~~~~

The full skill is always available from the binary (no extra install):

```bash
spar skills list
spar skills get core
```

Repo copy of the outer-agent blurb: [`AGENTS.md`](AGENTS.md).

---

## Quick start

From any **git project** you want spar to work on:

```bash
cd /path/to/your/project
spar doctor          # providers, git, optional tools
spar                 # fleet TUI
```

### Dry-run (no live agents, no real worktrees)

Safe way to learn the flow:

```bash
spar plan -t "add retry logic to the HTTP client" \
  --providers cli:claude,cli:grok --dry-run --json
# note run_id; exit 2 often means "awaiting plan approval"

spar approve <run_id>
spar implement --run <run_id> \
  --providers cli:claude,cli:grok,cli:agy --dry-run --json
spar status <run_id> --json
```

Or set `SPAR_DRY_RUN=1` for the same effect.

---

## Providers

Every plan / implement / run needs an explicit `--providers` list. No silent default fleet.

| Ref | Backend | Needs |
|-----|---------|--------|
| `cli:claude` | Claude Code CLI | `claude` on `PATH` |
| `cli:grok` | Grok Build CLI | `grok` on `PATH` |
| `cli:agy` | Antigravity CLI | `agy` on `PATH` |
| `api:openai` | OpenAI-compatible HTTP | `OPENAI_API_KEY` |
| `api:xai` | xAI (OpenAI-compatible) | `XAI_API_KEY` |

Optional env overrides: `OPENAI_BASE_URL`, `OPENAI_MODEL`, `XAI_BASE_URL`, `XAI_MODEL`, and similar for other `api:*` names.

Mix CLI and API in one run:

```bash
spar implement -t "fix the flaky test" \
  --providers cli:claude,api:openai --detach --json
```

Inventory:

```bash
spar provider list
```

---

## Everyday usage

### Human (TUI)

```bash
spar                 # product TUI in current repo
spar --task "…"      # open TUI with a seeded task
```

### Path A — plan, then implement (one run id)

```bash
spar plan -t "add retry logic to the HTTP client" \
  --providers cli:claude,cli:grok --detach --json

spar wait <run_id> --json
# exit 2 → review plan.md + test-contract.md under .spar/runs/<id>/artifacts/ then:
spar approve <run_id>

# SAME run id continues into implement
spar implement --run <run_id> \
  --providers cli:claude,cli:grok,cli:agy --detach --json

spar wait <run_id>
spar ship <run_id> --confirm    # draft PR only — never merges
```

### Path B — just implement

```bash
spar implement -t "fix the flaky test in foo_test.rs" \
  --providers cli:claude --detach --json
spar wait <run_id>
```

### Multi-model review

```bash
spar run --workflow review -t "Review auth changes for bugs" \
  --providers cli:claude,cli:grok --detach --json
```

### Arena (compete, then pick or merge)

```bash
spar run --workflow arena -t "feature X" \
  --providers cli:claude,cli:grok,cli:agy --detach --json
# later: spar confirm <run_id>  or  spar reconcile <run_id>
spar ship <run_id> --confirm
```

Other workflows: `peer` (split-stack), `roles`, `loop`. See `spar run --help` and `spar skills get core`.

### Observe

```bash
spar status [run_id] --json
spar wait <run_id> --follow --json
spar logs <run_id> [slot] -f
spar bus log <run_id>
spar attach <run_id>          # tmux backend only
```

---

## Config

Optional project file `spar.toml` (or `~/.config/spar/config.toml`).  
Start from the example:

```bash
cp /path/to/spar/spar.toml.example ./spar.toml
```

Useful knobs:

```toml
max_agents = 4
default_backend = "auto"   # auto | headless | tmux
isolation = "worktree"     # none | worktree | …
autonomy = "semi"          # manual | semi | high | full

[gates]
plan = true
winner = true
ship = true

[ship]
auto_confirm = false

[timeouts]
slot_secs = 1800
stall_warn_secs = 300
wait = "2h"
```

Higher `autonomy` auto-approves more gates; ship still prefers an explicit human confirm unless you change that deliberately.

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success / idle ok |
| 1 | Failure |
| 2 | Waiting on a human gate (approve plan, winner, ship) |
| 3 | Stuck / escalated / wait timeout |
| 4 | Provider quota / no usable provider |

**`status` is observe-only:** process exit is always `0` if the run loads. Read JSON `phase` / `exit_code` for run state. Use `wait` when you want the process exit coded by gate/stuck/quota.

---

## Where state lives

```
.spar/runs/<run-id>/
  state.json
  events.jsonl
  artifacts/          # plan.md, test-contract.md, reviews, …
  bus/                # swarm A2A bus
  logs/               # per-slot logs
  mailbox/
  markers/
```

Live coding slots use **sibling git worktrees** (not the main checkout), typically:

`../<repo>-spar-<run>-<slot>` on branch `spar/<run>/<slot>`.

Dry-run does **not** create real worktrees.

Cleanup:

```bash
spar cleanup <run_id>           # remove worktrees
spar cleanup <run_id> --purge   # also drop run data under .spar/
```

---

## Backend modes

| Mode | Behavior |
|------|----------|
| `auto` (default) | Prefer headless when the provider supports it; else tmux |
| `headless` | Spawn processes + capture logs |
| `tmux` | Session windows; `spar attach <run_id>` |

---

## Docs (deeper)

| Doc | What |
|-----|------|
| [`spar skills get core`](skills/core.md) | Outer-agent operator skill (source of truth in-tree) |
| [`AGENTS.md`](AGENTS.md) | Short blurb for project-scoped agent instructions |
| [`docs/agent-operator.md`](docs/agent-operator.md) | Operator contract details |
| [`docs/PRODUCT.md`](docs/PRODUCT.md) | Product vision |
| [`docs/architecture-dual-backend.md`](docs/architecture-dual-backend.md) | CLI + API backends |
| [`docs/architecture-a2a.md`](docs/architecture-a2a.md) | Swarm bus |
| [`roadmap/ROADMAP.md`](roadmap/ROADMAP.md) | Milestones |
| [`DECISIONS.md`](DECISIONS.md) | Locked design decisions |
| [`spar.toml.example`](spar.toml.example) | Config template |

---

## License

[MIT](LICENSE)
