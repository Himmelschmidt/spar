# Priority 1: Review result parser

## Goal

Create one pure module that parses a reviewer's markdown artifact into a structured
result: an anchored `## Verdict` and an `## Acceptance` block of `AC-n: pass|fail|unverified`
lines. Also parse `AC-n` ids out of `test-contract.md`. This is deliberately shipped
alone, with no call sites and no behavior change, because it is pure and trivially
verifiable — Priorities 2 and 3 then wire it in. Nothing depends on this priority
except Priorities 2 and 3.

Today the verdict is decided at `src/workflow/implement.rs:773-777` by
`text.to_ascii_lowercase().contains("request_changes")`, an unanchored whole-document
substring scan. `templates/reviewer_adversarial.md:20` literally contains the line
`approve | request_changes` inside its own format block, so a reviewer that echoes the
format is scored `request_changes` — approval is unreachable for that reviewer. There is
no test anywhere covering this scan.

## Approach

Model the parser on `parse_suite_result` (`src/workflow/implement.rs:280-304`) — the
repo's one existing artifact parser — but deliberately fix two of its bugs rather than
reproduce them:

- `parse_suite_result` does `lower.find("## result")` on the whole lowercased string, so
  a `## Result` mentioned in prose earlier wins. Use a **line-based** section walk.
- `parse_suite_result` reads `after.lines().nth(1)` — literally the next physical line —
  so a blank line between header and value yields `None`. **Skip blank lines.**

Target shape:

```rust
pub enum Verdict { Approve, RequestChanges }
pub enum AcStatus { Pass, Fail, Unverified }

pub struct AcLine {
    pub id: String,
    pub status: AcStatus,
    pub evidence: String,
}

pub struct ReviewResult {
    pub verdict: Option<Verdict>,
    pub acceptance: Vec<AcLine>,
}

pub fn parse_review(body: &str) -> ReviewResult;
pub fn parse_contract_criteria(body: &str) -> Vec<String>;
```

`parse_contract_criteria` is the first Rust code in the repo that reads
`test-contract.md` as anything but an opaque string.

---

## Steps

### Step 1: Create the worktree

```bash
cd /home/sholom/projects/spar
git worktree add ../spar-feat-acceptance -b feat/acceptance-gate
```

All of Priorities 1-3 happen in `../spar-feat-acceptance`. Never edit the primary
checkout and never switch its branch — other agents may be running there.

- [ ] Done

### Step 2: Confirm whether `regex` is already a dependency
**File:** `/home/sholom/projects/spar/Cargo.toml`

```bash
grep -n 'regex' /home/sholom/projects/spar/Cargo.toml
```

If `regex` is **not** present, **do not add it**. Hand-roll the line split with
`split_once(':')` / `split_once('—')` / `split_once(" - ")`, consistent with every other
parser in this repo (`parse_suite_result`, `src/model_select/vals.rs`). A new dependency
for one line-format is not justified.

- [ ] Done

### Step 3: Create the parser module
**File:** `/home/sholom/projects/spar/src/workflow/review_result.rs` (new)

Implement the types and two functions from **Approach** above.

Parsing rules for `parse_review`:

| Rule | Behavior |
|------|----------|
| Section detection | Walk lines. A line whose `trim_start()` starts with `##` opens a new section. Match the remaining title case-insensitively against `verdict` / `acceptance`. |
| Blank lines | Skipped when looking for the first content line of a section. |
| Verdict value | First non-blank line in the section, `trim()`, then `trim_start_matches(['*', '`', '_', '-', ' '])` — same noise set as `parse_suite_result:296`. |
| Verdict match order | Test `request_changes` **first**, then `approve`. A hedged `request_changes (see findings)` must not be mis-scored, and `approve` must not match a line reading `approve is not warranted`. Use `starts_with`. |
| Duplicate sections | Only the **first** `## Verdict` section counts. A reviewer quoting the format block later in the document must not flip the result. |
| Unparseable | `verdict: None`. The caller (Priority 3) treats `None` as blocking. |
| Acceptance lines | Under `## Acceptance`, match `[-*]? AC-<n> : <status> [— evidence]`. Case-insensitive on both the `AC` prefix and the status word. |
| Acceptance separators | Accept either an em-dash `—` or a spaced hyphen ` - ` before the evidence. Evidence is optional at parse time; Priority 3 does not require it. |
| Malformed lines | Skipped silently, not an error. A reviewer's prose inside the section must not abort parsing. |

