# Priority 3: Acceptance gate

## Goal

Replace the unanchored substring verdict scan with the Priority 1 parser, add an
acceptance gate on top of it, and record the decision. After this priority, a run cannot
reach `AwaitingShipConfirm` while any contract criterion is failed, unverified (by
default), or simply unmentioned by the reviewer. This is the payload of Workstream A and
extends DECISIONS O16 ("clean exit != success") into "unmet acceptance criterion !=
success".

Depends on Priority 1 (the parser) and Priority 2 (reviewers can actually see the
criteria — gating on criteria a reviewer was never shown would be unjust and would just
wedge every run).

## Approach

Three moving parts plus one easily-missed coupling:

1. Extend the reviewer template's output contract with an `## Acceptance` block.
2. Add a `[review]` config section with `require_all_criteria` (default `true`).
3. Replace `implement.rs:773-777` with `parse_review` + a new `acceptance_blocks_ship`
   helper, and fix the dead anchored branch in `arena.rs:400-417`.

**The coupling that will bite you:** the dry-run backend synthesizes review artifacts at
`src/executor.rs:~945-978` (forcing verdicts via `SPAR_FORCE_REQUEST_CHANGES`, slot ids
containing `harsh`, and an `extra_var` key literally named `request_changes`). The gate is
fail-closed by construction, so if the synthesizer is not updated in lockstep, **every
dry-run implement flips to `request_changes`** and a cluster of scenario tests fails in a
way that looks like a gate bug rather than a fixture bug. Step 6 handles this.

Target gate logic:

```rust
fn acceptance_blocks_ship(criteria: &[String], res: &ReviewResult, cfg: &Config) -> bool
```

| Condition | Blocks? |
|-----------|---------|
| `criteria` is empty (no contract — `[spec].enabled = false`) | **no** — acceptance is not evaluated at all; verdict alone gates |
| any reported `AcStatus::Fail` | yes |
| any id in `criteria` absent from `res.acceptance` | yes — the "reviewer skipped a criterion" gate |
| any `AcStatus::Unverified` | `cfg.review.require_all_criteria` |

---

## Steps

### Step 1: Extend the reviewer output contract
**File:** `/home/sholom/projects/spar/templates/reviewer_adversarial.md` (format block, ~lines 15-27)

Insert an `## Acceptance` section between `## Verdict` and `## Findings`:

```markdown
## Verdict
approve | request_changes

## Acceptance
AC-1: pass|fail|unverified — evidence (command output, file:line, or observed behavior)
AC-2: ...

## Findings
- severity: critical|major|minor — description

## Tests
what was checked; be explicit about suite evidence vs targeted checks
```

Add prose rules:
- Every `AC-n` in the acceptance contract must appear **exactly once**.
- `unverified` means you could not check it — not that it looks fine.
- Evidence is mandatory for `pass`.
- Write `## Verdict` once. Do not restate the format block in your output.

- [ ] Done

### Step 2: Add the `[review]` config section
**File:** `/home/sholom/projects/spar/src/config.rs`

There is no `[review]` section today. Review timeouts live at `[timeouts].review_secs`
(:214-248) and **stay there** — moving them would be a gratuitous break.

The repo's shadow-struct ritual requires **four** coordinated edits (mirror the `[suite]`
arm at :483 as the model):

2a. `pub struct ReviewConfig { #[serde(default = "default_true")] pub require_all_criteria: bool }`
    plus a `Default` impl, near `SuiteConfig` (:250-274).

2b. Field on `Config` (:7-40): `#[serde(default)] pub review: ReviewConfig,`

2c. `ReviewConfigFile` with all-`Option` fields, near :360-432; plus a field on
    `ConfigFile` (:360-376).

2d. A merge arm in `apply_file` (:449-567). Trust level: `Trust::Project` — a project may
    set this. (Only `[notify]` is user-only, at :552-565.)

- [ ] Done

### Step 3: Add the acceptance gate helper
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs`

Add `acceptance_blocks_ship` next to `suite_blocks_ship` (:322-325), implementing the
truth table in **Approach**.

Also add a reason-string function mirroring `suite_inconclusive_reason` (:328-347) that
renders which criteria blocked and why, so the next fix round's implementer prompt learns
*which* AC failed rather than just "changes requested".

- [ ] Done

### Step 4: Replace the substring scan
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs:773-777`

