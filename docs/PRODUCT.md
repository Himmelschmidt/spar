# spar — Product vision

**Not a Pi extension. Not a thin orchestrator wrapper.**  
A **first-class coding-agent product** in the same class as Claude Code, Grok Build, and Antigravity (`agy`) — with multi-agent, multi-provider, dual-backend, and a real control-room TUI as the default surface.

---

## One-liner

**spar** is the terminal product you open to get hard software work done with a *fleet* of agents — subscription CLIs and/or API models — that plan, build, review, compete, reconcile, and coordinate, under your gates and in isolated worktrees.

---

## Positioning

| | Claude / Grok / agy | Pi + messenger | **spar** |
|--|---------------------|----------------|-----------------|
| Primary UX | Single-agent TUI | Single-agent + optional multi ext | **Fleet-native TUI** |
| Providers | One vendor | Multi-model, one harness | **Multi-vendor + dual backend** (CLI sub + API SDK) |
| Multi-agent | Subagents / best-of | Extension mesh + crew | **First-class workflows + swarm bus** |
| Isolation | Often same tree | Same folder + file reserves | **Always worktrees** |
| Human | Chat with one agent | Peer in mesh overlay | **Operator in control room** + optional chat peer |
| Ship | You drive git/PR | You drive | **Gated draft PR** built-in |

You still use Claude/Grok/agy as **workers** (native-cli backend). spar is the **product you live in** when the job needs more than one brain.

---

## Default experience: first-class TUI

Launch:

```bash
spar                 # open product TUI in current repo
spar .               # explicit cwd
spar --task "..."    # start with a task
```

The TUI is not `dashboard` as an afterthought. It **is** the app (like `claude` / `grok` / `agy`).

### TUI pillars (parity with best-in-class agents)

1. **Session / run home** — active run, phase, gates, one run id end-to-end  
2. **Fleet panel** — every slot: role, provider, backend (cli|api), status, model, quota, worktree/branch  
3. **Stream** — live agent output (all slots); follow one or multiplex  
4. **Bus / chat** — swarm bus feed, DMs, broadcast, human as peer (Pi-messenger UX, better protocol)  
5. **Artifacts** — plan.md, reviews, rankings, PR links; open in pager  
6. **Actions** — approve plan, winner vs reconcile, ship draft PR, pause provider, cleanup  
7. **Composer** — talk to orchestrator *or* inject to a slot (steer when backend allows)  
8. **Doctor / providers** — who’s installed, API keys present, quota cooldowns  

Headless/CLI remains first-class for **outer agents** (`--json`, `wait --follow`, skills). Humans default to TUI.

### Quality bar for “nice”

- Fast redraw, clear hierarchy, no wall of dump  
- Keyboard-first (vim-ish or agent-cli familiar bindings)  
- Streaming that feels live, not “refresh status every 2s” only  
- Dark theme that doesn’t look like a debug console  
- Split panes that survive resize  
- Unread badges on bus; stuck/quota callouts  

Implementation stack: **Rust + ratatui** (product TUI is the default binary mode).

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
| **Implement** | 1 implementer + ≥2 adversarial reviewers; stuck policy |
| **Arena** | N implementers → **winner** *or* **reconcile** (+ review) → ship |
| **Roles / peer** | Split stack, mailbox/bus, contracts |
| **Ship** | Draft PR, never merge; force-with-lease only on swarm branches |

### Swarm bus (A2A — not a Pi plugin)

First-class **run-scoped** agent-to-agent communication:

- Presence, heartbeat, stuck  
- Typed messages (chat, blocked, contract, review_finding, ack, …)  
- Path reservations  
- Activity feed (dashboard + `events.jsonl`)  
- Delivery: steer (API) / inbox+inject (CLI) / human (TUI)  
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
- API token/$ in status + TUI  
- Pause/resume providers; exit 4 when none usable  

---

## What “done product” feels like (north star demos)

1. **You open `spar` in a repo**, type a goal, watch plan multi-provider stream in TUI, approve with one key, implement+dual-review runs, draft PR opens.  
2. **Arena + reconcile:** three providers implement; you choose reconcile; merge agent + review; ship one branch.  
3. **Peer FE/BE:** two worktrees, bus contract messages, dashboard shows blocked/unblocked, joint review, PR.  
4. **Outer agent:** Claude in another pane runs `spar plan --json` via skill, never needs the TUI.  
5. **API day:** same TUI, slots on `api:anthropic` + `cli:grok`, usage meters visible.  

---

## Non-goals

- Being a plugin inside Pi / Claude / Grok  
- Replacing every single-agent daily driver for tiny edits (those CLIs stay great)  
- Auto-merging to main  
- Multi-machine cluster (later, maybe)  
- Perfect CLI completion detection day one (improve continuously)  

---

## Relationship to current codebase

`main` has an **orchestrator skeleton** (workflows, dry-run, providers, ship, thin TUI stub).  

The product pivot:

| Was | Becomes |
|-----|---------|
| CLI-first + optional dashboard | **TUI-first product** + headless/CLI for agents |
| Thin mailbox | **Swarm bus** |
| native-cli only story | **Dual backend** (API track planned) |
| “tool to manage sessions” | **Agent product that owns the fleet** |

Evolution, not rewrite: keep `.spar/`, workflows, adapters; promote TUI and bus to the center.

---

## Success metrics (qualitative)

- You open spar instead of three tmux panes of claude/grok/agy for multi-agent jobs  
- Outer agents reliably drive it via skills + exit codes  
- Arena/reconcile and peer bus feel *better* than pi-messenger crew for your repos  
- When API prices are right, flipping a slot to API needs no second tool  
