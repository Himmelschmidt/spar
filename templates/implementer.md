# Role: Implementer

Implement the task in your isolated worktree. Do not modify the primary checkout.

## Task
{{task}}

{{amendment_section}}
## Plan (if any)
{{plan_body}}

## Acceptance contract (pre-written tests)
{{test_contract_body}}

## Paths
- Cwd (your worktree): {{cwd}}
- Project primary (read-only reference): {{project_root}}
- Artifacts: {{artifacts_dir}}
- Markers: {{markers_dir}}
- Slot: {{slot_id}} provider={{provider}}

## Required
1. Implement the change in `{{cwd}}` only. Pre-written acceptance tests may already be merged into this worktree — make them pass; do not delete or weaken them without documenting why in your summary.
2. Smoke-check only: compile, typecheck, or 1–2 targeted tests for **your** change (including the acceptance tests if small). Do **not** run the full multi-minute/hour suite — a dedicated cheap `tester` slot runs that after you finish.
3. Run every build, check, lint and test **in the foreground** and wait for it. No `&`, `nohup`, `disown`, background monitors, or "start it and poll later" patterns. A long build is fine — your wall-clock budget is `timeouts.slot_secs` (hours, not minutes), and blocking on it costs you nothing. Backgrounding it and burning your remaining turns polling is how a slot exits without ever writing its summary.
4. Keep long output out of your context: redirect build, lint and test output to a log and read back only what you need — `cargo build > {{artifacts_dir}}/build-{{slot_id}}.log 2>&1; tail -40 {{artifacts_dir}}/build-{{slot_id}}.log`. This is about output **volume**, not scheduling: still foreground, still blocking (see 3). On failure, mine the log for the real error (`grep -n` the failure, then read that section) — a blind `tail -5` costs you the diagnosis. Whatever lands in your transcript is re-sent on every later turn.
5. Write a summary to `{{artifacts_dir}}/summary-{{slot_id}}.md`. Write it **before** you run out of turns — if you are unsure how much room is left, write it now and update it after.
6. Write done marker `{{markers_dir}}/{{slot_id}}.done` or `.failed`

Do not merge. Do not push unless explicitly told. Prefer small commits on branch `{{branch}}`.

Do **not** use `pkill -f`, `pgrep -f`, or `killall` on any token derived from the task or a test name: your own process's argv contains the full task text, so those patterns match and kill YOU. Kill stray processes by pid instead.
