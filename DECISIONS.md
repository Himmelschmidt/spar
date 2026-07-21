# DECISIONS

Project-level product and architecture decisions. Status: `OPEN` | `LEANING` | `DECIDED`.

## Product

| ID | Decision | Status |
|----|----------|--------|
| P0 | Product name is **spar** (binary `spar`) | DECIDED |
| P1 | spar is a **first-class agent product** (TUI-first), not a Pi/Claude plugin | DECIDED |
| P2 | Default human surface is a **nice fleet TUI** (Claude/Grok/agy class); CLI/JSON for outer agents | DECIDED |
| P3 | Discovery via **built-in skills + AGENTS.md** (agent-browser pattern) | DECIDED |
| P4 | Dual execution: **native-cli** + **api-sdk**, one orchestrator | DECIDED |
| P5 | First-class **swarm bus** (A2A), not dumb mailbox only | DECIDED |
| P6 | **Dynamic model select** (vals benchmarks + prefs/urgency) is a first-class product path alongside explicit `--providers` | DECIDED — see MS* |

## Orchestration

| ID | Decision | Status |
|----|----------|--------|
| O1 | **One run id** plan → implement → ship | DECIDED |
| O2 | Always **worktrees** for coding slots | DECIDED |
| O3 | Cleanup **auto when fully done**, fail-closed if still needed | DECIDED |
| O4 | bwrap **per-provider optional**; none enabled by default yet | DECIDED |
| O5 | Default implement: **1 impl + ≥2 reviewers**; multi-impl in **arena** | DECIDED |
| O6 | Arena finish: **winner** or **reconcile** then review | DECIDED |
| O7 | Roles + peer are **v1 first-class** | DECIDED |
| O8 | Plan: multi-provider + critic; **structured/big** for large features | DECIDED |
| O9 | Gates **configurable**; lean high autonomy | DECIDED |
| O10 | Ship: **draft PR**, never merge; force-with-lease only swarm branches | DECIDED |
| O11 | Stream **everything** (logs/events) | DECIDED |
| O12 | Headless default; tmux **namespaced opt-in only** | DECIDED |
| O13 | Quota: **best-effort auto** + manual; Claude rate_limits-shaped signals | DECIDED |
| O14 | **Suite channel** (implement/loop): dedicated cheap `tester` slot runs full suites; impl/review smoke/diff-only when suite ran; long `suite.timeout_secs`; salvage partial review/suite artifacts on timeout; fail closed if enabled but no tester/provider. Independent `review` workflow may still run its own tests (no suite slot by default) | DECIDED |
| O15 | **Spec / test-author** (plan flow): after planner+critic write `plan.md`, a separate `TestAuthor` slot freezes acceptance tests **before** coding and before the plan gate. Not planner, not critic, not suite `tester`. **Artifact-first** (plan + critique); bus is audit trail (no live multi-turn with finished planner/critic). Outputs `artifacts/test-contract.md` + tests in worktree; implement **always overlays** author working tree into impl (plus best-effort branch merge; abort on conflict). Fail closed if author fails, contract missing, or apply fails when TestAuthor ran. Config `[spec]` (`enabled` default true, optional `provider`, `timeout_secs`). Human gate is plan + contract | DECIDED |
| O16 | **Clean exit ≠ success.** A slot's `expected_artifact` must exist and be non-empty or the slot is `Failed` (salvage its log tail), on the parallel path as well as the sequential one. A reviewer that exits 0 without producing a review no longer counts as a review | DECIDED |
| O17 | **Stall = log-quiet + (process gone OR past role budget).** The `bus` heartbeat is process-liveness, not progress (a live child beats every ~30s regardless of work), so gating `stalled` on the heartbeat alone would disable the wedged-alive alarm for the headless default. Dual threshold: a running slot is `stalled` when log-quiet past `stall_warn_secs` AND either its heartbeat is also quiet (dead/gone) OR it has been log-quiet for its whole role timeout (alive but hung). Busy-but-log-quiet agents (streaming-json mid tool-call) inside budget are not stalled. `stalled` is advisory only; a true hang still hits the role timeout → `Phase::Stuck` → exit 3 | DECIDED |
| O18 | **`cli:codex` adapter** (codex `exec --json`). `DeliveryStrategy::None` + `PresenceSource::None`, no takeover, no resume. FullAuto uses `--dangerously-bypass-approvals-and-sandbox` (unsandboxed like the other adapters; the worktree is the boundary) + `--skip-git-repo-check`. codex JSONL works: the stream coalescer parses `item.completed` (agent_message/reasoning/tool items) and `turn.completed.usage` (`input_tokens`/`cached_input_tokens`/`output_tokens`) for **real token tracking**. **Backend + model** come from a codex *profile* (its own (backend, model) bundle) since spar's provider ref can't carry a model and there's no inline `--model` flag; selection mirrors `api:` env style — `--select` model → `SPAR_CODEX_MODEL` (`-m`) → default; `SPAR_CODEX_PROFILE` picks `-p` (unset → `muse` = OpenRouter+Muse Spark; empty → omit `-p` for codex's own default; else that profile). OpenRouter profiles need `OPENROUTER_API_KEY` exported (it is `set -Ux`; codex reads it via `env_key`). Rejected per-slot model-in-ref (needs provider-ref/model rework — MS14-D/X6, deferred) and a `[codex]` config block (env suffices now; add later if per-project persistence is wanted). Opt-in only — **not** in `default_provider_order`. (kilo adapter was prototyped and dropped: kilo 7.4.11 `--format json` is broken — no stdout, hangs — so no token tracking was possible) | DECIDED |
| O19 | **Unmet acceptance criterion ≠ success.** Reviewers emit `## Acceptance` with one `AC-n: pass\|fail\|unverified — evidence` line per criterion in `test-contract.md`. Any `fail`, or any contract `AC-n` absent from the review, forces `request_changes`. `unverified` blocks by default; relax with `[review].require_all_criteria = false`. Extends O16 | DECIDED |
| O20 | **`## Verdict` is an anchored header, not a substring.** The first non-blank line under the first `## Verdict` section must be `approve` or `request_changes`; missing or unparseable ⇒ `request_changes` (fail closed). Replaces the whole-document scan that made the format block's own `approve \| request_changes` line self-blocking. One parser (`workflow/review_result.rs`) serves both implement and arena | DECIDED |
| O21 | **Reviewers see the plan and the contract.** `plan.md` and `test-contract.md` bodies are passed to the reviewer template, not summarized as a prose nudge. A reviewer that cannot see the criteria cannot verify them | DECIDED |
| O22 | **`[roles]` assigns providers by role, not position.** A top-level `[roles]` block maps `planner`/`plan_critic`/`implementer`/`reviewer` (a list)/`tester`/`test_author` to `@model`-capable provider refs, validated through `ProviderRef::parse` at config load (errors name the offending role key, so the `.expect()` in `init_slot_model` stays unreachable from user input). The old positional `[suite].provider` / `[spec].provider` are **removed** (clean break, no shim): `tester` and `test_author` replace them. Priority 8 lands the data model + plumbing; Priority 9 rewires the fleet to consume it. Distinct from `[model_select.role_profiles]`, which maps a role to a benchmark *profile*, not a provider | DECIDED |
| O23 | **One role vocabulary = `SlotRole` snake_case.** `SlotRole::as_config_key()`/`from_config_key()` are the single source of truth (an inline test asserts they agree with the serde `rename_all = "snake_case"` representation for every variant). `critic` is retired for `plan_critic` and the plan flow's overloaded `tester` label becomes `test_author`, freeing `tester` to mean only the cheap suite runner. No aliases — accepting `critic` is how two vocabularies become three. `[model_select.roles]` is renamed to `[model_select.role_profiles]` so the top-level `[roles]` (providers) and the model-select role→profile map no longer collide | DECIDED |
| O25 | **`cli:opencode` adapter, OpenRouter-default.** New native-cli adapter (`opencode run --format json --dangerously-skip-permissions [-m …] -- <prompt>`). Model routing prepends `openrouter/` to a bare vendor slug (`cli:opencode@meta/muse-spark-1.1` → `-m openrouter/meta/muse-spark-1.1`), passes an explicit `openrouter/…` or a bare word through unchanged — so OpenRouter is the **default** coder route. Token tracking parses opencode's per-step NDJSON, deduping the dash/underscore double-emit by `(normalized type, part.id)` so `step_finish` output is not counted twice (input=max, output=sum, cache=max). v1 is agy-tier (`DeliveryStrategy::None` / `PresenceSource::None`); opencode's SSE event stream + `session.prompt()` could make it first-class (`SdkPrompt` / `Sse`) later — reserved, not wired. **opencode is the recommended OpenRouter coder over `cli:codex`** (~half the per-turn token overhead: ~14.6k vs ~29.5k input on the same trivial task + model); codex stays the alternative | DECIDED |
| O24 | **Role resolution precedence + one resolver.** Fleet assignment runs through a single `workflow::roles_resolve::provider_for(role, idx, fleet, cfg)` with fixed precedence **explicit `--providers` (positional one-off) > `[roles]` > `[providers].order`**. A populated `[roles]` synthesizes the fleet in `model_select::resolve_providers`, so it *satisfies* the "`--providers` or `--select` is required" invariant (consolidated to that one choke point; the redundant `implement.rs` guard and dead `CommonOpts::require_providers` are deleted). `[roles].reviewer` is a list indexed per reviewer; reviewer **widening** and implementer **rotation** draw candidates from `[roles]` then `[providers].order` then the live fleet instead of a hardcoded provider list. `tester`/`test_author` keep their dedicated resolvers (`[roles]` head-of-chain with a usability check, falling through when unusable) | DECIDED |

