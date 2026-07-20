# Verify Priority 9: Role resolution rewire

> This file is for verifying the work done in [09-role-resolution-rewire.md](09-role-resolution-rewire.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

A new `src/workflow/roles_resolve.rs` provides one `provider_for(role, idx, fleet, cfg)`
implementing the precedence `--providers` > `[roles]` > `[providers].order`. A populated
`[roles]` now satisfies the "`--providers` or `--select` is required" invariant at the
single choke point in `src/model_select/mod.rs`. The plan and implement flows, the suite
and spec resolvers, reviewer widening, and implementer rotation were all rewired off
positional indexing and hardcoded provider lists. Two redundant invariant guards were
deleted. DECISIONS gained O22 and O23.

## Deliverables

### D1: One resolver with correct precedence
**Expected:** `provider_for` implements the three tiers.
- [ ] `src/workflow/roles_resolve.rs` exists and is registered in `src/workflow/mod.rs`
- [ ] Named `roles_resolve`, **not** `roles` — `src/workflow/roles.rs` already exists as
      the unrelated Peer workflow and was not clobbered. Confirm both files exist
- [ ] A non-empty `fleet` beats a populated `[roles]`
- [ ] An empty `fleet` uses `[roles]`
- [ ] Both empty falls to `[providers].order`
- [ ] `SlotRole::Reviewer` indexes into the `cfg.roles.reviewer` list, and an `idx` past
      the list end falls through rather than panicking

### D2: `[roles]` satisfies the invariant, at one place
**Expected:** the requirement is enforced once.
- [ ] `src/model_select/mod.rs:25-91` synthesizes a fleet from `[roles]` when both
      `--providers` and `--select` are absent and `cfg.roles` is non-empty
- [ ] Its error messages mention `[roles]` as a third option
- [ ] `grep -rn 'providers is required' src/` returns exactly **one** hit — the redundant
      guard at `implement.rs:147-149` and the dead `require_providers`
      (`src/workflow/mod.rs:51-62`) are both gone
- [ ] No regressions: `use either --providers or --select, not both` still bails —
      they remain mutually exclusive
- [ ] No regressions: with **no** `--providers`, **no** `--select`, and an **empty**
      `[roles]`, the command still fails with a clear message. The invariant was relaxed,
      not removed

### D3: Positional assignment is gone from plan and implement
**Expected:** roles drive assignment.
- [ ] `src/workflow/plan.rs:91-121` uses `provider_for`, not `i == 0 ? planner : critic`
- [ ] **`src/workflow/plan.rs:524-546` — the duplicate on the re-plan path — was also
      changed.** Verify both. A fix applied to one copy is the next drift bug
- [ ] `src/workflow/implement.rs:147-193`: the pad
      `while provs.len() < 3 { provs.push(provs[0].clone()) }` is gone, and
      `provs[0]`/`provs[1]`/`provs[2]` indexing is replaced
- [ ] `const DEFAULT_REVIEWERS: usize = 2;` exists — the count is named once instead of
      implied by the pad, two literal pushes, and `cfg.max_agents.max(3)` in three places
- [ ] Reviewer slot ids derive from the provider **name only**, never the model. Two slots
      on `cli:codex@a` and `cli:codex@b` still get distinct ids via the index
- [ ] No regressions: reviewer consumption at `implement.rs:704-712` still filters by
      `role == SlotRole::Reviewer`, not by index

### D4: The `tester` / `test_author` disambiguation
**Expected:** the plan flow's label no longer says `tester`.
- [ ] `src/workflow/plan.rs:32-38` passes `"test_author"`, and the lookup at :254 matches
- [ ] Correct behavior: `[roles].tester` (suite runner) and `[roles].test_author`
      (pre-coding acceptance tests) are now genuinely distinct and each reaches the right slot
- [ ] Label strings come from `SlotRole::as_config_key()`, not bare literals
- [ ] No regressions: `state.json` role strings are unchanged — `SlotRole::TestAuthor`
      already serialized as `test_author`, so `tests/scenarios/plan_implement.rs:118, :200,
      :363` should pass untouched. **Verify rather than assume**

### D5: Widening and rotation draw from config
**Expected:** no hardcoded provider lists remain in the escalation path.
- [ ] `try_widen_reviewers` (:876-921) no longer contains
      `["cli:claude","cli:grok","cli:agy","cli:claude","cli:grok"]`; it draws from
      `cfg.roles.reviewer`, then `cfg.providers.order`, then the fleet
- [ ] `try_rotate_implementer` (:834-874) no longer contains its hardcoded default list
- [ ] `try_rotate_reviewer_provider` (:923-956) consults `cfg.roles.reviewer` ahead of
      `cfg.providers.order`
- [ ] **No regressions — critical:** the synthetic duplicate-provider fallback in
      `try_widen_reviewers` (:899-911) still exists and still returns `true`. The
      escalation ladder at :808-828 depends on widening succeeding exactly once; deleting
      it would wedge runs at `Phase::Stuck` early
- [ ] No regressions: `max_fix_rounds` (:410) and the ladder order (fix rounds → rotate
      implementer → widen reviewers → `Phase::Stuck`) are unchanged

### D6: Suite and spec resolvers rewired
**Expected:** they read `[roles]`, and the usability gap is closed.
- [ ] `resolve_suite_provider` (`implement.rs:228-277`) reads `[roles].tester` at the head
      of its chain **with a usability check** — the old `[suite].provider` branch at
      :228-232 skipped that check; the replacement must not
- [ ] `resolve_spec_provider` (`plan.rs:375-418`) reads `[roles].test_author` and preserves
      its existing fall-through-if-unusable behavior
- [ ] Both bail messages reference the new config keys
- [ ] No regressions: the rest of each fallback chain survives — model-select artifact →
      `pick_one_for_role` → `PREFS` (:259) → first usable fleet provider

### D7: Out-of-scope workflows untouched
- [ ] `git diff main -- src/workflow/arena.rs src/workflow/peer.rs src/workflow/review.rs src/workflow/roles.rs`
      shows only Priority 4's sanitize changes, no role-keying. These are positional by
      nature and were correctly left alone

### D8: Decisions and docs
- [ ] `DECISIONS.md` has O22 and O23 with status `DECIDED`
- [ ] `skills/core.md:192` — the claim "always pass `--providers` explicitly… multiple
      names cycle across slots" was rewritten. **Both halves were wrong after this change**
- [ ] `skills/core.md:237-245` spec/suite channel bullets no longer describe the removed
      `provider` keys

## Automated checks

```bash
cd ../spar-feat-roles
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
- [ ] All pass
- [ ] `Cargo.toml` has a `[[test]] name = "roles_config"` block with an explicit
      `path = "tests/scenarios/roles_config.rs"`. **Without it the new scenario file
      silently never runs** — confirm the tests actually execute:
      `cargo test --test roles_config -- --list`
- [ ] All five new scenario tests appear in that listing and pass
- [ ] `stuck_policy_dry_run_request_changes` (`plan_implement.rs:798`) passes — it drives
      widening and rotation, both of which changed

## Integration checks

```bash
cd /tmp && rm -rf spar-v9 && mkdir spar-v9 && cd spar-v9 && git init -q && \
  git commit -q --allow-empty -m init
cat > spar.toml <<'EOF'
[roles]
planner = "cli:claude"
plan_critic = "cli:grok"
implementer = "cli:claude"
reviewer = ["cli:grok", "cli:agy"]
tester = "cli:agy"
test_author = "cli:grok"
EOF
spar plan --dry-run -t "add a hello function"; echo "exit=$?"
jq '.slots[] | {id, role, provider}' .spar/runs/*/state.json
```
- [ ] The command **succeeds with no `--providers`** — this is the invariant change
- [ ] Planner is `cli:claude`, critic `cli:grok`, test author `cli:grok` — assignment is
      by role, not by list order

```bash
spar plan --dry-run --providers cli:agy,cli:claude -t "another task"
jq '.slots[] | {role, provider}' .spar/runs/*/state.json | head
```
- [ ] Planner is now `cli:agy` — explicit `--providers` beats `[roles]`
- [ ] With an empty `spar.toml`, `[providers].order` still drives assignment (tier 3)
- [ ] Exit codes are unchanged as a public contract: `0` ok, `1` fail, `2` human gate,
      `3` stuck, `4` quota

## Notes

[Leave blank — the verifier fills this in with findings]
