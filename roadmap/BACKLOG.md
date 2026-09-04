# Backlog

Unscheduled ideas, grouped by theme. Promote to `roadmap/features/NNN-*.md` when picked up.

## Remote / persistence architecture

- **Thin-client split (`spar --remote`)** — a local spar TUI talking to a remote spar
  orchestrator over spar's own protocol stream, instead of ssh-then-run-server-side.
  Would give attach/persistence without leaning on tmux, and native image paste falls out
  as a single message type (herdr's model). Big: it reopens the tmux-vs-own-protocol
  decision the workspace initiative settled (`DECISIONS.md` W1/W2). Tracked as **X7**;
  the additive local-companion image bridge (feature 002, decision W6) ships first and
  does not depend on this. Revisit after the workspace initiative lands.

## Run store hygiene

- **Widen `spar archive --all` to the ambiguous at-rest phases** — archiving (O36) ships
  with one predicate doing two jobs: `auto_archivable` (`done` / `plan_rejected`) gates
  both the automatic launch sweep *and* the explicit `--all`. Excluding gates from the
  automatic path is the point of the feature; excluding `failed` / `stuck` / `escalated` /
  `quota` / `stopped` from an *explicitly requested* `--all` was a silent narrowing of the
  scope this was approved under ("auto-archive on terminal phases past some age"). On
  payforge that leaves 27 of 76 rows archivable one id at a time, so the listing settles
  around 42 rather than somewhere tight.
  Preferred shape: keep the launch sweep on `auto_archivable`, and let explicit `--all`
  take anything `archivable_by_hand` (already exists, already excludes in-flight). Gates
  stay opt-in via an id or a flag. Touches `state.rs` + `main.rs`, so it must land *after*
  the cleanup-safety fix, which owns those files.
- **TUI has no archive affordance** — all three rail surfaces filter through
  `registry::list_visible_project_runs`, but there is no key to show archived runs, no
  archive/undo action and no hidden count, all of which the CLI has. On a TUI-first
  product the TUI can now hide runs it offers no way to recover.

## Slot worktree cost

Measured across `/data/projects` on 2026-08-17, after a disk-full incident: 457 GB of
587 GB was `target/` + `node_modules`. `spar reclaim` / `auto_reclaim` (O37) now reclaims
that for finished runs. These two reduce how much gets created in the first place.

- **Collapse `test_author` and `impl` onto one worktree** — they never run concurrently
  (test_author is dispatched in the plan phase, impl in the implement phase) and compile
  the same crate graph, so each run carries the same dependency build twice: 5-6 GB per
  run, and 5 of the 9 `target/` dirs found on the box belonged to test_author slots.
  The tester slot already does exactly this correctly, reusing the implementer's cwd
  (`implement.rs`, `s.cwd = Some(review_cwd.clone())`).
  **Not a directory change — a branch identity change**, which is why it was not folded
  into O37:
  - Sharing a worktree shares its branch, so implementer commits land on
    `spar/<run>/test-author-*`. That changes what `ship` pushes and what
    `merged_into_base` reasons over, and **O26 ties the no-rebase rule specifically to
    the author worktree being reused and overlaid**.
  - `apply_spec_tests_to_impl` becomes a no-op, including the git-visible-only
    enumeration and the ignored-file notice added in O30. That needs re-deciding, not
    deleting.
  - Must be **conditional on workflow**: arena dispatches N implementers that genuinely
    do run concurrently (`arena.rs`), so an unconditional collapse puts concurrent
    implementers in one tree — the isolation regression this is supposed to avoid.
- **Non-building roles do not need a build-capable worktree** — planner, plan_critic and
  reviewers emit markdown and produced no `target/` in any tree on the box, ever.
  Lower payoff than it looks, and partly already done:
  - In the **loop** workflow reviewers already share the implementer's cwd
    (`implement.rs`, "Only isolate the implementer; reviewers share its cwd") — only the
    standalone `review` workflow cuts one per reviewer (`review.rs`).
  - What remains is planner + plan_critic + standalone-review reviewers at ~40 MB each:
    tens of MB, so the value is preventing a future accidental compile, not reclaiming.
  - Not free: dropping the worktree makes cwd fall back to `project_root`, which is the
    `isolation = "none"` shape that artifact-recovery had to be guarded against in O31.
    A shared read-only reviewer tree has to stay read-only against concurrent reviewers.

## Telemetry

- **`cli:grok` token accounting is partial.** spar spawns grok with
  `--output-format streaming-json` (`src/providers/grok.rs`), which grok's own help calls
  the native ACP session-update stream; the Anthropic-wire option is
  `streaming-messages-json` and spar does not request it. The consequences, measured
  against grok's own `~/.grok/sessions/<cwd>/<id>/updates.jsonl` (its `turn_completed`
  update is ground truth) rather than guessed:
  - No grok log on the box carries a `· session`, `· done` or `· turn` marker, so grok
    hits neither claude's `result` arm nor codex's `turn.completed`. It never gets a
    terminal usage record and is scored entirely on the per-request path.
  - Exact today: `cache_read` (2,853,504) and `input_tokens` as the uncached remainder
    (124,866 = 2,978,370 - 2,853,504), both because grok emits them cumulatively and
    `max` lands on the final value. The tool *count* is also exact (83 = 83 `tool_call`
    updates).
  - Wrong today: `output_tokens` read 61,292 against a real 30,646, a cumulative value
    counted twice. O48 removed the duplicate `absorb_usage` calls, which is the likely
    fix, but it is **unverified**: grok is out of quota and was deliberately not probed.
    The mechanism is identified and fits every observation: `feed` absorbed usage from
    every parsed line (`src/process.rs:732`), and `handle_claude_assistant` then absorbed
    a second time via `v.pointer("/message").unwrap_or(v)` — on a line with no `/message`
    the fallback re-absorbs the *same* value, which is exactly 2x and nothing else. It
    also explains the rest of grok's symptoms together: `v.pointer("/message/content")?`
    then returns `None`, so the arm bails before printing, which is why grok logs carry no
    marker, and `/message/model` never resolves, which is why `model` is always `None`.
    Measured on four matched slot/session pairs, the output ratio is 2.00 in all four and
    `input + cache_read` reconstructs grok's `inputTokens` exactly in all four. Confirming
    it needs one live capture showing grok's stdout carries top-level `type: "assistant"`.
  - Still wrong after O48 regardless: `context_tokens` for grok is a cumulative total, not
    a peak (grok's own `modelCalls: 31` says the real window is far smaller), so the
    context gauge is meaningless for grok slots; `model` is never captured; `tool_errors`
    is structurally always zero; tool names never resolve (every line is a bare `→ tool`).
  - The fix is a real grok/ACP branch in `StreamCoalescer::feed` reading
    `params.update.sessionUpdate` and the camelCase `turn_completed` usage
    (`inputTokens` / `outputTokens` / `cachedReadTokens` / `cacheCreationTokens`, with
    `reasoningTokens` already inside `outputTokens`). Needs one live grok run to confirm
    the stdout shape, which is **not** byte-identical to the persisted envelope, since spar
    matches `cache_read` today so stdout evidently uses different keys from the stored
    JSON-RPC form. Do not rewrite the parser without that capture.

- **opencode `task` subagent spend is structurally invisible.** opencode's json emitter
  filters child sessions out of the stream it prints: the one `process.stdout.write` call
  site sits behind `if (A.sessionID !== e) continue`, so a subagent's `step_finish` never
  reaches `handle_opencode` and never reaches `billed_tokens`. Unlike muse, which walks
  `subagent/*/session.jsonl` post-exit, there is no recovery pass. Measured against
  `opencode.db` (`session.parent_id`, summing
  `tokens_input + output + reasoning + cache_read + cache_write`): the top parent by child
  spend books 1,384,565 against 6,605,838 across 4 children (4.8x), the next 1,234,061
  against 6,106,099 (4.9x), and one 197,553 against 3,226,622 (16.3x). Latent under spar
  today only because no spar role prompt tells an opencode slot to fan out. **The O48
  verification cannot detect this**: it reconciles a slot against the provider's ledger for
  the session spar named, so spend booked to a child session is out of frame by
  construction. The fix is a post-exit pass in the shape of `muse_telemetry::collect`,
  walking `session.parent_id` in `opencode.db` from the `sessionID` spar already records.

- **`worktree+bwrap` cannot write artifacts or markers.** `src/sandbox/bwrap.rs` binds `/`
  read-only and makes only the slot's `cwd` writable, but `artifacts_dir` and `markers_dir`
  both live under `.spar/runs/<id>/`, outside the worktree. Under that isolation mode a
  slot cannot write `summary-<slot>.md`, its `.done`/`.failed` marker, its build logs, or a
  carry-forward brief, so every dispatch fails the artifact gate and every terminal verdict
  has to be inferred. **Pre-existing on `main`, not introduced by the telemetry work**, and
  latent because `Worktree` is the default and no project config on this box selects bwrap.
  Fix sketch: `--bind` the run directory (`.spar/runs/<id>/`) read-write alongside `cwd`,
  and add a scenario that runs one dispatch under `worktree+bwrap` end to end — the mode is
  broken end to end today and nothing tests it.

- **Two adapters still have no live token reading (O50).** The soft budget nudges on
  `billed_tokens` read from the live sidecar, and two adapters cannot fill it mid-dispatch:
  - **codex** reports usage only in `turn.completed`, its single terminal record, so a
    codex slot gets a time nudge and never a token nudge. Fixing it means finding a
    per-request usage record in `codex exec --json` that spar does not currently parse, or
    accepting the gap.
  - **claude** reports per-message usage, but `absorb_usage`'s Request arm `max`es the input
    and cache-read components (correct, because providers disagree on delta vs running
    total) until the terminal `result` supersedes them. So a live claude reading is a lower
    bound and its token nudge fires late, never early. Settling it means learning whether
    claude's per-message `input_tokens` / `cache_read_input_tokens` are per-call or
    cumulative, from one instrumented run, not from the shape of the JSON.

- **muse's turn-boundary socket is still unwired.** muse ships
  `session-message send|serve` over a unix socket, which is a real inject channel into a
  running session. O50 gave muse `DeliveryStrategy::PollFile` instead, which only lands at
  the agent's next major step. Wiring the socket would make muse first-class for delivery
  and would let a nudge interrupt an in-progress turn rather than waiting for one.

- **A rate-limited slot fails the run instead of parking it on the quota gate.** Observed
  dogfooding on 2026-09-04: two concurrent runs (`bf7770ae`, `abd35a54`) both died with
  `slot impl failed: exit 1` and `phase = failed`, when the cause was in the slot log as
  `! rate limit  seven_day  rejected` / "You've hit your weekly limit · resets 12am".
  Detection is not the problem: `QuotaStore::scrape_log_hint` is wired in at
  `executor.rs:1226` and `"rate limit"` is one of its needles, so the provider *was*
  paused. The problem is the three lines after it — `run_slot` pauses the provider and
  then `bail!`s anyway, so the run is classified as a failure. `Phase::Quota` is only ever
  set in `workflow/plan.rs:59`, the *pre-dispatch* check for an already-paused provider;
  nothing maps a slot that dies mid-dispatch from a rate limit onto it. Three
  consequences:
  - The exit code is `1` where the contract says a quota stop is `4`, so an outer agent
    cannot tell "your code failed" from "your account is out of tokens until the window
    rolls" — the one distinction exit code 4 exists to make.
  - **The run becomes unrecoverable.** `Phase::Failed` is terminal, so `spar stop` refuses
    ("already at Failed; nothing to stop") and `spar implement --run <id>` refuses ("plan
    is not approved (phase=Failed)"). The only way back is `--new`, which forks a second
    run for work that is already a run and throws away the frozen contract and the round
    history. Verified on `bf7770ae`.
  - The pause auto-recovers on the generic ~30-minute timer, which is right for a
    five-hour window and wrong for a weekly one: the provider is re-probed and the next
    dispatch walks into the same wall, for days. Claude states the reset in the same line
    ("resets 12am (America/New_York)"); `scrape_claude_rate_limits` already knows how to
    carry a `cooldown_until`, but only parses the `rate_limits` / `five_hour` JSON shape,
    not this plain-text form.

  Fix is to route a quota-detected slot failure onto `Phase::Quota` with exit `4`, let
  `implement --run` re-enter from it the way it does from `Stopped`, and parse the stated
  reset into the cooldown instead of falling back to the generic timer. Worth checking the
  other adapters for the same gap at the same time.