| Old | New |
|-----|-----|
| `if text.to_ascii_lowercase().contains("request_changes") { any_request_changes = true; }` | parse with `review_result::parse_review(&text)`; block when `matches!(res.verdict, Some(Verdict::RequestChanges) \| None)`; **also** block when `acceptance_blocks_ship(&contract_criteria, &res, cfg)` |

`verdict: None` blocking extends the fail-closed posture already established at
:753-772 (O16 in code), where a missing or empty review artifact both sets
`any_request_changes` and synthesizes a `## Verdict\nrequest_changes` artifact.

Compute `contract_criteria` **once, before the reviewer loop**, via
`review_result::parse_contract_criteria` on the contract body already read at :510-515.
Do not re-parse per reviewer.

**Exceptions — do NOT change:**
- The seed at :711 (`any_request_changes = suite_channel_active && suite_blocks_ship(...)`).
  The suite channel gate is independent and stays.
- The fail-closed missing-artifact branch at :753-772. It already does the right thing.
- The escalation ladder at :808-828 and `max_fix_rounds` (hardcoded 3 at :410).

- [ ] Done

### Step 5: Fix the dead anchored branch in arena
**File:** `/home/sholom/projects/spar/src/workflow/arena.rs:400-417`

This is **not merely fragile — it is dead**. The outer `if` computes a condition and the
inner `if` then re-tests a *weaker* substring condition, so the anchored
`## verdict\nrequest_changes` check never gates anything the substring check did not
already gate. Replace the whole nested construct with a single
`review_result::parse_review(&text)` call using the same fail-closed semantics as Step 4.
This removes a real bug, not just a style wart.

Arena has no acceptance contract, so call `parse_review` for the verdict only — do not
add an acceptance gate here.

- [ ] Done

### Step 6: Update the dry-run artifact synthesizer
**File:** `/home/sholom/projects/spar/src/executor.rs` (~:945-978)

**This step is mandatory and is the most likely source of a confusing failure.**

6a. The happy-path synthesized review must emit a schema-valid artifact: a `## Verdict`
    section reading `approve`, plus an `## Acceptance` block marking **every** contract
    `AC-n` as `pass` with placeholder evidence. Read the run's `test-contract.md` (or the
    contract the dry-run itself synthesized) to enumerate the ids.

6b. The forced-failure path (`SPAR_FORCE_REQUEST_CHANGES`, `harsh` in the slot id, the
    `request_changes` extra_var key at :976-978) must emit a schema-valid
    `request_changes` artifact with one AC marked `fail`.

Without 6a, `implement_dry_run_writes_suite_artifact` (tests/scenarios/plan_implement.rs:319),
`implement_dry_run_surfaces_suite_outcome` (:368), and every happy-path assertion break.

- [ ] Done

### Step 7: Make the fail-closed synthetic artifact schema-uniform
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs:753-772`

The synthetic artifact written when a review is missing or empty already sets
`## Verdict\nrequest_changes`. Add an `## Acceptance` block marking every contract
criterion `unverified` with evidence `"reviewer produced no review"`. Cosmetic for the
gate (the verdict already blocks) but keeps every `review-*.md` on one schema, which
matters for the TUI and for any outer agent reading artifacts.

- [ ] Done

