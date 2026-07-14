# Swarm bus — agent-to-agent communication

**Status:** LEANING (product planning)  
**Not:** a Pi extension. First-class subsystem of spar.  
**Inspiration:** pi-messenger (file bus, presence, steer, reserves, crew waves) — improved for dual-backend + orchestrator ownership.

---

## Goals

1. Agents (and humans) can **coordinate** without sharing one chat context window.  
2. Same protocol for **native-cli**, **api-sdk**, **orchestrator**, and **human (TUI)**.  
3. **Workspace-scoped, keyed by `agent_id`** (W5); a run is an optional tag for
   grouping/filtering, not the address. Durable; streamable into the product TUI.  
4. Typed messages + optional task DAG — not only free-form chat.  
5. Orchestrator remains authority on spawn/ship/gates; bus is coordination, not anarchy.

---

## Layout

The bus is a single **workspace-level** store keyed by a **globally-unique** `agent_id`.
Run-slot role ids (`orchestrator`, `impl-1`, …) repeat across concurrent runs, so a run
slot's bus id is run-qualified to `run:slot` (`SPAR_AGENT_ID` carries this qualified id);
a bare agent (spawned from the Composer with `SPAR_AGENT_ID` but no run) keeps its own
already-unique id. Presence rows and inbox directories are keyed by this unique id, so a
bare agent and a run slot address each other directly by id. `run` is demoted to an
optional message/presence tag; run-scoped views (`--run <id>`) filter by it, but delivery
never does.

```text
.spar/bus/                # workspace bus (W5 canonical)
  agents.jsonl            # join / leave / heartbeat / status (each row optionally run-tagged)
  events.jsonl            # append-only: messages + activity (tail = live stream)
  inbox/<agent_id>/       # keyed by the unique id (run slots: `run:slot`, bare: own id)
    claimed/              # exactly-once drain lands here
  pending_ack/            # requires_ack redelivery records
  queue/<run>/<agent_id>.jsonl  # durable turn-boundary queue, partitioned per run
  reserves.json           # path claims by bare agents

.spar/runs/<run_id>/bus/  # run-scoped + back-compat mirror
  events.jsonl            # mirror of that run's tagged events (TODO(W5): remove once readers move)
  agents.jsonl            # mirror of that run's presence
  reserves.json           # path claims stay run-scoped (separate worktrees per slot)
  tasks/                  # optional DAG (big plan / crew-like waves)
    graph.json
    task-<id>.json
```

---

## Message envelope

```json
{
  "id": "msg_...",
  "ts": "RFC3339",
  "from": "agent_id | orchestrator | human",
  "to": "agent_id | broadcast | orchestrator | human",
  "kind": "chat|status|blocked|unblocked|contract|review_finding|task_claim|task_done|steer|ack|system",
  "body": "string",
  "run": "run_id | null",
  "refs": { "paths": [], "artifact": null, "task_id": null },
  "requires_ack": false,
  "meta": { "provider": "...", "backend": "native-cli|api-sdk" }
}
```

`run` is an optional grouping tag (null for bare traffic). A **broadcast** stays
origin-scoped: it reaches only agents sharing the message's `run` scope (a bare broadcast
reaches other bare agents, never run slots). **Directed** delivery is always explicit and
keys purely on the recipient's unique `agent_id` — a short `to` is qualified with the
sender's run, an already-qualified `run:slot` or bare id passes through, and there is **no
run-tag filter** on the drain. That is what lets a bare agent and a run slot message each
other across scopes.

---

## Delivery

| Target | Mechanism |
|--------|-----------|
| api-sdk | Immediate **steer** into agent loop (interrupt-after-tool or queue) |
| native-cli | Inbox + best-effort inject / include unread on next turn; optional namespaced tmux poke |
| orchestrator | Watcher on `events.jsonl` |
| human | TUI bus panel + optional notify on `@human` / stuck |

---

## Primitives (v1 set)

1. join / heartbeat / leave  
2. send / broadcast (+ ack)  
3. reserve / release paths  
4. feed (tail events)  
5. task graph + waves (for `--big` / structured plans)  
6. message budgets (`none` … `chatty`)

---

## vs pi-messenger

| | pi-messenger | swarm bus |
|--|--------------|-----------|
| Host | Pi extension | spar product |
| Scope | Often global + folder | **Run-scoped** default |
| Fleet | Pi subprocesses | **Heterogeneous** CLI + API |
| Orchestration | Optional crew self-mesh | **Strong orchestrator** + peer talk |
| Protocol | Tool actions + free text | **Typed kinds** + free chat |
| UI | Pi overlay | **Product TUI** control room |

---

## TUI integration

Bus is a first-class pane: presence list, unread, thread view, composer `@slot` / `@all` / `@human`. Activity feed merges tool activity when backends can emit it.
