# spar workspace + bus-delivery initiative — staged build plan

Feeds `/build-review-ship`. Each stage = one work item = one worktree + PR.
Two tracks (B = bus delivery, A = workspace/tmux-invisible) are largely independent;
they share the hook substrate (Stage 3). W5 (Stage 12) depends on everything and lands last.

Gates every stage must pass (there is no CI):
`cargo fmt` · `cargo clippy --all-targets -- -D warnings` · `cargo test`.
Scenario tests need an explicit `[[test]]` block in Cargo.toml or they silently never run.

---

## Stage 0 — Decisions + doc reconcile (docs only)
- Add W1–W5 to `DECISIONS.md` (matching `ID | Decision | Status` format); flip `X4` to DECIDED.
- Reconcile `docs/architecture-dual-backend.md`: O12's "tmux opt-in only" spirit preserved
  (dedicated `spar` socket never touches personal sessions) but tmux is no longer optional on
  the workspace path.
- **Accept:** W1–W5 rows present; X4 = DECIDED; dual-backend doc updated; no code changes.

## Stage 1 — Inbox exactly-once drain (B0)  ← blocking correctness bug
- `spar bus inbox <agent> --claim`: atomically `rename` each message into
  `inbox/<agent>/claimed/` (atomic on same fs), return the claimed set. Non-claim call still peeks.
- `ensure_bus` creates `claimed/`.
- **Accept:** two back-to-back `--claim` calls → first returns N, second returns 0; concurrent
  claimers never double-deliver. Unit test in `bus.rs`.

## Stage 2 — Bus integrity: append lock + budget TOCTOU + body cap (B5a)
- `append_jsonl` under an advisory file lock so record + `\n` can't tear (currently two syscalls).
- Fold `count_events` + `append` under the same lock → closes the budget TOCTOU (bus.rs:185).
- Cap `body` size; `bail` past the cap.
- **Accept:** M-thread hammer on `send()` → exactly M well-formed lines, none past budget. Test.

### Adapter delivery/presence capability matrix (verified 2026-07-13 via provider docs)
Each adapter declares a `DeliveryStrategy` + `PresenceSource`. The orchestrator calls the seam;
it never branches on provider inline (stays inside the orchestrator/adapter split).

| Adapter | Injection strategy | Presence source | Pane needed? |
|---|---|---|---|
| Claude Code | `StopHookInject` — Stop hook returns `{"decision":"block","reason":…}` / `additionalContext` | hooks: PreToolUse→working, Notification→blocked, Stop→idle | no (headless) |
| Grok | `NativeQueue` — push to `/queue` (queues even while working); or ACP `session/prompt` | `http`-push notification hooks: `turn_complete`/`approval_required` | no |
| opencode | `SdkPrompt` — plugin on `session.idle` → `client.session.prompt()`, or `serve`+`prompt_async` | SSE bus: `session.idle`/`tool.execute.*`/`permission.ask` | no |
| agy (Antigravity) | `None` — inbox-on-next-turn only (Stop is notify-only; PreToolUse `additionalContext` is tool-scoped, not idle) | none (degraded — process/output heuristic) | n/a |

Notes: Grok **reads `.claude/settings.json` hooks**, so one Claude-format hook file covers Claude
*and* Grok presence. agy specifics are medium-confidence (official docs are an unfetchable SPA) —
**Stage 3 task 1 must verify against the live `agy --help`/`agy hooks` binary before building.**

## Stage 3 — Presence + delivery capability seam (shared by A & B)
- Introduce an adapter capability seam: each adapter reports `DeliveryStrategy` + `PresenceSource`
  (see matrix). On slot spawn (`process.rs` + tmux path), wire the adapter's presence source:
  install Claude/Grok hooks into `.claude/settings.json`; register the opencode plugin / subscribe
  its SSE bus; agy → none, log degraded mode. Export `SPAR_AGENT_ID` to every agent.
- **Accept:** each available adapter drives `working`/`blocked`/`idle` transitions visible via
  `spar bus presence`; agy logs "no event stream — inbox-on-next-turn, degraded presence";
  the matrix's provider assumptions are re-verified against installed binaries (task 1).

## Stage 4 — Turn-boundary delivery, adapter-dispatched (B1)  ← closes X4
- `spar bus deliver <agent>`: `--claim` drain, then dispatch to the adapter's `DeliveryStrategy`:
  - `StopHookInject` (Claude): Stop hook emits the claimed messages as `block`+reason / `additionalContext`. Headless; no pane.
  - `NativeQueue` (Grok): push claimed messages to `/queue` (safe even mid-turn — Grok applies at the boundary).
  - `SdkPrompt` (opencode): `POST prompt_async` / `client.session.prompt` into the session.
  - `None` (agy): leave claimed messages for inbox-on-next-turn; agent reads them itself. Documented, no injection.
- No `send-keys` injection required by any adapter for delivery → Track B is fully decoupled from
  Track A's panes. (`send-keys` remains only Track A's interactive-typing path, Stage 10.)
- **Accept:** for each strategy, a message to an idle agent is consumed on its next turn exactly
  once; a message sent while the agent is `working` is applied at the next boundary, never mid-turn.
  Scenario test per strategy (dry-run stubs the injection call).

