# spar — Product vision

**Not a thin orchestrator wrapper.**  
A **first-class coding-agent product** in the same class as Claude Code, Grok Build, and Antigravity (`agy`), with multi-agent, multi-provider and dual-backend orchestration behind one CLI. The engine is the `spar` binary; the human front end is a pi package (`pi/`, O145) that drives it through `spar <cmd> --json`.

---

## One-liner

**spar** is the terminal product you open to get hard software work done with a *fleet* of agents — subscription CLIs and/or API models — that plan, build, review, compete, reconcile, and coordinate, under your gates and in isolated worktrees.

---

## Positioning

| | Claude / Grok / agy | Pi + messenger | **spar** |
|--|---------------------|----------------|-----------------|
| Primary UX | Single-agent TUI | Single-agent + optional multi ext | **Fleet-native CLI + pi front end** |
| Providers | One vendor | Multi-model, one harness | **Multi-vendor + dual backend** (CLI sub + API SDK) |
| Multi-agent | Subagents / best-of | Extension mesh + crew | **First-class workflows + swarm bus** |
| Isolation | Often same tree | Same folder + file reserves | **Always worktrees** |
| Human | Chat with one agent | Peer in mesh overlay | **Operator in control room** + optional chat peer |
| Ship | You drive git/PR | You drive | **Gated draft PR** built-in |

You still use Claude/Grok/agy as **workers** (native-cli backend). spar is the **product you live in** when the job needs more than one brain.

---

## Default experience: pi front end over the CLI

Humans work in pi with the `spar-pi` package (`pi/`, O145): a footer with runs in flight,
runs waiting on the operator, paused providers and cost; `/spar` run views; human gates as
pi dialogs; and model-callable tools. The package is a thin client: it reads and acts only
through `spar <cmd> --json`, so the CLI stays the single engine and the one contract
agents and humans share. Bare `spar` prints help.

Headless/CLI is first-class for **outer agents** (`--json`, `wait --follow`, skills) and is
what the pi package itself calls.

---

## Core product features

### Dual execution backend

See [architecture-dual-backend.md](./architecture-dual-backend.md).

- **native-cli** — claude / grok / agy (subscription, TOS-safe)  
- **api-sdk** — OpenAI, Anthropic, xAI, Google, Meta, … (metered, full control)  
- Mixable per slot when ready; same workflows and `.spar/` layout  

### Workflows (orchestrator-owned)

| Workflow | Product meaning |
|----------|-----------------|
| **Plan** | Multi-provider plan + real critic; structured/big mode like plan-big |
| **Implement** | 1 implementer + adversarial reviewers (2 by default; a pinned `--role reviewer=` list sets the size exactly, `--fleet small` narrows to 1); stuck policy |
| **Arena** | N implementers → **winner** *or* **reconcile** (+ review) → ship |
| **Roles / peer** | Split stack, mailbox/bus, contracts |
| **Ship** | Draft PR, never merge; force-with-lease only on swarm branches |

### Swarm bus (A2A — not a Pi plugin)

First-class **run-scoped** agent-to-agent communication:

- Presence, heartbeat, stuck  
- Typed messages (chat, blocked, contract, review_finding, ack, …)  
- Path reservations  
- Activity feed (`events.jsonl`, `wait --follow --json`)  
- Delivery: steer (API) / inbox+inject (CLI) / human (`@human` inbox + notifier)  
- Message budgets to prevent chat storms  
- Optional task DAG + waves for large features  

Better than pi-messenger by being **backend-agnostic**, **run-scoped**, **typed**, and **tied to a real orchestrator** (not only a chat room in one harness).

### Isolation & safety

- Always worktrees for coding slots  
- Fail-closed cleanup (never drop trees we still need)  
- Optional bwrap per provider (off by default until configured)  
- Never hijack user’s personal tmux; optional namespaced sessions only  

### Autonomy & gates

Configurable knobs: plan / winner / ship / full-auto levels. Default lean autonomous; ship remains the serious gate unless you turn it up.

### Discovery (agent-browser style)

- `spar skills list|get core|...`  
- Short AGENTS.md blurb for outer agents  
- Product teaches itself; no “read 40 pages of markdown first”  

### Quota & cost

- Best-effort CLI limit scrape (Claude `rate_limits.five_hour.*`, etc.)  
- API token/$ in `status`  
- Pause/resume providers; exit 4 when none usable  

---

## What “done product” feels like (north star demos)

1. **You open pi in a repo**, type a goal, watch the plan run in the spar footer, approve in a dialog, implement+dual-review runs, draft PR opens.  
2. **Arena + reconcile:** three providers implement; you choose reconcile; merge agent + review; ship one branch.  
3. **Peer FE/BE:** two worktrees, bus contract messages, the bus shows blocked/unblocked, joint review, PR.  
4. **Outer agent:** Claude in another pane runs `spar plan --json` via skill, never needs a front end.  
5. **API day:** same front end, slots on `api:anthropic` + `cli:grok`, usage meters visible.  

---

## Non-goals

- Being an engine that lives inside Pi / Claude / Grok (the pi package is a front end only)  
- Replacing every single-agent daily driver for tiny edits (those CLIs stay great)  
- Auto-merging to main  
- Multi-machine cluster (later, maybe)  
- Perfect CLI completion detection day one (improve continuously)  

---

## Relationship to current codebase

`main` has an **orchestrator skeleton** (workflows, dry-run, providers, ship, dry-run test backend).  

The product pivot:

| Was | Becomes |
|-----|---------|
| CLI-first + optional dashboard | **CLI engine** + pi front end for humans (the TUI was removed, O145) |
| Thin mailbox | **Swarm bus** |
| native-cli only story | **Dual backend** (API track planned) |
| “tool to manage sessions” | **Agent product that owns the fleet** |

Evolution, not rewrite: keep `.spar/`, workflows, adapters; promote the bus to the center; humans reach it through pi.

---

## Success metrics (qualitative)

- You open spar instead of three tmux panes of claude/grok/agy for multi-agent jobs  
- Outer agents reliably drive it via skills + exit codes  
- Arena/reconcile and peer bus feel *better* than pi-messenger crew for your repos  
- When API prices are right, flipping a slot to API needs no second tool  
