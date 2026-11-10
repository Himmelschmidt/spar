# Agent operator contract

How an **outer agent** (Claude, Grok, agy, etc.) should call `spar`.

Also: `spar skills get core` (preferred; always current).

## Principles

1. Prefer `--json` and parse stdout. Both `run_id` and `id` are present on emit JSON.
2. **Subscribe, don't poll:** to wait on a run, `--detach` to launch it, then block on `spar wait <id> --follow --json` — it returns the instant the run hits a terminal state *or* a human gate (exit `2`), so you're woken exactly when there's something to do instead of having to remember to re-check `status`. Poll `status` only when you can't block (e.g. several runs at once: background one `wait` per run).
3. Read artifacts from disk under `.spar/runs/<run-id>/` — do not rely on chat alone.
4. Branch on exit codes, not only stderr text.
5. Never merge to the default branch; shipping is gated.
6. Use `--dry-run` (or `SPAR_DRY_RUN=1`) to exercise workflows without live provider CLIs.
7. **Know what branch the slots are on.** Slot worktrees are cut from the run's *base*: `--base <ref>` if given, else the HEAD of the directory you invoked spar from. Assert `base_commit` from the launch JSON before you trust a review or a diff.

## Exit codes

| Code | Constant | When |
|------|----------|------|
| 0 | Success | Command succeeded; plan auto-approved / done |
| 1 | Failure | Hard error, rejected plan, no usable providers, failed run |
| 2 | HumanGate | Plan approval, winner confirm, or ship confirm required |
| 3 | Stuck | Policy chain exhausted; needs human |
| 4 | Quota | No usable provider / quota pause (`phase: quota`) |

**`status` is observe-only:** process exit is always `0` when the run loads. Read `phase` / JSON `exit_code` for run state. Use `wait` when you want process exit coded by gate/stuck.

**`--dry-run`:** no real git worktrees; only `.spar/` state + stubbed agents. Live runs provision sibling worktrees.

## Reclaiming worktrees

Nothing reclaims slot worktrees on its own: the normal successful path ends at the ship
gate rather than `done`, and `auto_cleanup` is off by default. `spar cleanup --all` sweeps
runs nothing can resume (`done`, `plan_rejected`); add `--older-than 7d` to also take
resumable-but-stale ones (`stopped`, `failed`, `stuck`, `quota`, gates). In-flight runs are
never swept — `spar stop --abandoned` first. `spar reject` reaps its own worktrees.

## Abandoned runs

A run in a non-resting phase with no live orchestrator is abandoned: the driver died and
its slots are still running (they are in their own process groups by design). `spar wait`
exits `3` with an `error` saying so after a 15s grace rather than blocking; `status --json`
carries `orphan_pids`; `spar stop --abandoned` reaps every such run in the project and
parks it at `stopped`. Runs at rest (terminal, gate, stopped) are never swept.
SIGINT/SIGTERM to an orchestrating `spar` now reaps its slots first; SIGKILL cannot be
caught, which is what the sweep is for.

## Run config isolation

`spar.toml` is one file per project and every process reads it, so parallel agents writing
`[roles]` into it clobber each other. Two things fix that:

- **`--role <role>=<provider>`** on `plan` / `implement` / `run` assigns per role without
  touching the file (repeatable; repeated `reviewer` builds the panel). It satisfies the
  `--providers`-or-`--select` requirement on its own. Use it instead of editing `spar.toml`.
