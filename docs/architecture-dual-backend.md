# Dual-backend architecture

**Status:** DECIDED (product discussion, 2026-07-09)  
**Binary:** `agent-swarm`  
**Principle:** One orchestrator; two (or more) execution backends. Workflows do not fork.

---

## Problem

1. **Subscription CLIs** (Claude Code, Grok Build, Antigravity/`agy`, …) are cheap relative to API billing and match how you already work, but third-party HTTP harnesses are often against provider TOS. Orchestration must drive **native CLIs**, not reimplement their APIs.
2. **API SDKs** (OpenAI, Anthropic, xAI, Google, Meta, …) give clean streaming, tools, structured output, and usage metering. As API prices fall, a first-class API path becomes desirable—not as a replacement product, but as a **second lane** behind the same swarm.

Spawning CLI sessions is “hacky” only at the **adapter** layer. The product is multi-agent orchestration (plan, implement, review, arena, reconcile, roles/peer, ship, dashboard).

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  Outer agent / human                                            │
│  agent-swarm skills (agent-browser style) + AGENTS.md blurb     │
│  CLI (--json, wait --follow) · nice TUI dashboard               │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│  Orchestrator (backend-agnostic)                                │
│  · one run id per job                                           │
│  · phases, gates, stuck policy                                  │
│  · worktrees, mailbox, artifacts, markers, event stream         │
│  · workflows: plan, implement, arena, reconcile, roles, peer    │
│  · ship (draft PR, never merge)                                 │
│  · quota registry (pause/resume/cooldown)                       │
└────────────┬───────────────────────────────┬────────────────────┘
             │                               │
             ▼                               ▼
┌────────────────────────┐     ┌────────────────────────────────┐
│  Backend: native-cli   │     │  Backend: api-sdk              │
│  · spawn claude/grok/  │     │  · in-tree thin agent runtime  │
│    agy (headless)      │     │  · provider SDKs               │
│  · optional namespaced │     │  · tool loop (read/edit/bash…) │
│    tmux (never user    │     │  · streaming + token usage     │
│    sessions)           │     │  · optional $ estimates        │
│  · scrape rate limits  │     │  · full control / metered      │
│    best-effort         │     │                                │
└────────────────────────┘     └────────────────────────────────┘
             │                               │
             └───────────┬───────────────────┘
                         ▼
              .swarm/runs/<id>/  (same layout either way)