## Model select (vals-backed)

Opt-in path: resolve fleet slots from benchmark data + user profiles + per-run urgency, instead of only manual `--providers`. Explicit `--providers` remains valid and default-required until select is opted in.

| ID | Decision | Status |
|----|----------|--------|
| MS0 | Data source: **vals.ai** coding benches; abstract behind `BenchmarkSource` so an official API can replace HTML scrape later | DECIDED |
| MS1 | Per-model fields used: **accuracy**, **latency**, **cost_per_test** (from vals payload) | DECIDED |
| MS2 | Phase A primary bench: **SWE-bench Verified** only; later optional blend (Terminal-Bench, LiveCodeBench, Vibe Code, Vals Index) | DECIDED |
| MS3 | Selection is **opt-in**: keep `--providers` required unless `--select <profile>` and/or `[model_select]` enables auto | DECIDED |
| MS4 | Named **profiles** in config (`best` / `value` / `fast` or custom) with weights for quality / cost / speed + optional `min_accuracy` | DECIDED |
| MS5 | **Urgency** is a per-run multiplier on the chosen profile (high → speed↑ cost↓; low → cost↑), not a separate parallel system | DECIDED |
| MS6 | Score: normalize metrics in candidate set; `score = w_q·acc − w_c·cost − w_s·latency`; apply floors/allowlists before rank | DECIDED |
| MS7 | **CLI economics**: treat `cli:*` cost as **0** for scoring (flat sub); do not use vals $ against subscription CLIs — superseded by MS15 for `@model` refs | DECIDED |
| MS8 | Resolve to **assignable slots** (`ProviderRef` + optional model), not abstract model names only; filter by doctor-available backends | DECIDED |
| MS9 | Multi-slot fleets (plan/arena/review ≥2): prefer **provider-family diversity**, then next-best score | DECIDED |
| MS10 | Always write **`artifacts/model-select.json`** (candidates, weights, urgency, winners, why) into the run | DECIDED |
| MS11 | Cache: `~/.spar/cache/vals/…` + TTL; `spar model refresh`; stale cache usable within grace window; fail closed with clear error if no cache and fetch fails | DECIDED |
| MS12 | Role defaults in config (`planner`/`implementer`/`reviewer`/`tester`/`critic` → profile names); suite `tester` defaults toward **fast/value** | DECIDED |
| MS13 | CLI surface: `spar model list|pick|refresh`; doctor reports cache age | DECIDED |
| MS14 | Ship phases: **A** cache+parser+list/pick · **B** `--select` into plan/implement · **C** roles/urgency/diversity · **D** per-adapter CLI model flags + richer vals→spar map | DECIDED |
| MS15 | **Revises MS7 for `@model` refs.** `cli:*` is zero-cost only when the adapter bills against its own subscription. `cli:codex@<openrouter-slug>` is a `cli:` ref that routes to OpenRouter and bills **per token** (observed: 29.5k input on a trivial task; 489k on a real review run), so cost scoring must key on the **resolved model**, not the backend prefix. Recording the decision here; joining `turn.completed.usage` (already parsed into `stats.json`) to the OpenRouter price table is follow-up work | DECIDED |
| MS16 | **`spar model list --provider openrouter` filters to `supported_parameters` containing `tools` by default** (268/339 models); the remaining 71 cannot function as agents and are hidden unless `--all` is passed. Guardrail, not a preference | DECIDED |

