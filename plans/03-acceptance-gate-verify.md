# Verify Priority 3: Acceptance gate

> This file is for verifying the work done in [03-acceptance-gate.md](03-acceptance-gate.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

The reviewer output contract gained an `## Acceptance` block. A new `[review]` config
section with `require_all_criteria` (default `true`) was added to `src/config.rs`. The
substring verdict scan at `src/workflow/implement.rs:773-777` was replaced with
`review_result::parse_review` plus a new `acceptance_blocks_ship` helper, and the dead
nested branch at `src/workflow/arena.rs:400-417` was replaced with the same parser. The
dry-run artifact synthesizer in `src/executor.rs` was updated to emit schema-valid
reviews. DECISIONS gained O19, O20, O21.

## Deliverables

### D1: Reviewer output contract declares the acceptance block
**Expected:** `templates/reviewer_adversarial.md` specifies `## Acceptance`.
- [ ] `grep -n '## Acceptance' templates/reviewer_adversarial.md` returns a hit
- [ ] The format line shows `AC-1: pass|fail|unverified — evidence`
- [ ] Correct behavior: prose states every contract `AC-n` must appear exactly once, that
      `unverified` means "could not check", and that evidence is mandatory for `pass`
- [ ] No regressions: `## Verdict`, `## Findings`, `## Tests` are all still present

### D2: The `[review]` config section
**Expected:** `[review].require_all_criteria` parses and defaults to `true`.
- [ ] `Config::default().review.require_all_criteria == true` (test `review_config_defaults_to_strict`)
- [ ] A project `spar.toml` with `[review] require_all_criteria = false` overlays correctly
- [ ] Correct behavior: all four shadow-struct touch points exist — `ReviewConfig`,
      field on `Config`, `ReviewConfigFile`, field on `ConfigFile`, and a merge arm in
      `apply_file`. Missing the merge arm makes the key silently do nothing, which the
      default-value test would not catch
- [ ] No regressions: `[timeouts].review_secs` was **not** moved into `[review]` —
      `grep -n 'review_secs' src/config.rs` still shows it under `TimeoutConfig` (:214-248)

### D3: The verdict scan is gone
**Expected:** no unanchored substring scan remains.
- [ ] `grep -rn 'contains("request_changes")' src/` returns **nothing**
- [ ] `src/workflow/implement.rs` around :773-777 calls `review_result::parse_review`
- [ ] Correct behavior: `verdict: None` blocks (fail-closed). Read the match and confirm
      it is `Some(RequestChanges) | None`, not just `Some(RequestChanges)`
- [ ] `src/workflow/arena.rs:400-417` no longer has the nested outer/inner condition and
      uses the shared parser

### D4: The acceptance gate truth table
**Expected:** `acceptance_blocks_ship` implements exactly the four rules.
- [ ] Empty criteria (no contract) → **does not block**. This matters: runs with
      `[spec].enabled = false` must not be gated on criteria that were never written
- [ ] Any `fail` → blocks
- [ ] A contract id absent from the review → blocks
- [ ] `unverified` → blocks iff `require_all_criteria`
- [ ] Correct behavior: `contract_criteria` is computed **once before** the reviewer loop,
      not per reviewer — read the code
- [ ] The failure reason names which criterion blocked, so the next fix round's
      implementer prompt is actionable

### D5: The dry-run synthesizer emits schema-valid reviews
**Expected:** dry-run happy paths still reach `AwaitingShipConfirm`.
- [ ] `src/executor.rs` (~:945-978) synthesizes a review with `## Verdict\napprove` and an
      `## Acceptance` block marking every contract AC `pass`
- [ ] The forced-failure path (`SPAR_FORCE_REQUEST_CHANGES`, `harsh` slot ids, the
      `request_changes` extra_var key) emits a schema-valid `request_changes` with one AC `fail`
- [ ] Correct behavior: **this is the highest-risk item.** If the synthesizer were not
      updated, every dry-run implement would flip to `request_changes` and the failure
      would look like a gate bug. Confirm the happy-path integration check below passes

### D6: The fail-closed synthetic artifact is schema-uniform
**Expected:** `implement.rs:753-772` writes an `## Acceptance` block too.
- [ ] The synthetic artifact for a missing/empty review has both `## Verdict` and
      `## Acceptance` sections
- [ ] No regressions: it still sets `request_changes` and still sets `any_request_changes`

### D7: Decisions and docs
- [ ] `DECISIONS.md` `## Orchestration` table has O19, O20, O21 with status `DECIDED`,
      matching the existing 3-column format with `\|` escaped inside cells
- [ ] `skills/core.md` config-knobs block (:195-235) documents `[review]`
- [ ] `skills/core.md` documents the reviewer artifact schema — it is a public schema
      that outer agents read
- [ ] `spar.toml.example` has a commented `[review]` block

## Automated checks

```bash
cd ../spar-feat-acceptance
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
- [ ] All pass
- [ ] `stuck_policy_dry_run_request_changes` (tests/scenarios/plan_implement.rs:798)
      passes and still asserts `revs >= 3` — the ladder (fix rounds → rotate implementer
      → widen reviewers → `Phase::Stuck`) is intact and only its fixture changed
- [ ] `implement_dry_run_writes_suite_artifact` (:319) and
      `implement_dry_run_surfaces_suite_outcome` (:368) pass
- [ ] The new `implement_dry_run_missing_ac_requests_changes` and
      `implement_dry_run_unverified_ships_when_relaxed` exist and pass

## Integration checks

```bash
cd /tmp && rm -rf spar-v3 && mkdir spar-v3 && cd spar-v3 && git init -q && \
  git commit -q --allow-empty -m init
spar plan --dry-run --providers cli:claude,cli:grok,cli:agy -t "add a hello function"
RUN=$(ls -t .spar/runs | head -1)
spar approve $RUN
spar implement --run $RUN --dry-run --providers cli:claude,cli:grok,cli:agy; echo "exit=$?"
cat .spar/runs/$RUN/artifacts/review-*.md
jq '.phase' .spar/runs/$RUN/state.json
```
- [ ] Every `review-*.md` has both a `## Verdict` and an `## Acceptance` section
- [ ] The happy-path run reaches `AwaitingShipConfirm` with exit code `2` (human gate) —
      **not** an endless `request_changes` loop
- [ ] Exit codes are unchanged as a public contract: `0` ok, `1` fail, `2` human gate,
      `3` stuck, `4` quota
- [ ] A run with `[spec].enabled = false` (no contract) still ships on an `approve`
      verdict — the empty-criteria rule works end to end
- [ ] With `[review] require_all_criteria = false` in `spar.toml`, an `unverified` AC
      does not block

## Notes

[Leave blank — the verifier fills this in with findings]