Parsing rules for `parse_contract_criteria`: scan the whole body (not just
`## Scenarios` — a criterion may be restated) for tokens matching `AC-<digits>`,
uppercase-normalize, and return them **deduplicated, in first-appearance order**.

**Exceptions — do NOT:**
- Add a `regex` dependency (see Step 2).
- Return `Result` from either function. Both are total: unparseable input yields
  `verdict: None` / an empty vec. Parse failure is a *policy* signal, not an error.
- Reuse or modify `parse_suite_result` — it stays as-is for `suite.md`.

- [ ] Done

### Step 4: Register the module
**File:** `/home/sholom/projects/spar/src/workflow/mod.rs`

Add `pub mod review_result;` alongside the existing workflow module declarations.

- [ ] Done

### Step 5: Add inline tests
**File:** `/home/sholom/projects/spar/src/workflow/review_result.rs`

Add `#[cfg(test)] mod tests` at the bottom of the same file (the repo convention — 30
inline test modules across `src/`). Required cases:

| Test | Asserts |
|------|---------|
| `verdict_approve` | `## Verdict\napprove` → `Some(Approve)` |
| `verdict_request_changes` | `## Verdict\nrequest_changes` → `Some(RequestChanges)` |
| `verdict_blank_line_after_header` | `## Verdict\n\napprove` → `Some(Approve)` — the bug `parse_suite_result` has |
| `verdict_markup_tolerated` | `## Verdict\n**approve**` → `Some(Approve)` |
| `approve_body_mentioning_request_changes` | An `approve` verdict whose `## Findings` prose says "I considered request_changes" → `Some(Approve)`. **This is the headline regression the whole workstream exists to fix.** |
| `format_block_echo_does_not_flip` | A body containing the literal `approve \| request_changes` line after a real `## Verdict\napprove` → `Some(Approve)` |
| `first_verdict_section_wins` | Two `## Verdict` sections (`approve` then `request_changes`) → `Some(Approve)` |
| `verdict_missing_is_none` | No `## Verdict` section → `None` |
| `verdict_garbage_is_none` | `## Verdict\nlgtm` → `None` |
| `acceptance_parses_all_three_statuses` | pass / fail / unverified all round-trip, ids uppercase |
| `acceptance_evidence_captured` | `AC-1: pass — cargo test output` → evidence `"cargo test output"` |
| `acceptance_hyphen_separator` | `AC-1: pass - foo` parses evidence `"foo"` |
| `acceptance_bulleted_lines` | `- AC-1: pass — x` parses identically |
| `acceptance_malformed_line_skipped` | A prose line inside `## Acceptance` is skipped, surrounding AC lines still parse |
| `acceptance_missing_section_is_empty` | No `## Acceptance` → empty vec, not an error |
| `contract_criteria_extracted_in_order` | A contract body with AC-1, AC-3, AC-2 → `["AC-1","AC-3","AC-2"]` |
| `contract_criteria_deduplicated` | An id repeated in `## Scenarios` and `## Notes` appears once |
| `contract_criteria_empty_when_absent` | A contract with no `AC-n` tokens → empty vec |

- [ ] Done

### Step 6: Verify
```bash
cd ../spar-feat-acceptance
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test review_result
cargo test
```

Expected: the new `review_result` tests all pass; **no existing test changes behavior**
(this priority adds no call sites). Clippy clean with `-D warnings`.

- [ ] Done