- **Each run freezes its config** at creation into `.spar/runs/<id>/config.json`. Every later
  phase of that run reads the snapshot, so another agent's edit cannot change its fleet,
  timeouts, spec/suite channels, or ship gate. `spar implement --run <id> --reload-config`
  re-reads the file, re-freezes, and then re-applies the overrides the run was launched
  or amended with. `--role`, `--backup`, `--effort`, `--without` and `--fleet` amend a
  resume's frozen config directly (only `--suite-command` still needs `--reload-config`).
  `--without reviewer[N]` (or an existing seat id) drops a reviewer seat outright: the
  seat, its review history, and its verdicts stop gating the ship. The ordinal indexes
  the live panel positions at the moment `--without` is parsed. Dropping the last
  reviewer is refused, unless `--role reviewer=` in the same amend re-seats the panel
  to its pin; a run with no live panel seats left fails before dispatch (exit 1) and
  never reaches the ship gate unreviewed. A bound `--role reviewer=` pin moves, mints
  or retires live seats to match it, so the panel the dispatch reviews with is the
  panel the frozen config names. A widened `-wide` extra holds no ordinal (an ordinal
  past the panel is refused) and keeps reviewing outside the config when the pin it
  duplicated is dropped. The amend applies only once every launch precondition passes,
  so a refused launch changes nothing.
