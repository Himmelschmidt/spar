# Priority 9: Role resolution rewire

## Goal

Consume the `[roles]` block: replace positional index assignment with one
`provider_for(role, idx, fleet, cfg)` function, make a populated `[roles]` satisfy the
"`--providers` or `--select` is required" invariant, and give reviewer widening and
implementer rotation a sane source of candidates instead of hardcoded provider lists.

Depends on Priority 8 (the config block and role vocabulary) and Priority 5 (`@model`
refs). **The tree does not compile between Priority 8 Step 6 and this priority's Step 4**
— `resolve_suite_provider` and `resolve_spec_provider` reference config fields that
Priority 8 deletes.

## Approach

Precedence, exactly as specified:

**explicit `--providers` (positional one-off) > `[roles]` > `[providers].order`**

One function replaces seven open-coded assignment sites:

```rust
pub fn provider_for(role: SlotRole, idx: usize, fleet: &[String], cfg: &Config) -> Option<String>
```

The invariant change is the notable one. Today `--providers` or `--select` is enforced at
**three** places with **three different messages**:

| # | Location | Message |
|---|---|---|
| 1 | `src/model_select/mod.rs:34-51` (primary, reached by every flow) | `--providers is required (or pass --select <profile>, e.g. --select value)` |
| 2 | `src/workflow/implement.rs:147-149` (redundant second guard) | `--providers is required` |
| 3 | `src/workflow/mod.rs:51-62` (`require_providers`) | already `#[allow(dead_code)]` |

Consolidate to one. A populated `[roles]` becomes a third way to satisfy it.

---

## Steps

### Step 1: Add the resolver
**File:** `/home/sholom/projects/spar/src/workflow/roles_resolve.rs` (new)

Name it `roles_resolve`, **not** `roles` — `src/workflow/roles.rs` already exists as the
frontend/backend Peer workflow and is unrelated.

Implement `provider_for` with the three-tier precedence above. For `SlotRole::Reviewer`,
`idx` selects into `cfg.roles.reviewer` (the list), falling back to `[providers].order`
when the list is exhausted.

Register with `pub mod roles_resolve;` in `/home/sholom/projects/spar/src/workflow/mod.rs`.

Inline tests:

| Test | Asserts |
|------|---------|
| `explicit_fleet_wins_over_roles` | non-empty `fleet` beats a populated `[roles]` |
| `roles_used_when_fleet_empty` | empty `fleet` + `[roles].implementer` → that provider |
| `provider_order_is_last_resort` | empty `fleet` + empty `[roles]` → `[providers].order[idx]` |
| `reviewer_list_indexes` | `idx` 0 and 1 return distinct entries from `cfg.roles.reviewer` |
| `reviewer_list_exhausted_falls_back` | `idx` past the list end falls to `[providers].order` |

- [ ] Done

### Step 2: Make `[roles]` satisfy the invariant
**File:** `/home/sholom/projects/spar/src/model_select/mod.rs:25-91` (`resolve_providers`)

This is the single choke point every flow reaches via `CommonOpts::resolve_fleet`
(`src/workflow/mod.rs:65-123`; call sites at `plan.rs:38`, `implement.rs:89` and `:416`,
`arena.rs:34`, `review.rs:46`, `roles.rs:37`, `peer.rs:37`).

Add: when `providers` is empty **and** `select` is `None` **and** `cfg.roles` is not
empty, synthesize the fleet from `[roles]` for the requested `roles: &[&str]` labels and
return it.

Update the error messages at :34-51 to mention `[roles]` as a third option.

**Exceptions — do NOT change:**
- `bail!("use either --providers or --select, not both")` — still mutually exclusive.
- `bail!("--select requires at least one profile name (or auto)")`.
- `bail!("need at least one slot to select")`.

- [ ] Done

### Step 3: Remove the redundant guards
**Files:**

| File | Lines | Change |
|---|---|---|
| `/home/sholom/projects/spar/src/workflow/mod.rs` | :51-62 | delete `CommonOpts::require_providers` — already `#[allow(dead_code)]` |
| `/home/sholom/projects/spar/src/workflow/implement.rs` | :147-149 | delete the second `bail!("--providers is required")` |

Confirm nothing calls `require_providers` before deleting:
```bash
grep -rn 'require_providers' /home/sholom/projects/spar/src/
```
Expected: only the definition.

- [ ] Done

### Step 4: Rewire the suite and spec provider resolvers
**Files:**

