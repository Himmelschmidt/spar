# Priority 2: Criterion ids + reviewer context

## Goal

Give the acceptance contract stable, machine-parseable criterion ids (`AC-1`, `AC-2`, …)
and stop the reviewer from flying blind. Today a reviewer's prompt contains **only**
`review_cwd`, `suite_body`, and `suite_guidance` — it never sees `plan.md` or
`test-contract.md`. The contract reaches it as a single line of English nudge
(`contract_note`, `src/workflow/implement.rs:688-692`). A reviewer that cannot see the
criteria cannot verify them, so Priority 3's gate would be meaningless without this.

Depends on Priority 1 for the `AC-n` shape (the template prose and the parser must agree
on the exact format). Priority 3 depends on this.

## Approach

Two template edits plus a two-line wiring change. The wiring is trivial because
`base_vars` (`src/templates.rs:58-90`) **already seeds both variables**: `plan_body`
(empty string) and `test_contract_body` (`"(no pre-written acceptance contract)"`). No
new defaults are needed — only placeholders in the reviewer template and two inserts into
the reviewer's `extra_vars`.

Also fix `suite_body`: it is seeded at `src/templates.rs:80` with `"(no suite report)"`
and referenced by **no template at all** — it is dead globally, not just on the reviewer
path. Rather than delete it, give it a real `## Suite report` section in the reviewer
template. `suite_guidance` (`implement.rs:349-379`) currently embeds the suite body
inside a prose blob, so the reviewer sees evidence interleaved with instructions; a
dedicated section is cleaner and shortens the guidance string.

**Naming decision — reuse `test_contract_body`, do not introduce `contract_body`.**
It is already in `base_vars` with a sensible default and already consumed by
`templates/implementer.md`. A second name for the same document means a second default,
and `render_str` (`src/templates.rs:37-43`) silently leaves `{{contract_body}}` literal
on any path that forgets it.

---

## Steps

### Step 1: Emit stable criterion ids in the contract template
**File:** `/home/sholom/projects/spar/templates/test_author.md` (contract format block, ~lines 43-58)

| Element | Old | New |
|---------|-----|-----|
| `## Scenarios` item format | `- [ ] …` | `- [ ] AC-1: <observable statement> — verify: <how to check it>` |

Add instruction prose above the block stating:
- Ids are `AC-<n>`, numbered from 1, contiguous, and **never renumbered** once written.
- Every criterion must be independently verifiable by someone who did not write the code.
- The `verify:` hint names a command, a `file:line` + assertion, or an observable
  behavior — not "check it works".

**Exceptions — do NOT change:**
- `## Non-goals`, `## How to run`, `## Expected before implement` (`red | compile-only |
  skipped-reason`), `## Notes` — those sections keep their current format.
- The `- [ ]` checkbox itself stays; the id goes *inside* it.

- [ ] Done

### Step 2: Add plan and contract sections to the reviewer template
**File:** `/home/sholom/projects/spar/templates/reviewer_adversarial.md`

Insert after `## Task` (~line 6) and before `## Paths`:

```markdown
## Plan (what was agreed)
{{plan_body}}

## Acceptance contract (what must be true)
{{test_contract_body}}
```

Add a `## Suite report` section carrying `{{suite_body}}` so the previously-dead variable
is actually referenced.

**Exceptions — do NOT change:**
- `{{suite_guidance}}` stays. It carries the pass/fail/inconclusive policy text from
  `implement.rs:349-379`, which is instruction, not evidence.
- Do not touch the `## Verdict` / `## Findings` / `## Tests` output format block yet —
  that is Priority 3, Step 1.

- [ ] Done

### Step 3: Pass plan and contract to the reviewer
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs:721-724`

| Line | Old | New |
|------|-----|-----|
| ~723 | `extra.insert("suite_body".into(), suite_body.clone());` | unchanged — now actually referenced by the template |
| after ~724 | — | `extra.insert("plan_body".into(), plan_body.clone());` |
| after ~724 | — | `extra.insert("test_contract_body".into(), contract_body.clone());` |

Both locals are already in scope in this function — `plan_body` is read at
`implement.rs:499-500` and the contract body at `implement.rs:510-515`. **No new file
I/O.** Confirm the exact local binding names at those lines before writing the inserts.

- [ ] Done

### Step 4: Remove the superseded contract nudge
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs:688-692`

Delete the `contract_note` binding and its append into `suite_guidance`. Handing the
reviewer the actual document supersedes a one-line English reminder that the document
exists. Leaving both is redundant prompt bloat.

Grep for other uses before deleting:
```bash
grep -n 'contract_note' /home/sholom/projects/spar/src/workflow/implement.rs
```
Expected: 2 hits (the binding and its single use). If more, handle each.

- [ ] Done

### Step 5: Add template-content assertions
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs` (in `mod suite_parse_tests`, ~:1065-1225)

Follow the established precedent at `implement.rs:1186-1212`
(`tester_template_never_routes_budget_exhaustion_to_green`), which greps a template via
`include_str!` and asserts a prose rule maps to Rust semantics. This is the **only**
mechanism preventing Priority 3's parser from silently matching nothing forever.

| Test | Asserts |
|------|---------|
| `test_author_template_emits_criterion_ids` | `include_str!("../../templates/test_author.md")` contains `"AC-1:"` and `"verify:"` |
| `reviewer_template_sees_plan_and_contract` | `include_str!("../../templates/reviewer_adversarial.md")` contains `"{{plan_body}}"` and `"{{test_contract_body}}"` |
| `reviewer_template_uses_suite_body` | the reviewer template contains `"{{suite_body}}"` — proves the var is no longer dead |

Verify the `include_str!` relative path matches the existing tests in that module before
writing (it is relative to the source file, so from `src/workflow/` it is `../../templates/`).

- [ ] Done

### Step 6: Add a no-residual-placeholder render test
**File:** `/home/sholom/projects/spar/src/templates.rs` (in the inline `mod tests`, ~:92-207)

`render_str` (:37-43) neither errors on a missing variable nor on an unused one — a
`{{foo}}` with no entry is left **literal in the agent's prompt**. Add a test that
renders `reviewer_adversarial` with `base_vars` only (no `extra_vars`) and asserts the
output contains no `"{{"`. This catches the class of bug where a new placeholder is added
to a template but never seeded.

An equivalent test already exists in spirit at `templates.rs:191-206`
(`reviewer_gets_suite_guidance`) — match its setup style.

- [ ] Done

### Step 7: Update the operator skill
**File:** `/home/sholom/projects/spar/skills/core.md`

The reviewer's input surface is agent-facing. In the `## Rules of the road` section
(~:237-245), note that reviewers now receive the plan and acceptance contract in full and
that contract criteria carry stable `AC-n` ids. Do not document the `## Acceptance`
output schema yet — that lands in Priority 3, where it becomes enforced.

- [ ] Done

### Step 8: Verify
```bash
cd ../spar-feat-acceptance
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then confirm the reviewer prompt actually contains the plan:
```bash
cd /tmp && rm -rf spar-p2 && mkdir spar-p2 && cd spar-p2 && git init -q && \
  git commit -q --allow-empty -m init
spar plan --dry-run --providers cli:claude,cli:grok,cli:agy -t "add a hello function"
# note the run id, approve it, then:
spar implement --run <id> --dry-run --providers cli:claude,cli:grok,cli:agy
grep -c 'Acceptance contract' .spar/runs/<id>/prompt-review-*.md
```
Expected: at least 1 hit per reviewer prompt file, and **zero** occurrences of `{{` in
those prompt files.

- [ ] Done
