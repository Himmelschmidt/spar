# Swarm bus — agent-to-agent communication

**Status:** LEANING (product planning)  
**Not:** a Pi extension. First-class subsystem of agent-swarm.  
**Inspiration:** pi-messenger (file bus, presence, steer, reserves, crew waves) — improved for dual-backend + orchestrator ownership.

---

## Goals

1. Agents (and humans) can **coordinate** without sharing one chat context window.  
2. Same protocol for **native-cli**, **api-sdk**, **orchestrator**, and **human (TUI)**.  
3. Run-scoped by default; durable; streamable into the product TUI.  
4. Typed messages + optional task DAG — not only free-form chat.  
5. Orchestrator remains authority on spawn/ship/gates; bus is coordination, not anarchy.

---

## Layout

```text
.swarm/runs/<run_id>/
  bus/
    agents.jsonl       # join / leave / heartbeat / status
    events.jsonl       # append-only: messages + activity (tail = live stream)
    inbox/<slot_id>/   # unread pointers or copies
    reserves.json      # path claims
    tasks/             # optional DAG (big plan / crew-like waves)
      graph.json
      task-<id>.json
```

---

## Message envelope

```json
{
  "id": "msg_...",
  "ts": "RFC3339",
  "from": "slot_id | orchestrator | human",
  "to": "slot_id | broadcast | orchestrator | human",
  "kind": "chat|status|blocked|unblocked|contract|review_finding|task_claim|task_done|steer|ack|system",
  "body": "string",
  "refs": { "paths": [], "artifact": null, "task_id": null },
  "requires_ack": false,
  "meta": { "provider": "...", "backend": "native-cli|api-sdk" }
}
```

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
| Host | Pi extension | agent-swarm product |
| Scope | Often global + folder | **Run-scoped** default |
| Fleet | Pi subprocesses | **Heterogeneous** CLI + API |
| Orchestration | Optional crew self-mesh | **Strong orchestrator** + peer talk |
| Protocol | Tool actions + free text | **Typed kinds** + free chat |
| UI | Pi overlay | **Product TUI** control room |

---

## TUI integration

Bus is a first-class pane: presence list, unread, thread view, composer `@slot` / `@all` / `@human`. Activity feed merges tool activity when backends can emit it.