## Stage 5 — @human routing (TUI sink + pluggable notifier) + requires_ack (B2 + B3)
- **spar is a public app — no notifier is hardcoded.** `@human`/`Blocked` routing has two sinks:
  - **TUI (always on, ships baseline):** surfaces `to == "@human"` and `MsgKind::Blocked` as a
    first-class in-app panel/badge — works with zero config.
  - **External notifier (opt-in, operator-configured):** generic `[notify]` config — either a
    `command = "..."` spar shells out to (message passed on argv/stdin) or a `webhook = "..."` URL.
    The user wires their own sink (ntfy, Slack, a script). The operator's personal jimothy
    connection lives in their local spar config, never in the source.
- Implement `requires_ack`: redeliver-until-acked with backoff; escalate to `@human` after K retries;
  `MsgKind::Ack` clears. (Turns the currently-dead field real.)
- **Accept:** a Blocked/@human msg surfaces in the TUI with no config; with `[notify]` set, it also
  fires the configured command/webhook; an unacked `requires_ack` msg redelivers then escalates;
  an Ack stops redelivery. Tests + one manual notifier check (via a local echo-command config).

## Stage 6 — Reserve leases (B4)
- `Reserve` gains a lease tied to holder heartbeat TTL; `reserve()` reclaims expired claims;
  crash-release happens by TTL (no more hand-editing `reserves.json`).
- **Accept:** a claim whose holder's last heartbeat is older than TTL is reclaimable; a fresh claim
  still blocks. Test.

## Stage 7 — Loop prevention (B5b)
- Reply-depth cap / per-pair rate limit on `chat` so two agents can't ping-pong inside the message budget.
- **Accept:** A↔B past depth D is refused with a clear error; normal traffic unaffected. Test.

## Stage 8 — spar tmux socket + control-mode client (A1 / W1 / W2)
- All spar tmux ops move to `-L spar`. New control-mode client: spawn `tmux -L spar -C`, parse
  `%output` and pane-lifecycle events into an internal stream. (Dotfiles attach helper is a doc note, not repo code.)
- **Accept:** spar creates sessions only on the `spar` socket (personal `tmux ls` untouched);
  client yields output events for a scripted pane; unit test on `%output` frame parsing.

## Stage 9 — Embedded terminal widget (A2 / W3)
- Add `vt100` + `tui-term` deps; `vt100::Parser` consumes control-mode `%output`; render buffer
  as a `tui-term` widget; new focus target in `tui.rs` alongside Runs/Agents/Log/Activity/Composer.
- **Accept:** attaching to a live pane shows live-updating contents; resize handled; smoke test on a known sequence.

## Stage 10 — Key forwarding (A3)
- Focused terminal pane forwards keystrokes via `tmux -L spar send-keys` (respect the write-text-then-separate-Enter gotcha).
- **Accept:** typing in the embedded pane reaches the agent; Enter submits. Verified against a shell pane.

## Stage 11 — Spawn + dispatch from Composer (A4)
- Composer spawns an agent into a spar-socket pane and prompts it (write text, then Enter as a
  separate step). Bare agents get `SPAR_AGENT_ID` and join the bus. Ship the wait primitive that
  handles the false-idle race: wait for `working` **then** `idle`.
- **Accept:** from the TUI — spawn, prompt, watch, steer — no external tmux. Scenario test for the
  wait-for-working-then-idle primitive.

## Stage 12 — Workspace bus re-scope (W5)  ← last
- One `.spar/bus/` keyed by `agent_id`; `run` demoted to an optional message tag; run views filter by tag.
  Bare agents addressable exactly like run slots via `SPAR_AGENT_ID`.
- Reconcile the `.spar/runs/<id>/bus/` invariant in `CLAUDE.md` + `docs/architecture-a2a.md`.
- **Accept:** a bare agent and a run slot message each other; per-run filtering still works;
  invariant docs updated; migration note.

---

### Dependency / parallelism
- **0 → 1 → 2** first (decisions, then the bug, then integrity before adding traffic).
- **3** is the fork point: **4 → 5,6,7** (Track B) and **8 → 9 → 10 → 11** (Track A) run in parallel.
- **12** after both tracks.
- Design risk RESOLVED (research 2026-07-13): Claude Stop-hook injection confirmed; delivery is an
  adapter capability (see matrix). Track B has **no** dependency on Track A's panes. Only agy's
  specifics remain unverified — Stage 3 task 1 checks the live binary.

### Backlog / follow-on (out of scope here, captured from the research)
- **Structured backends beat TTY-scraping for two providers:** Grok `agent stdio` (ACP JSON-RPC,
  clean `session/prompt`→`stopReason`) and `opencode serve`+SSE (`GET /event`, `prompt_async`).
  Both are stronger `api-sdk`-backend paths than a tmux pane and could reduce Track A's surface for
  those adapters. Revisit as a backend-evolution item after this initiative; don't block on it.
- **Grok in-tool features to potentially exploit:** `/fork` (in-tool fan-out ≈ arena), native
  worktrees (`-w`/`--worktree` — reconcile with spar's worktree ownership), `GROK_HOME` per-slot
  config isolation, `--rules`/`--system-prompt-override` for injecting review policy.