## Workspace + bus delivery

Workspace initiative (2026-07-13). See `roadmap/workspace-initiative-plan.md` for the staged build.

| ID | Decision | Status |
|----|----------|--------|
| W1 | spar owns its **own tmux server socket** (`tmux -L spar`); the user's personal tmux sessions are never touched | DECIDED |
| W2 | Live pane output via tmux **control mode** (`tmux -C` `%output` stream), not `capture-pane` polling | DECIDED |
| W3 | Embedded terminal rendering via `vt100::Parser` + `tui-term` as a new TUI focus target | DECIDED |
| W4 | "Own coding UI over direct API" **deferred** — use opencode as the harness for API-only providers for now | DECIDED (deferred) |
| W5 | Workspace bus keyed by `agent_id`; `run_id` demoted to an optional message tag; `SPAR_AGENT_ID` gives bare agents identity. Lands last | DECIDED |
| W7 | Terminal panel is a **real embedded workspace shell** (persistent `$SHELL` on the spar socket, rooted at the project — run dev servers etc., survives TUI restarts) by default; agent panes reachable by cycling (Ctrl+PgUp/PgDn). Extends W3 | DECIDED |
| W8 | Embedded Terminal is a **real tmux client via PTY passthrough** (`portable-pty` runs `tmux -L spar attach`; raw keys/mouse/paste forwarded, output rendered): full tmux — prefix, copy-mode/scroll, paste, splits, search. `F12` returns focus to spar. Supersedes the W7 custom pane-switcher. Extends W3/W7 | DECIDED |
| W9 | **Agent takeover** in the Terminal: selecting a slot in the Agents pane attaches the passthrough terminal to that run's `spar-<run_id>:<slot>` pane so you can view + drive the agent directly. Requires the run to have used `--backend tmux` (headless has no pane); `Ctrl+b d` / session-end hands focus back to spar. Extends W8 | DECIDED |
| B1 | `@human` routing = always-on TUI sink (baseline, zero config) + opt-in generic `[notify]` command/webhook; **no hardcoded personal integrations** in source | DECIDED |
| Z1 | **On-disk markers are ground truth for slot status at the read boundary.** `status` / `list_runs` / TUI reconcile a `running` slot that has a `<slot>.done` / `.failed` marker to that verdict (`state::reconcile_slot_status`). In memory only — observe-only commands never rewrite `state.json` | DECIDED |
| Z2 | **"Abandoned" is a computed read-time flag, not an exit code.** A run in a non-terminal, non-gate, non-`Stopped` phase with no live `RunLock` owner is abandoned (`state::is_abandoned`); surfaced in `status`, `status --json` (`"abandoned"`), and the TUI. The `0/1/2/3/4` exit-code contract is untouched | DECIDED |
| Z3 | **`spar cleanup` reaps by cwd, never by command line.** Processes are matched via `/proc/<pid>/cwd` inside the run's own worktrees (self + ancestors excluded), SIGTERM → grace → SIGKILL, then the worktree is removed. Matching on command line self-matches the caller's shell | DECIDED |
| W6 | **Image paste over SSH = local-companion bridge (option A).** A local `spar clip` reads the OS clipboard image (`arboard` / `wl-paste` / `pbpaste`), ships raw bytes to the remote spar over the existing SSH connection (ControlMaster-exec or a forwarded socket); the remote stages a `0600` temp file and injects its **path** into the agent pane via Track A `send-keys` (agents accept image file paths). Additive; fits the ssh-then-run + `tmux -L spar` model. Thin-client alternative deferred — see X7 | DECIDED — backlog feature 002 |

