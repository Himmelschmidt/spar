# Verify Priority 2: Criterion ids + reviewer context

> This file is for verifying the work done in [02-criterion-ids-and-reviewer-context.md](02-criterion-ids-and-reviewer-context.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

`templates/test_author.md` now instructs the test author to emit stable `AC-n:` criterion
ids with `verify:` hints. `templates/reviewer_adversarial.md` gained `## Plan`,
`## Acceptance contract`, and `## Suite report` sections carrying `{{plan_body}}`,
`{{test_contract_body}}`, and the previously-dead `{{suite_body}}`. Two lines were added
to the reviewer's `extra_vars` at `src/workflow/implement.rs:721-724`, and the superseded
one-line `contract_note` nudge at :688-692 was removed. Template-content assertions and a
no-residual-placeholder render test guard the wiring.

## Deliverables

### D1: Contract template emits stable criterion ids
**Expected:** `templates/test_author.md` specifies `- [ ] AC-1: <statement> — verify: <hint>`.
- [ ] `grep -n 'AC-1:' templates/test_author.md` returns a hit inside the `## Scenarios`
      format block
- [ ] `grep -n 'verify:' templates/test_author.md` returns a hit
- [ ] Correct behavior: the prose states ids are contiguous from 1 and never renumbered
- [ ] No regressions: `## Non-goals`, `## How to run`, `## Expected before implement`
      (with its `red | compile-only | skipped-reason` values), and `## Notes` are unchanged

### D2: Reviewer template sees the plan and the contract
**Expected:** the reviewer template references both bodies.
- [ ] `grep -n '{{plan_body}}' templates/reviewer_adversarial.md` returns a hit
- [ ] `grep -n '{{test_contract_body}}' templates/reviewer_adversarial.md` returns a hit
- [ ] `grep -n '{{suite_body}}' templates/reviewer_adversarial.md` returns a hit —
      the variable is no longer dead
- [ ] Correct behavior: the variable is named `test_contract_body`, **not**
      `contract_body`. `grep -rn 'contract_body' src/ templates/` shows only
      `test_contract_body` — a second name for the same document was not introduced
- [ ] No regressions: `{{suite_guidance}}` is still present

### D3: Reviewer extra_vars carry the bodies
**Expected:** `src/workflow/implement.rs` inserts both into the reviewer's `extra_vars`.
- [ ] Around :721-724 the map now contains `plan_body` and `test_contract_body` alongside
      `review_cwd`, `suite_body`, `suite_guidance`
- [ ] Correct behavior: the values come from locals already read at :499-500 and :510-515
      — **no new file I/O was added** in the reviewer path. Read the surrounding function
      to confirm

### D4: The superseded nudge is gone
**Expected:** `contract_note` no longer exists.
- [ ] `grep -n 'contract_note' src/workflow/implement.rs` returns nothing
- [ ] No regressions: `suite_guidance` still renders its three distinct pass/fail/
      inconclusive bodies (`implement.rs:349-379` is otherwise unmodified)

### D5: Drift guards are in place
**Expected:** tests fail if a template and the Rust code disagree.
- [ ] `src/workflow/implement.rs` has `test_author_template_emits_criterion_ids` and
      `reviewer_template_sees_plan_and_contract` using `include_str!`
- [ ] `src/templates.rs` has a test rendering `reviewer_adversarial` with `base_vars`
      only and asserting no `"{{"` remains in the output
- [ ] Correct behavior: temporarily delete `{{plan_body}}` from the reviewer template and
      confirm `cargo test` **fails**, then restore it. A guard that does not fail is not
      a guard

## Automated checks

```bash
cd ../spar-feat-acceptance
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
- [ ] All pass

## Integration checks

```bash
cd /tmp && rm -rf spar-v2 && mkdir spar-v2 && cd spar-v2 && git init -q && \
  git commit -q --allow-empty -m init
spar plan --dry-run --providers cli:claude,cli:grok,cli:agy -t "add a hello function"
RUN=$(ls -t .spar/runs | head -1)
spar approve $RUN
spar implement --run $RUN --dry-run --providers cli:claude,cli:grok,cli:agy
```
- [ ] Each `.spar/runs/$RUN/prompt-review-*.md` contains the string `Acceptance contract`
- [ ] Each of those prompt files contains **zero** occurrences of `{{` —
      `grep -c '{{' .spar/runs/$RUN/prompt-review-*.md` returns 0 for every file
- [ ] `.spar/runs/$RUN/artifacts/test-contract.md` exists and its scenarios carry `AC-`
      ids (dry-run synthesis may differ; if the synthesized contract has no ids, note it —
      Priority 3 Step 6 depends on the synthesizer producing them)
- [ ] The implementer prompt still contains the plan and contract
      (`prompt-impl.md` — the pre-existing behavior at :562-565 was not broken)
- [ ] Priority 1's parser is still uncalled by production code — this priority wires
      **inputs** to the reviewer, not the gate

## Notes

[Leave blank — the verifier fills this in with findings]