4a. `/home/sholom/projects/spar/src/workflow/implement.rs:228-277` (`resolve_suite_provider`)
    — drop the `[suite].provider` override branch (:228-232). Note that branch **skipped
    the usability check** that the spec path performs; the replacement must not. Put
    `[roles].tester` at the head of the existing fallback chain **with** a usability check.
    Keep the rest of the chain: model-select artifact → `pick_one_for_role` → `PREFS`
    (:259) → first usable fleet provider. Update the bail message to reference
    `[roles].tester`.

4b. `/home/sholom/projects/spar/src/workflow/plan.rs:375-418` (`resolve_spec_provider`)
    — same, with `[roles].test_author`. Its override branch (:381-388) already checks
    usability and falls through when unusable; **preserve that behavior** for the new
    source. Update the bail message.

- [ ] Done

### Step 5: Rewire the plan flow
**File:** `/home/sholom/projects/spar/src/workflow/plan.rs`

5a. **:91-121** — replace `i == 0 ? planner : critic` with
    `provider_for(SlotRole::Planner, 0, …)` and `provider_for(SlotRole::PlanCritic, 1, …)`.

5b. **:524-546** — **the same logic is duplicated** on the re-plan path. Both copies must
    change. Consider extracting the shared body into one function first, then rewiring
    once; two copies of role assignment is exactly how the next drift bug is born.

5c. **:32-38** — the role label vector passes `"tester"` for what becomes a
    `SlotRole::TestAuthor` slot. Change to `"test_author"`, and update the lookup at
    **:254** (`c.role.as_deref() == Some("tester") || c.slot == 2`) to match. This is the
    disambiguation that lets `[roles].tester` and `[roles].test_author` be distinct.

    Use `SlotRole::as_config_key()` (Priority 8, Step 2) rather than a bare string literal.

- [ ] Done

### Step 6: Rewire the implement flow
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs:147-193`

6a. Delete the pad `while provs.len() < 3 { provs.push(provs[0].clone()); }` (:157-159)
    and the hardcoded `provs[0]` / `provs[1]` / `provs[2]` indexing; use `provider_for`.

6b. Introduce `const DEFAULT_REVIEWERS: usize = 2;`. The count is currently implied by
    three unrelated coincidences — the `while` pad, the two literal `Reviewer` pushes at
    :181-192, and `cfg.max_agents.max(3)` appearing in **three** copies (:84, :150, :411).
    Name it once.

6c. Slot ids `review-{…}-a` / `-b` become indexed `review-{n}`, derived from the provider
    **name only** (already sanitized by Priority 4) — **never** from the model. Two slots
    on `cli:codex@a` and `cli:codex@b` must still get distinct ids; the index provides that.

6d. Update the role label vector at :85-88 and :411-415
    (`once("implementer").chain(repeat("reviewer"))`) to use `as_config_key()`.

**Exceptions — do NOT change:**
- The reviewer consumption at :704-712 — it already filters by `role == SlotRole::Reviewer`
  rather than by index, which is correct.
- The escalation ladder at :808-828 and `max_fix_rounds` (:410).

- [ ] Done

### Step 7: Rewire widening and rotation
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs`

| Function | Lines | Change |
|---|---|---|
| `try_widen_reviewers` | :876-921 | replace the hardcoded `["cli:claude","cli:grok","cli:agy","cli:claude","cli:grok"]` candidate list with `cfg.roles.reviewer` first, then `cfg.providers.order`, then the fleet |
| `try_rotate_implementer` | :834-874 | replace its hardcoded `["cli:claude","cli:grok","cli:agy"]` with `[roles].implementer` then `cfg.providers.order` |
| `try_rotate_reviewer_provider` | :923-956 | already consults `cfg.providers.order` at :944 — just insert `cfg.roles.reviewer` ahead of it |

**Exceptions — do NOT remove:**
- The synthetic duplicate-provider fallback in `try_widen_reviewers` at :899-911, which
  appends `review-{len}-wide` and returns `true` unconditionally. The escalation ladder at
  :808-828 depends on widening succeeding exactly once. After this change it should be
  genuinely reachable only when the reviewer list is exhausted, which is the improvement —
  but it must still exist.

- [ ] Done

### Step 8: Confirm what is out of scope
**Files:** `src/workflow/arena.rs:53-70`, `src/workflow/peer.rs:52-66`,
`src/workflow/review.rs:60-66`, `src/workflow/roles.rs:52-66`

