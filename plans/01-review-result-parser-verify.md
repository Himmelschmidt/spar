# Verify Priority 1: Review result parser

> This file is for verifying the work done in [01-review-result-parser.md](01-review-result-parser.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

A new pure module `src/workflow/review_result.rs` was added, exposing `Verdict`,
`AcStatus`, `AcLine`, `ReviewResult`, `parse_review(&str)`, and
`parse_contract_criteria(&str)`. It parses an anchored `## Verdict` header and an
`## Acceptance` block of `AC-n: pass|fail|unverified — evidence` lines out of a reviewer
artifact, and extracts `AC-n` ids from `test-contract.md`. No call sites were added — this
priority is additive only and changes no runtime behavior.

## Deliverables

### D1: The module exists and is registered
**Expected:** `src/workflow/review_result.rs` exists and `src/workflow/mod.rs` declares it.
- [ ] `src/workflow/review_result.rs` exists
- [ ] `grep -n 'review_result' src/workflow/mod.rs` returns a `pub mod` declaration
- [ ] Correct behavior: `cargo build` succeeds
- [ ] No regressions: no other source file references `review_result` yet
      (`grep -rn 'review_result' src/ | grep -v 'review_result.rs\|mod.rs'` is empty)

### D2: Verdict parsing is anchored, not a substring scan
**Expected:** parsing keys on the `## Verdict` header, and only the first such section.
- [ ] Correct behavior: a body with `## Verdict\napprove` and the words `request_changes`
      appearing later in `## Findings` prose parses as `Approve`. **This is the headline
      fix** — read the test `approve_body_mentioning_request_changes` and confirm it
      asserts exactly this
- [ ] Correct behavior: a blank line between `## Verdict` and `approve` still parses
      (test `verdict_blank_line_after_header`) — this is the bug `parse_suite_result`
      has at `implement.rs:296` via `.lines().nth(1)`
- [ ] Correct behavior: two `## Verdict` sections → the first wins
      (`first_verdict_section_wins`)
- [ ] Correct behavior: a missing or unrecognized verdict yields `None`, not a default
      of `Approve`. Grep the implementation to confirm there is no `unwrap_or(Approve)`
      anywhere
- [ ] `request_changes` is matched **before** `approve` in the match order — read the code

### D3: Acceptance block parsing
**Expected:** `AC-n: <status> [— evidence]` lines parse; malformed lines are skipped.
- [ ] Correct behavior: all three statuses parse, ids normalize to uppercase
- [ ] Correct behavior: both `—` and ` - ` evidence separators work
- [ ] Correct behavior: a prose line inside `## Acceptance` is skipped without aborting
      the surrounding lines
- [ ] Correct behavior: a missing `## Acceptance` section yields an empty vec, not an error

### D4: Contract criterion extraction
**Expected:** `parse_contract_criteria` returns `AC-n` ids deduplicated in
first-appearance order.
- [ ] Correct behavior: duplicates collapse
- [ ] Correct behavior: order is first-appearance, not sorted
- [ ] Correct behavior: a contract with no `AC-n` tokens yields an empty vec

### D5: No new dependency
**Expected:** no `regex` crate was added for this.
- [ ] `git diff main -- Cargo.toml Cargo.lock` shows no new dependency, **or** `regex`
      was already present before this change

## Automated checks

```bash
cd ../spar-feat-acceptance
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test review_result
cargo test
```
- [ ] All pass
- [ ] `cargo test review_result` reports at least 18 tests
- [ ] The full `cargo test` count is unchanged from `main` **plus** the new tests — no
      pre-existing test changed status

## Integration checks

- [ ] Both functions are `pub` and callable from `src/workflow/implement.rs`
      (Priority 3 needs them) — confirm by adding a throwaway call, compiling, then reverting
- [ ] Neither function returns `Result` — unparseable input is a policy signal
      (`None` / empty vec), not an error
- [ ] `parse_suite_result` at `src/workflow/implement.rs:280-304` is **unmodified** —
      it still serves `suite.md` and was not refactored into the new module
- [ ] The work is on branch `feat/acceptance-gate` in worktree `../spar-feat-acceptance`,
      not in the primary checkout: `cd /home/sholom/projects/spar && git status --short`
      is clean and `git rev-parse --abbrev-ref HEAD` is still `main`

## Notes

[Leave blank — the verifier fills this in with findings]