## TUI

TUI restructure (2026-07-14). See `roadmap/tui-redesign-plan.md` for the staged build.

| ID | Decision | Status |
|----|----------|--------|
| U1 | TUI is a **rail + one main area** (content = f(selection × tab)), not N co-equal Tab-cycled panels. Rail is a `projects ▸ runs ▸ agents` drill-down (`Enter` pushes, `Esc` pops, breadcrumb in a single status line); Log/Activity/Diff/Shell are **tabs over Main**. Focus targets cut 6 → 3 (Rail/Main/Composer) with direct keys `1/2/3`. Chrome cut from 10 rows to 2. Researched against k9s / lazygit / lazydocker / herdr / claude-squad, which all use this shape; an N-way focus ring destroys spatial memory | DECIDED |
| U2 | The embedded terminal is a **mode, not a panel** — it lives in Main's Shell tab (Stage A) and becomes a full-screen Driving mode (Stage B). Escape is a prefix (`C-a`) + `F12`, never `Esc`/`Tab` (the agent needs those — Shift+Tab is Claude Code's permission toggle) | DECIDED |
| U3 | Stage B: the `:` **command palette replaces the Composer focus target** — focus is 2-wide (Rail/Main, keys `1`/`2`); the palette is a transient overlay, not in the ring. Its verbs are the run-lifecycle actions the orchestrator already brokers (`approve`/`reject`/`ship`/`confirm`/`reconcile`/`takeover`/`spawn`/`chat`) plus `implement`/`plan`, which spawn the detached CLI **reusing the selected run's recorded `providers`** — a fresh fleet needs a provider picker a text palette can't offer, so those error to the CLI. All complete run ids from the roster | DECIDED |
| U4 | Stage B: **`q` is the quit path**; double-`Ctrl+C` retired so `Ctrl+C` is unambiguously the agent's SIGINT in the Shell tab. `/` filters the rail (dim-in-place + match-only navigation; hiding rows would desync the selection index that feeds the snapshot). Diff renders the selected slot's real `git diff HEAD`, capped, falling back to artifacts when a slot has no worktree | DECIDED |
| U5 | Stage C: **attention model.** Each run gets a cheap `Attention` level from its summary (`Gate > Broken > Working > Idle`); the rail is **attention-sorted** (loudest first, then recency) with selection **glued to the run id** across re-sorts. A run that wants the operator flies a `⚑` and rolls up to its project row and a status-line `⚑N need you` count. **`a` jumps to the next run that needs you** (approve loses its one-key shortcut — tap the button or `:approve`); tapping the roll-up does the same. A run crossing into Gate/Broken **toasts** (first snapshot primes silently). Width bands: `<80` Main-only · `80–119` rail+Main · `>=120` the extra width goes to Main, never a 4th box | DECIDED |