These stay positional. N arena competitors, two peers, and frontend/backend are positional
*by nature*; role-keying them adds a config surface with no meaning behind it. They
already received Priority 4's sanitize fix. Leave them alone.

- [ ] Done (confirmed no changes needed)

### Step 9: Add a scenario test
**File:** `/home/sholom/projects/spar/tests/scenarios/roles_config.rs` (new)

**File:** `/home/sholom/projects/spar/Cargo.toml` (:47-72) — **this new file requires a
`[[test]]` block with an explicit `path`. There is no autodiscovery. Without the block the
file silently never runs.**

```toml
[[test]]
name = "roles_config"
path = "tests/scenarios/roles_config.rs"
```

Reuse the helpers from `tests/scenarios/plan_implement.rs`: `spar_home_dir()` (:9),
`spar_cmd()` (:20), `init_git_repo()` (:26), `primary_branch()` (:55).

| Test | Asserts |
|------|---------|
| `roles_config_satisfies_provider_invariant` | a project `spar.toml` with `[roles]` and **no** `--providers` runs `plan --dry-run` successfully |
| `roles_config_assigns_by_role` | `state.json` slots carry the provider named for each role, not positional order |
| `explicit_providers_override_roles` | `--providers` beats a populated `[roles]` |
| `bad_role_ref_errors_cleanly` | a malformed ref in `[roles]` exits non-zero with a message naming the role — **not a panic** |
| `reviewer_widening_draws_from_role_list` | drive the stuck ladder and assert the widened reviewer comes from `[roles].reviewer`, not the hardcoded default |

- [ ] Done

### Step 10: Check existing scenario tests
```bash
cd ../spar-feat-roles
cargo test 2>&1 | tail -40
```

Watch specifically for:
- `tests/scenarios/plan_implement.rs:118, :200, :363` — assert `state.json` role strings
  (`test_author`, `implementer`, `tester`). `SlotRole::TestAuthor` already serializes as
  `test_author`, so these should be unaffected by Step 5c (only the internal `&[&str]`
  label moved) — **verify rather than assume.**
- Any test asserting the `review-…-a` / `-b` slot id shape, which Step 6c changes.
- `stuck_policy_dry_run_request_changes` (:798), which drives widening and rotation.

- [ ] Done

### Step 11: Record the decisions
**File:** `/home/sholom/projects/spar/DECISIONS.md`

Append to the `## Orchestration` table (after O19-O21 from Priority 3):

```
| O22 | **Role-keyed fleet.** `[roles]` assigns providers by role; precedence is explicit `--providers` (positional) > `[roles]` > `[providers].order`. A populated `[roles]` satisfies the "`--providers` or `--select` is required" invariant. `[suite].provider` and `[spec].provider` are removed in favour of `[roles].tester` and `[roles].test_author` — clean break, no shim | DECIDED |
| O23 | **One role vocabulary**: config role keys are `SlotRole` snake_case (`planner, plan_critic, implementer, reviewer, tester, test_author`). `[model_select.roles]` is renamed `[model_select.role_profiles]` to end the collision with top-level `[roles]` | DECIDED |
```

- [ ] Done

### Step 12: Update the operator skill
**File:** `/home/sholom/projects/spar/skills/core.md`

12a. **:192** — the claim "**Providers:** always pass `--providers` explicitly. A single
     name is fine…; multiple names cycle across slots" is now wrong on both halves.
     Rewrite it to describe the three-tier precedence and note that `[roles]` satisfies
     the requirement.

12b. **:237-245** (`## Rules of the road`) — update the spec-channel and suite-channel
     bullets, which describe `[spec] enabled = false` and tester-slot behavior in terms of
     the removed `provider` keys.

12c. Confirm the `[roles]` block added in Priority 8, Step 8 is present in the
     config-knobs block (:195-235).

- [ ] Done

### Step 13: Verify
```bash
cd ../spar-feat-roles
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then prove role assignment end to end with no `--providers` at all:
```bash
cd /tmp && rm -rf spar-p9 && mkdir spar-p9 && cd spar-p9 && git init -q && \
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
spar plan --dry-run -t "add a hello function"
jq '.slots[] | {id, role, provider}' .spar/runs/*/state.json
```
Expected: the command succeeds **without `--providers`**; the planner slot is
`cli:claude`, the critic `cli:grok`, the test author `cli:grok`. Then confirm the override:
```bash
spar plan --dry-run --providers cli:agy,cli:claude -t "another task"
```
Expected: positional assignment wins — planner is `cli:agy`.

- [ ] Done
