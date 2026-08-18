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