## Open

| ID | Topic | Status |
|----|-------|--------|
| X1 | First API provider to spike (xAI / Anthropic / OpenAI) | DECIDED — OpenAI-compatible (`api:openai`, `api:xai`) via ureq |
| X2 | TUI keymap / layout (mimic which product most?) | DECIDED — rail + one main area, k9s/lazygit shape; see U1/U2 |
| X3 | Project template overrides day one vs later | OPEN |
| X4 | Bus steer reliability for native-cli headless | DECIDED — adapter-dispatched turn-boundary delivery (Stop-hook inject / native queue / SDK prompt / inbox-on-next-turn per adapter) |
| X5 | vals scrape parser brittleness / grace TTL days / ship in-repo snapshot | OPEN — decide at MS phase A impl |
| X6 | Exact vals model id → `cli:`/`api:` + model string mapping table | OPEN — phase A/B |
| X7 | **Thin-client split** (`spar --remote`: local TUI ↔ remote orchestrator over spar's own protocol stream) as an alternative to ssh-then-run — would make image paste fall out as one message type (herdr's model) and give attach/persistence without tmux. Reopens the tmux-vs-own-protocol call (W1/W2) | OPEN — backlog; revisit after the workspace initiative lands |
| X8 | ProviderRef `@model` split + precedence | DECIDED — split on **first** `@` before the colon check (model may carry `:`/`/`, name may not); `display()` round-trips with `@model`, `storage_key()` strips it (quota bucket + slot.provider stay model-free so `@opus`/`@haiku` share one bucket); slot.model = `pref.model.or(explicit)` so an explicit `@model` beats a `--select` choice |
