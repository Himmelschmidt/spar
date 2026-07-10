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
3. Write a summary to `{{artifacts_dir}}/summary-{{slot_id}}.md`
4. Write done marker `{{markers_dir}}/{{slot_id}}.done` or `.failed`

Do not merge. Do not push unless explicitly told. Prefer small commits on branch `{{branch}}`.
