---
name: spar
description: Drive spar, the multi-agent coding orchestrator, from this session. Use when the user wants work planned, implemented or reviewed by a fleet of agents, asks about spar runs (status, findings, cost, providers, logs), or names a spar run id.
---

# spar

spar runs fleets of coding agents in their own git worktrees: `plan` (planner, critic,
test contract) -> operator approves -> `implement` (build and review rounds) -> operator
ships a draft PR. `review` is a standalone multi-provider review. One run id threads the
whole unit of work.

## Tools

- Look before acting: `spar_status` (runs, or one run), `spar_brief` (findings, events,
  the exact next command), `spar_events`, `spar_logs`, `spar_bus_log`,
  `spar_providers`, `spar_stats`.
- Launch: `spar_plan`, `spar_implement` (a fresh task, or `run` to continue an approved
  plan), `spar_review`. Launches spend money; the operator confirms each one. Say which
  providers or roles you intend. If they decline, ask what to change; do not retry.
- Follow: `spar_wait` blocks up to `timeout_minutes`. A timeout means still working.
- Steer: `spar_bus_send` to a run's agents, e.g. `to: "impl"` with `run`.

## Gates belong to the operator

Approve, reject, ship, confirm, reconcile, round extensions, rulings, reassign, amend,
stop, resume and cleanup are not tools, and running them through bash asks the operator.
When a run waits at a gate (exit 2), tell the operator what is waiting and give them
`next_command` from `spar_brief`; they clear it from `/spar`. Launch through the tools,
not bash: the tools validate and show the operator the fleet and the gates. Exit codes: 0 ok, 1 failed or
stopped, 2 human gate, 3 stuck, 4 no usable providers.

## Everything else

The full CLI surface (flags, workflows, config, fleet shaping, exit codes) is spar's own
manual, which always matches the installed binary. Read it with `spar_skill`: no
argument for the overview and section list, `section` for one section. Do not guess
flags from memory.
