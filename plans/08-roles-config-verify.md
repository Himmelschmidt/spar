# Verify Priority 8: Roles config block

> This file is for verifying the work done in [08-roles-config.md](08-roles-config.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

A `[roles]` config section was added with `planner`, `plan_critic`, `implementer`,
`reviewer` (a list), `tester`, and `test_author`, validated at load time.
`[model_select.roles]` was renamed to `[model_select.role_profiles]` to end the naming
collision. `SlotRole::as_config_key()` / `from_config_key()` were added as the single
source of truth for role key strings. `[suite].provider` and `[spec].provider` were
removed with no compat shim.

## Deliverables

### D1: One canonical role vocabulary
**Expected:** config keys match `SlotRole`'s serde representation exactly.
- [ ] `SlotRole::as_config_key()` and `from_config_key()` exist in `src/state.rs:225-239`
- [ ] Correct behavior: a test serializes every `SlotRole` variant to JSON and asserts it
      equals `as_config_key()`. Without this the two representations will drift
- [ ] The keys are `planner, plan_critic, test_author, implementer, tester, reviewer,
      ranker, peer, reconciler`
- [ ] Correct behavior: `critic` is **not** accepted as an alias for `plan_critic`.
      `grep -rn '"critic"' src/` returns nothing. Aliases are how two vocabularies become
      three

### D2: The `[roles]` block parses
**Expected:** all six keys overlay correctly.
- [ ] A project `spar.toml` `[roles]` block populates `cfg.roles`, including `reviewer`
      as a list
- [ ] `Config::default().roles.is_empty()` is true
- [ ] Correct behavior: all five shadow-struct touch points exist — `RolesConfig`, field
      on `Config`, `RolesConfigFile`, field on `ConfigFile`, and a merge arm in
      `apply_file`. **Missing the merge arm makes the whole block silently do nothing**,
      which a default-value test would not catch. Verify by loading a real config file
- [ ] `[roles] implementer = "cli:codex@openai/gpt-4o-mini"` parses — the block accepts
      `@model` refs (Priority 5)

### D3: Load-time validation, not a panic
**Expected:** a bad ref errors with the role key named.
- [ ] `[roles] implementer = "claude"` (no backend prefix) produces a config error whose
      message contains `implementer`
- [ ] Correct behavior: it is an **error, not a panic**. Without load-time validation the
      `.expect()` in `init_slot_model` (`src/executor.rs:1380-1405`) becomes reachable
      from a typo in `spar.toml`. Test this for real, per the integration check below

### D4: The naming collision is gone
**Expected:** `[model_select.roles]` is renamed.
- [ ] `grep -rn 'model_select.roles' src/ skills/ docs/ spar.toml.example` returns nothing
- [ ] The field is `role_profiles` in `src/config.rs:70-72`, with `role_profile()`
      (:105-115) and the defaults fn (:140-148) updated
- [ ] `spar.toml.example:53-57` and `skills/core.md:224-235` use the new name
- [ ] Correct behavior: the old `[model_select.roles]` key is simply **ignored** if
      present — no shim, no error, per the no-compat-shim rule
- [ ] No regressions: the internal `roles: &[&str]` argument to `resolve_fleet`
      (`src/workflow/mod.rs:65-123`) was **not** renamed — it is an unrelated positional
      label vector

### D5: `[suite].provider` and `[spec].provider` are gone
**Expected:** clean break, no shim.
- [ ] `grep -n 'provider' src/config.rs | grep -i 'suite\|spec'` returns nothing
- [ ] No regressions: `SuiteConfig.enabled` and `.timeout_secs` survive — they are read at
      `implement.rs:203, :607, :608` and `executor.rs:512, :801`
- [ ] No regressions: `SpecConfig.enabled` and `.timeout_secs` survive — read at
      `plan.rs:32, :33, :199` and `executor.rs:513, :802`
- [ ] Correct behavior: **no deprecation shim, no fallback read, no warning path** exists
      for the removed keys

### D6: Tests updated, not deleted
**Expected:** the broken test was rewritten.
- [ ] `suite_and_review_timeout_overlay` (`src/config.rs:618-650`) still exists and now
      asserts `cfg.roles.tester` / `cfg.roles.test_author` instead of
      `cfg.suite.provider` / `cfg.spec.provider`
- [ ] Correct behavior: its **timeout assertions were kept** — the test covers both
      concerns and only the provider half changed
- [ ] The new tests `roles_block_overlays`, `roles_default_is_empty`,
      `roles_reject_bad_ref`, `roles_accept_model_ref`, and `role_profiles_renamed` exist

## Automated checks

```bash
cd ../spar-feat-roles
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test config
cargo test
```
- [ ] All pass

**Note on build state:** Priority 8 Step 6 deletes config fields that
`resolve_suite_provider` (`implement.rs:228-277`) and `resolve_spec_provider`
(`plan.rs:375-418`) still read, so the tree may not compile between that step and
Priority 9 Step 4.
- [ ] If `cargo build` fails, the **only** errors are those two resolver functions
      referencing the removed fields — no other breakage. If Priority 9 Step 4 was pulled
      forward to keep the tree compiling, that is acceptable and preferred

## Integration checks

```bash
cd /tmp && rm -rf spar-v8 && mkdir spar-v8 && cd spar-v8 && git init -q && \
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
spar doctor --json | jq .
sed -i 's/^planner = "cli:claude"/planner = "claude"/' spar.toml
spar doctor --json; echo "exit=$?"
```
- [ ] The valid config loads clean
- [ ] The typo produces an error naming `planner`, exits non-zero, and shows **no panic
      backtrace**
- [ ] `spar.toml.example` documents `[roles]` and no longer shows `provider` under
      `[suite]` or `[spec]`
- [ ] `skills/core.md` config-knobs block documents `[roles]` and distinguishes it from
      `[model_select.role_profiles]` — a future reader will otherwise conflate them
- [ ] This priority added the data model only. Confirm no workflow consumes `cfg.roles`
      yet: `grep -rn 'cfg.roles\|\.roles\.' src/workflow/` is empty (that is Priority 9)

## Notes

[Leave blank — the verifier fills this in with findings]