### Step 8: Add unit tests
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs` (in `mod suite_parse_tests`, ~:1065-1225)

| Test | Asserts |
|------|---------|
| `acceptance_empty_criteria_never_blocks` | no contract → `false` even with an empty review |
| `acceptance_fail_blocks` | one `AC-1: fail` → `true` |
| `acceptance_missing_criterion_blocks` | contract has AC-1 and AC-2, review reports only AC-1 → `true` |
| `acceptance_unverified_blocks_by_default` | `require_all_criteria = true` + one `unverified` → `true` |
| `acceptance_unverified_allowed_when_relaxed` | `require_all_criteria = false` + one `unverified`, rest pass → `false` |
| `acceptance_all_pass_does_not_block` | every criterion `pass` → `false` |

**File:** `/home/sholom/projects/spar/src/config.rs` (in the inline `mod tests`, :591-683)

| Test | Asserts |
|------|---------|
| `review_config_defaults_to_strict` | `Config::default().review.require_all_criteria == true` |
| `review_config_overlay` | a project `spar.toml` with `[review] require_all_criteria = false` overlays correctly |

**File:** `/home/sholom/projects/spar/src/workflow/implement.rs` — extend the Priority 2
template-content assertions with `reviewer_template_declares_acceptance_block`, asserting
the reviewer template contains `"## Acceptance"` and `"unverified"`.

- [ ] Done

### Step 9: Add scenario tests
**File:** `/home/sholom/projects/spar/tests/scenarios/plan_implement.rs`

Reuse this existing file — **no new `Cargo.toml` `[[test]]` block needed**. Reuse the
helpers `spar_home_dir()` (:9), `spar_cmd()` (:20), `init_git_repo()` (:26),
`primary_branch()` (:55).

| Test | Asserts |
|------|---------|
| `implement_dry_run_missing_ac_requests_changes` | a synthesized review omitting one contract AC drives the run to a fix round, not `AwaitingShipConfirm` |
| `implement_dry_run_unverified_ships_when_relaxed` | with `[review] require_all_criteria = false` in the project `spar.toml`, an `unverified` AC does not block |

**Existing test that will break and must be updated:**
`stuck_policy_dry_run_request_changes` (:798) — its forcing mechanism must now produce a
schema-valid artifact. It drives the full ladder (fix rounds → rotate implementer → widen
reviewers → `Phase::Stuck`) and asserts `revs >= 3`; keep those assertions, fix the fixture.

- [ ] Done

### Step 10: Record the decisions
**File:** `/home/sholom/projects/spar/DECISIONS.md`

Append to the `## Orchestration` table (existing ids run O1..O18, so these are next):

```
| O19 | **Unmet acceptance criterion ≠ success.** Reviewers emit `## Acceptance` with one `AC-n: pass\|fail\|unverified — evidence` line per criterion in `test-contract.md`. Any `fail`, or any contract `AC-n` absent from the review, forces `request_changes`. `unverified` blocks by default; relax with `[review].require_all_criteria = false`. Extends O16 | DECIDED |
| O20 | **`## Verdict` is an anchored header, not a substring.** The first non-blank line under the first `## Verdict` section must be `approve` or `request_changes`; missing or unparseable ⇒ `request_changes` (fail closed). Replaces the whole-document scan that made the format block's own `approve \| request_changes` line self-blocking. One parser (`workflow/review_result.rs`) serves both implement and arena | DECIDED |
| O21 | **Reviewers see the plan and the contract.** `plan.md` and `test-contract.md` bodies are passed to the reviewer template, not summarized as a prose nudge. A reviewer that cannot see the criteria cannot verify them | DECIDED |
```

Match the existing 3-column format exactly (`| ID | Decision | Status |` with the
`|----|----------|--------|` separator). Escape the `|` inside decision cells as `\|`.

- [ ] Done

### Step 11: Update the operator skill
**File:** `/home/sholom/projects/spar/skills/core.md`

11a. Add `[review]` with `require_all_criteria` to the config-knobs TOML block (:195-235),
     placed near `[suite]` (:210-213).

11b. Document the reviewer artifact schema (`## Verdict` / `## Acceptance` / `## Findings`
     / `## Tests`) and the gate rule in `## Rules of the road` (:237-245). Outer agents
     read review artifacts; this is a public schema now.

**File:** `/home/sholom/projects/spar/spar.toml.example` — add a commented `[review]`
block next to `[suite]` (:29-33).

- [ ] Done

### Step 12: Verify
```bash
cd ../spar-feat-acceptance
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then prove the gate end to end:
```bash
cd /tmp && rm -rf spar-p3 && mkdir spar-p3 && cd spar-p3 && git init -q && \
  git commit -q --allow-empty -m init
spar plan --dry-run --providers cli:claude,cli:grok,cli:agy -t "add a hello function"
spar approve <run_id>
spar implement --run <run_id> --dry-run --providers cli:claude,cli:grok,cli:agy
cat .spar/runs/<run_id>/artifacts/review-*.md
```
Expected: every review artifact has a `## Verdict` and an `## Acceptance` block; the
happy-path run reaches `AwaitingShipConfirm` (exit code 2, the human gate) rather than
looping on `request_changes`.

- [ ] Done