```

### Non-negotiable split

| Layer | Owns | Does **not** own |
|-------|------|------------------|
| **Orchestrator** | Run lifecycle, workflows, isolation, gates, review policy, arena/reconcile, roles/peer, ship, dashboard events | Provider wire protocols |
| **Backend** | How a slot “thinks and acts” (CLI process vs API agent loop), streaming into the run log, usage/quota signals | Whether to arena vs implement; worktree policy |
| **Adapter** | One provider on one backend (e.g. `claude` native-cli, `anthropic` api-sdk) | Cross-run scheduling |

---

## Backend: native-cli

**Purpose:** Subscription-friendly multi-provider swarm without third-party API harnesses.

| Concern | Decision |
|---------|----------|
| Default execution | **Headless** process spawn |
| Tmux | Opt-in only; **dedicated session per run** (`agent-swarm-<run_id>`); never touch the user’s personal sessions |
| Completion | Process exit + expected artifacts/markers; never success-on-timeout-alone |
| Trust | Configurable; default strong auto-approve flags the human UI allows |
| Quota | Best-effort parse of provider signals (e.g. Claude `rate_limits.five_hour.*` as in statusline JSON) + log/error scrape + manual pause |
| Providers (v1) | `claude`, `grok`, `agy` |

Dry-run remains a test backend: real `.swarm/` layout, no provider spawn.

---

## Backend: api-sdk

**Purpose:** First-class metered agents when you opt in—clean SDK use, not a second product.

| Concern | Decision |
|---------|----------|
| Runtime | **In-tree thin tool loop** (not a heavy third-party agent framework dependency for core) |
| Providers | OpenAI, Anthropic, xAI, Google, Meta, … via official SDKs as added |
| Streaming | Token/events into the **same** run log / event stream the dashboard tails |
| Usage | Record input/output tokens (and optional cost estimate) per slot on the run |
| Tools | Shared tool surface orchestrator configures (fs, git-safe commands, tests)—API agents do not bypass worktree cwd |
| Selection | Opt-in: config / CLI / per-slot; mixable with native-cli in one run |

### Suggested implementation order (API lane)

1. Freeze orchestrator contracts (this doc + run state / events).
2. **API backend v0** — one provider done well (streaming + tools + usage).
3. Pluggable API providers (matrix of SDKs).
4. Mixed fleets + cost knobs (“prefer API under $X”, “CLI when API paused”).
5. Keep native-cli maintained; no deprecation until API path wins daily use for you.

**First API provider to spike:** TBD (xAI / Anthropic / OpenAI)—pick when implementing the lane, not a blocker for native-cli polish.

---

## Mixing backends in one run

Allowed and desirable later:

- Plan on subscription CLIs, arena implementers on cheap API models  
- Reviewers on a different backend than the implementer  
- Fail over: provider paused on CLI → reschedule slot to API (or vice versa) if config allows  

v1 can ship **one backend per run** if mixing is unfinished; the **types** must still allow per-slot backend so we don’t paint into a corner.

```text
Slot {
  id, role, provider,      // "claude" | "anthropic" | "grok" | ...
  backend,                 // native-cli | api-sdk
  cwd, worktree, ...
}
```

Provider id is namespaced by backend in config to avoid ambiguity, e.g.:

```toml
# Conceptual — exact schema when implemented
[[slots]]
# or provider = "cli:claude" / "api:anthropic"
```

---

## Orchestrator contracts (shared)

These must not depend on backend:

1. **One run id** for plan → approve → implement → ship (phases on one record).
2. **Always worktrees** for coding slots; primary checkout never switches feature branches.
3. **Cleanup fail-closed** — auto-remove worktrees only when the run is fully terminal and nothing still needs the tree; branches outlive worktree dirs when possible.
4. **Artifacts / mailbox / markers / logs / events** under `.swarm/runs/<id>/`.
5. **Gates** configurable (`plan`, `winner`, `ship`, autonomy levels).
6. **Exit codes:** 0 ok, 1 fail, 2 human gate, 3 stuck, 4 quota.
7. **Stream everything** — process or API tokens → followable logs + dashboard.
8. **Ship:** draft PR default, never merge; force-with-lease only on swarm-owned branches.

---

## Workflows (backend-agnostic)

| Workflow | Behavior |
|----------|----------|
| **Plan** | Multi-provider plan + real critic/synthesize; **structured plan-big style** when large (`--big` / flag) |
| **Implement** | 1 implementer + ≥2 adversarial reviewers; stuck: fix → rotate provider → widen reviewers → stuck |
| **Arena** | N implementers; finish **`winner`** *or* **`reconcile`** (merge-good-parts agent → multi-review → ship) |
| **Roles / peer** | First-class v1; worktrees + mailbox protocol; multi-review before ship |
| **Ship** | Confirm per gates; draft PR |

Two implementers in the *default* loop is overkill; multi-implementer lives in **arena** (and optional explicit flag later).

---

## Discovery (agent-browser pattern)

- `agent-swarm skills list` / `skills get core` (and workflow skills)
- Short **AGENTS.md** blurb: use agent-swarm for multi-provider orchestration; load core skill first; don’t invent API harnesses for subscription CLIs
- Dashboard and CLI both first-class for humans; outer agents prefer CLI + JSON + stream

---

## Quota & cost

| Backend | Signal | Action |
|---------|--------|--------|
| native-cli | Best-effort (Claude `rate_limits.five_hour.used_percentage` / `resets_at`, log phrases, errors) | Pause provider until reset; schedule others; exit 4 if none |
| api-sdk | SDK usage / HTTP 429 | Pause or backoff; surface tokens/$ in status + dashboard |
| both | Manual `provider pause/resume` | Always honored |

Context-window % (Claude statusline `context_window.used_percentage`) is **not** the same as rate limit; log it, don’t auto-pause solely on context pressure unless configured later.

---

## Dashboard

First-class **nice** TUI control room (not a thin `status` skin):

- Phase, gates, run id  
- Per-slot: backend, provider, role, state, activity, quota/cost badge  
- Live log/event stream  
- Arena candidates + reconcile status  
- Actions: approve, winner vs reconcile, ship, pause provider  

Outer agents need not use it; humans should want to.

---

## What we are not doing

- Two binaries or two products  
- Replacing native-cli before api-sdk is good enough for daily use  
- Calling subscription HTTP APIs as a “clever” bypass of native CLIs  
- Cleaning worktrees while a gate or worker might still need them  
- Merging PRs automatically  

---

## Milestone alignment

| Track | Focus |
|-------|--------|
| **A — Orchestrator + native-cli** | Current plan M0–M6 refined by discussion decisions (one run id, gates knobs, stream, arena reconcile, roles/peer, nice dashboard, safe cleanup) |
| **B — api-sdk lane** | Runtime + first provider + usage; then multi-SDK; then mixed fleets |
| **C — Polish** | Skills + AGENTS.md discovery, quota scrapers per provider, cost UX |

Track A remains the dogfood path; Track B is planned first-class, not a hacked afterthought.

---

## Decision log (this doc)

| Topic | Decision |
|-------|----------|
| Product shape | Single orchestrator, dual execution backends |
| Native default | Headless CLI; tmux namespaced opt-in |
| API shape | In-tree runtime + official SDKs; opt-in / mixable |
| Run identity | One id end-to-end |
| Isolation | Always worktree; bwrap per-provider later (none on by default yet); fail-closed cleanup |
| Implement default | 1 impl + ≥2 reviewers |
| Arena finish | `winner` or `reconcile` then review |
| Roles/peer | v1 first-class |
| Ship | Draft PR, never merge; lease force only on swarm branches |
| Discovery | Skills + AGENTS.md like agent-browser |
| Dashboard | Nice, first-class TUI |
| Quota | Best-effort auto + manual; Claude statusline-shaped signals where available |
