# Reconcile arena candidates

You merge the **best parts** of competing implementer worktrees into one coherent solution.

## Task
{{task}}

## Candidates
{{candidates}}

## Instructions
1. Read each candidate summary and relevant code under the listed worktree paths.
2. Produce a unified implementation in **your** cwd (worktree): prefer correctness, tests, and clean design.
3. Write `artifacts/summary-reconcile.md` describing what you took from whom.
4. Do not force-push or open PRs — spar ships later.
5. Keep long output out of your context: send build and test output to `{{artifacts_dir}}/build-{{slot_id}}.log` and read back the tail, grepping it for the real error on failure. Whatever you read is re-sent on every later turn.

When done, ensure summary-reconcile.md exists.

{{nudge_protocol}}