- **`--reassign <role|seat>=<provider>`** on `implement --run <id>` moves a seat that already
  exists to another provider on the same worktree and branch (a reload only changes a
  seat's model). Operator-only; refused while the run's orchestrator is alive and onto a
  paused provider (O136).

## Base ref

`plan` / `implement` / `run` accept **`--base <ref>`** (branch, tag, sha, `origin/main`,
`HEAD~2`), resolved in your current directory when it is inside the project's repo, against
the main checkout otherwise. It fixes the commit every slot worktree in the
run is cut from. Without it the base is the HEAD of the invoking directory — *not*
`project_root`, which is always the repo's main checkout even when you drive spar from a
linked worktree (that is where `.spar/` lives).

The base is resolved once per run id and reported as `base_ref` / `base_commit` in every run
JSON and in `status --json` (both the single-run and the list form). It is a commit, so
uncommitted work in your tree is not in it — spar warns about that on **stderr**, which is the
only channel for it, so capture stderr as well as stdout. `implement --run <id>` inherits the
base; a run cannot be re-based after creation (`--base` with a different commit exits `1`).

```bash
BASE=$(spar run --workflow review -t "$TASK" --providers cli:grok,cli:claude --json | jq -r .base_commit)
[ "$BASE" = "$(git rev-parse HEAD)" ] || echo "slots are not on my branch"
```

`spar ship` targets its draft PR at the run's base branch when a local remote-tracking ref
exists for it (no network call — `git fetch --prune` if stale), else the repo default (noted
on stderr); `spar ship <id> --base <branch>` forces the target.

## Path A (plan → approve → implement) — **one run id**

```bash
spar plan --task "$TASK" --providers cli:claude,cli:grok --detach --json
# → { "run_id": "...", "id": "...", "phase": "...", ... }

spar wait "$RUN_ID" --json
# exit 2 + phase awaiting_plan_approval  (manual autonomy)
# OR exit 0 + plan_approved             (semi/high/full)

# Read plan + acceptance contract:
#   .spar/runs/$RUN_ID/artifacts/plan.md
#   .spar/runs/$RUN_ID/artifacts/test-contract.md

spar approve "$RUN_ID" --json   # only if still awaiting_plan_approval

# SAME run id continues into implement (workflow becomes loop).
spar implement --run "$RUN_ID" --providers cli:claude,cli:grok,cli:agy --detach --json
# → { "run_id": "$RUN_ID", ... }   # not a child run

spar wait "$RUN_ID" --json
# exit 2 + awaiting_ship_confirm when ready
# exit 2 + awaiting_round_extension if the run hit its round ceiling first:
#   spar implement --run "$RUN_ID" --max-rounds 12 --detach --json

spar ship "$RUN_ID" --confirm --json
```

**Rounds are bounded (O52).** A round is a cold re-dispatch, and measured over 197 runs
one fix round is a 6.6x median run cost. `[rounds] max` (default 8) caps `state.round`;
reaching it parks the run at the `awaiting_round_extension` human gate (**exit 2**) with
`round` / `max_rounds` in `status --json` and the reason in `error`. Raise it with
`--max-rounds <N>` (>= 1, sticky on the run, clears `error`) or stop the run — do not
loop on re-entering `implement`, which gates again without dispatching. Exit `3`
(`stuck`) still outranks this gate, and a lift preserves the escalation ladder's
progress, so a lift loop terminates. Two consecutive implementer dispatches that change nothing
while a still-blocking review against the same commit is open park at the same
gate (O178): the first re-dispatches the implementer with a warning before the
panel runs, the second parks with the reason in `error`. A stall parks before
the panel, so re-enter after changing the tree, without `--max-rounds` below the ceiling (at the ceiling the park message names the `--max-rounds` value). On every re-entry `--max-rounds` only ever raises: a value resolving under the ceiling keeps it, and an uncapped run stays uncapped. If the blocking review is wrong, rule it out with `spar rule add` and re-enter with `implement --run <id> --re-review` (O180): the unchanged HEAD goes straight back to the suite and panel with the ruling in context, without dispatching the implementer. A re-entry that would
adopt a `test-contract.md` that drifted mid-round exits `1` until you pass
`--accept-contract`. Between rounds the implementer's
`artifacts/carry-forward-<slot>.md` is seeded into the next round's prompt, capped by
`[rounds] carry_forward_chars`.

`--providers` is **required** for plan / implement / run (no implicit fleet).

**Note:** `exit_code` in JSON is only set when the phase is terminal or a human gate (`null` while in-flight). Block on `wait --follow` and branch on its exit code rather than polling `status` for `phase`.

## Path B (autonomous task)

```bash
spar implement --task "$TASK" --providers cli:claude --detach --json
spar wait "$RUN_ID" --timeout 2h --json
```

## Providers (dual backend)

```bash
# bare names = native-cli
--providers cli:claude,cli:grok

# explicit
--providers cli:claude,api:openai,api:xai
```

API keys: `OPENAI_API_KEY`, `XAI_API_KEY`, optional `*_BASE_URL` / `*_MODEL`.

Quota pauses: `spar provider pause <ref>` / `spar provider resume <ref>`, `spar provider list [--json]` shows every pause with its age. agy (Gemini) meters models separately, so a pause lands per model (`cli:agy@flash` never benches `cli:agy@pro`): `resume cli:agy` clears the login and every model under it, `resume cli:agy@<model>` clears one and reports when a login-wide pause still holds it. claude meters its weekly windows per model family: a per-model rejection pauses only `cli:claude@opus` / `@sonnet` / `@fable`, while login-wide windows pause `cli:claude` and hold every family; `resume cli:claude` clears the login and every family, `resume cli:claude@<family>` clears one. codex bills `/` models to one `openrouter` bucket and explicit bare models to a `native` bucket (`cli:codex@native`); only a model-less seat dispatches under the default profile, which layers OpenRouter. The bare login stays the operator key holding both buckets. pi bills `openrouter` models to one bucket and each direct upstream (`direct:<provider>`) to its own; a bare-model seat keys the login. `api:` providers bill per model. grok and muse pause per login. Only seats on a paused bucket stop: a paused seat moves to its backup when it has one, else a paused implementer or tester parks the run at `quota` (exit `4`) while a paused reviewer stands down and the run carries on. A panel with no usable reviewer left stops too: `quota` when any reviewer stood down on quota (exit `4`), readiness naming the failed probes when every reviewer stood down for lack of a healthy provider (exit `1`). A stood-down reviewer records no new verdict, but a `request_changes` it already recorded on the head being shipped still blocks the ship gate, and the ship body marks stood-down seats instead of printing their verdicts. A bare `provider pause <ref>` names the account and holds every bucket under it; naming a model (`provider pause cli:pi@direct:anthropic/x`) pauses just that bucket and leaves its siblings running, and resuming names the same scope. Probes key what the seat would bill, never the whole login: a bare `provider probe cli:codex` keys the ambient route's bucket (an ambient model names its own bucket, so `SPAR_CODEX_MODEL=gpt-5` probes `cli:codex@native`; only a model-less seat under the default profile probes `cli:codex@openrouter`), and `provider probe --run <id>` keys that run's recorded dispatch route. Two models, families, or buckets of one login are one subscription for backups: never a primary's backup.

Failure classifier (O148): opt-in `[classifier]` in `~/.config/spar/config.toml` only.
When on, a failure spar's rules read as work is also put to `typesafe/jev-1.14`; at
>= 0.7 a quota answer pauses the provider and takes the backup (exit `4` with none), an
auth answer takes the O104 path, a transient one the retry loop. A text-read quota or
auth verdict the classifier calls work at >= 0.9 becomes a weak pause instead of parking.
Errors, timeouts and a missing key change nothing. Key: `key_file` or
`OPENROUTER_API_KEY`; `spar doctor` reports whether it is readable.

## Arena

```bash
spar run --workflow arena --task "$TASK" --providers cli:claude,cli:grok,cli:agy --json
spar confirm "$RUN_ID" [--winner slot-id] --json
# or: spar reconcile "$RUN_ID" --json
spar ship "$RUN_ID" --confirm --json
```

## Bus / peer

The bus is workspace-scoped and keyed by `agent_id`; `--run` is an optional grouping tag.

```bash
spar bus send --run "$RUN_ID" -m "hello" --to broadcast
spar bus log --run "$RUN_ID"
spar run --workflow peer --task "$TASK" --providers cli:claude,cli:grok --json
```

Poll-file slots (agy, pi) read `logs/nudges-<slot>.md`, a rendering of their inbox;
a chat line there names the exact `spar bus ack <id>` command to clear it. Inside a
slot, `bus send`/`ack` (`--from`) and `reserve`/`release` (`--holder`) default to
`$SPAR_AGENT_ID`, so a bare `spar bus ack <id>` from the slot records the slot.
From outside, `spar bus ack <id> --run <id>` stops a `requires-ack` redelivery
without dropping the slot's file copy and without flipping the log word; it is the
escape hatch for a wedged slot. To take the item back entirely, withdraw the message
instead.

## Event feed

`spar events --follow --json` streams every run's lifecycle events in the project as
one feed (phase changes, gates, slot status, slot notes, daemon-detected abandonment),
read from each run's `.spar/runs/<id>/events.jsonl`. One object per line:

```json
{"run_id":"3d3d6f59","cursor":"…","ts":"2026-11-01T12:00:00Z","type":"phase","phase":"dispatch","prev_phase":"plan_approved","round":1}
```

`type` is `phase`, `slot`, `gate`, `info`, `archived`, `unarchived`, `abandoned`
(daemon-detected; `phase` is where the run was left) or `classifier` (O148: `slot` and a
`classifier` object with `model`, `choice`, `probability`, `probabilities`, `acted`,
`action`, `regex_verdict`, `latency_ms`, `cost_usd`, `cached`, `error`); `phase`, `prev_phase`, `slot`,
`status`, `message` and `round` appear when set. Fields and types are only added, never
renamed, so ignore a `type` you don't know.

- Without `--follow` it prints the backlog of every non-archived run, ordered by `ts`,
  and exits `0`. A run archived while you were seeing it is shown through its `archived`
  event, then hidden until `unarchived`. With `--follow` it starts at the tail (`--since now`) and keeps going,
  picking up runs created after it started.
- **Resume:** pass the `cursor` of the last line you handled to `--since`. The cursor is
  opaque and records a position in every run, so the replay has no gaps and no repeats,
  even for runs created after it was issued. It grows about 15 bytes per run, so with
  several thousand runs it approaches the ~128 KiB single-argument limit.
- One run's events keep their file order; across runs a late event can carry an
  earlier `ts` than one already printed.
- `--run <id>` limits the feed to one run (shown even if archived). An unknown id or a
  malformed cursor exits `1`.
- `--tail-cursor` prints `{"cursor": "..."}` (with `--json`) for the current end of every
  log and exits, printing no events. Start `--follow --since <that cursor>` with it so a
  follower that drops before its first event still resumes with no gap.
- `--exit-on-stdin-eof` makes `--follow` exit `0` when stdin closes, so a follower
  spawned with a stdin pipe dies with its parent.

## Status JSON

`status` / `wait` print full `RunState` (`id` field). Workflow emits also include `run_id` (alias of `id`).
