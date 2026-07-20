# Priority 8: Roles config block

## Goal

Add a `[roles]` config section assigning providers by role instead of by positional index,
unify the role vocabulary onto one canonical set, and **remove** `[suite].provider` and
`[spec].provider` in favour of `[roles].tester` and `[roles].test_author`. Clean break, no
compat shim — explicitly approved by the user.

This priority lands the data model and the config plumbing. Priority 9 rewires the
workflows to consume it. Splitting them keeps each gate run interpretable: if Priority 8
is green, config parses and validates; if Priority 9 is green, the fleet actually assigns.

Depends on Priority 5 — `[roles]` values are `@model`-capable provider refs.

## Approach

Three intertwined changes that must land together because they share a vocabulary:

**1. The `[roles]` block.**
```toml
[roles]
planner = "cli:claude"
plan_critic = "cli:grok"
implementer = "cli:codex@anthropic/claude-opus-4.5"
reviewer = ["cli:grok", "cli:agy", "cli:claude"]
tester = "cli:agy"
test_author = "cli:grok"
```

**2. The naming collision.** `ModelSelectConfig` already has a field literally named
`roles: HashMap<String, String>` (`src/config.rs:70-72`) mapping role name → *profile*
name. A new top-level `[roles]` would sit beside `[model_select.roles]` meaning something
entirely different — provider assignment vs benchmark-profile selection. **Rename
`[model_select.roles]` to `[model_select.role_profiles]`**, which is both collision-free
and a more accurate name for a role→profile map. Blast radius is small and fully
enumerated: `config.rs:70-72`, `:105-115` (`role_profile`), `:140-148` (defaults),
`spar.toml.example:53-57`, `skills/core.md:224-235`.

**3. One role vocabulary.** Today there are two. Config/model-select uses
`planner, critic, implementer, reviewer, tester`; `SlotRole` (`src/state.rs:225-239`)
serializes as `planner, plan_critic, test_author, implementer, tester, reviewer, ranker,
peer, reconciler`. Worse, the plan flow passes the role label `"tester"` for a slot that
becomes `SlotRole::TestAuthor` (`plan.rs:32-38`, `:254`) — so `tester` currently means
two different things depending on flow, and the requested `[roles]` block needs both
`tester` and `test_author` as distinct keys.

**Resolution: `SlotRole` snake_case is canonical.** `critic` becomes `plan_critic`; the
plan flow's `"tester"` label becomes `"test_author"`, freeing `tester` to unambiguously
mean the suite runner. Add `SlotRole::as_config_key()` / `from_config_key()` as the single
source of truth. Do **not** accept `critic` as an alias — aliases are how two vocabularies
become three.

---

## Steps

### Step 1: Create the worktree

```bash
cd /home/sholom/projects/spar
git worktree add ../spar-feat-roles -b feat/roles-config
```

Priorities 8-9 happen in `../spar-feat-roles`. Branch from a base that includes Priority 5.

- [ ] Done

### Step 2: Add the canonical role key mapping
**File:** `/home/sholom/projects/spar/src/state.rs:225-239`

Add to `SlotRole`:
```rust
pub fn as_config_key(&self) -> &'static str;
pub fn from_config_key(s: &str) -> Option<SlotRole>;
```

These must return exactly the existing `#[serde(rename_all = "snake_case")]` strings —
`planner`, `plan_critic`, `test_author`, `implementer`, `tester`, `reviewer`, `ranker`,
`peer`, `reconciler` — so `state.json` and config keys share one vocabulary.

Add an inline test asserting `as_config_key` agrees with the serde representation for
every variant (serialize to JSON and compare), so the two can never drift.

- [ ] Done

### Step 3: Add the `[roles]` config section
**File:** `/home/sholom/projects/spar/src/config.rs`

```rust
pub struct RolesConfig {
    pub planner: Option<String>,
    pub plan_critic: Option<String>,
    pub implementer: Option<String>,
    pub reviewer: Vec<String>,
    pub tester: Option<String>,
    pub test_author: Option<String>,
}
```

The full shadow-struct ritual, mirroring the `[suite]` arm at :483:

3a. `RolesConfig` + `Default` impl near `SuiteConfig` (:250-274).
3b. Field on `Config` (:7-40): `#[serde(default)] pub roles: RolesConfig,`
3c. `RolesConfigFile` with all-`Option` fields near :360-432, plus a field on
    `ConfigFile` (:360-376).
3d. Merge arm in `apply_file` (:449-567). Trust: `Trust::Project`.

Add `RolesConfig::is_empty()` — Priority 9 needs it for the invariant check.

`reviewer` is a `Vec<String>`, not an `Option` — an empty vec is the natural "unset".

- [ ] Done

### Step 4: Validate refs at config load
**File:** `/home/sholom/projects/spar/src/config.rs`

Every non-empty `[roles]` value must go through `ProviderRef::parse` during
`apply_file`/`load`, producing an error that **names the offending role key**, e.g.
`invalid provider in [roles].implementer: …`.

This is what keeps the `.expect()` in `init_slot_model` (`src/executor.rs:1380-1405`)
unreachable from user input. Without it, a typo in `spar.toml` panics the binary instead
of printing a config error.

- [ ] Done

### Step 5: Rename `[model_select.roles]`
**Files:**

| File | Lines | Change |
|---|---|---|
| `/home/sholom/projects/spar/src/config.rs` | :70-72 | field `roles` → `role_profiles` |
| `/home/sholom/projects/spar/src/config.rs` | :105-115 | `role_profile()` match — update the field reference and the role key strings (`critic` → `plan_critic`) |
| `/home/sholom/projects/spar/src/config.rs` | :140-148 | `default_model_select_roles` → `default_model_select_role_profiles`, keys updated |
| `/home/sholom/projects/spar/spar.toml.example` | :53-57 | `[model_select.roles]` → `[model_select.role_profiles]` |
| `/home/sholom/projects/spar/skills/core.md` | :224-235 | same rename in the config-knobs block |

```bash
grep -rn 'model_select.roles\|\.roles\b' /home/sholom/projects/spar/src/ /home/sholom/projects/spar/skills/ /home/sholom/projects/spar/docs/
```
Expected: every hit is either the renamed field or the new top-level `cfg.roles`.

**Exceptions — do NOT rename:**
- The internal `roles: &[&str]` argument to `resolve_fleet` (`src/workflow/mod.rs:65-123`)
  and its call sites — that is a positional label vector, unrelated. Its *values* change
  in Priority 9, not its name.

- [ ] Done

### Step 6: Remove `[suite].provider` and `[spec].provider`
**File:** `/home/sholom/projects/spar/src/config.rs`

Delete the `provider: Option<String>` field from `SuiteConfig` (:250-274) and
`SpecConfig` (:276-300), plus the corresponding fields in `SuiteConfigFile` (:413-418),
`SpecConfigFile` (:420-425), and their merge arms in `apply_file`.

**Exceptions — do NOT remove:**
- `SuiteConfig.enabled` / `SuiteConfig.timeout_secs` — read at `implement.rs:203, :607,
  :608` and `executor.rs:512, :801`.
- `SpecConfig.enabled` / `SpecConfig.timeout_secs` — read at `plan.rs:32, :33, :199` and
  `executor.rs:513, :802`.

The two resolver functions that consume these (`resolve_suite_provider` at
`implement.rs:228-277`, `resolve_spec_provider` at `plan.rs:375-418`) will fail to compile
until Priority 9, Step 4 rewires them. That is expected and is why these two priorities
must be developed together even though they are gated separately — **the tree does not
build between Step 6 and Priority 9 Step 4.** If you prefer a compiling intermediate,
do Priority 9 Step 4 immediately after this step and before the tests below.

- [ ] Done

### Step 7: Update config tests
**File:** `/home/sholom/projects/spar/src/config.rs` (inline `mod tests`, :591-683)

**Will break — must be updated:** `suite_and_review_timeout_overlay` (:618-650) asserts
`cfg.suite.provider == Some("cli:grok")` and `cfg.spec.provider == Some("cli:agy")`.
Rewrite it to assert `cfg.roles.tester` and `cfg.roles.test_author` instead. **Keep the
timeout assertions** — those still hold.

Also check `partial_project_overlays_user` (:596-616) for `[providers]` assumptions.

New tests:

| Test | Asserts |
|------|---------|
| `roles_block_overlays` | a project `spar.toml` `[roles]` block parses into `cfg.roles`, including the `reviewer` list |
| `roles_default_is_empty` | `Config::default().roles.is_empty()` |
| `roles_reject_bad_ref` | `[roles] implementer = "claude"` (no backend prefix) errors, and the message names `implementer` |
| `roles_accept_model_ref` | `[roles] implementer = "cli:codex@openai/gpt-4o-mini"` parses |
| `role_profiles_renamed` | `[model_select.role_profiles]` overlays; the old `[model_select.roles]` key is simply ignored (no shim, no error) |

- [ ] Done

### Step 8: Update docs
**Files:**

| File | Change |
|---|---|
| `/home/sholom/projects/spar/spar.toml.example` | Add a commented `[roles]` block; remove `# provider = …` from `[suite]` (:29-33) and `[spec]` (:35-39) |
| `/home/sholom/projects/spar/skills/core.md` | Config-knobs block (:195-235): add `[roles]`, drop `provider` from `[suite]` (:210-213) and `[spec]` (:215-218) |

Note the `[roles]` vs `[model_select.role_profiles]` distinction explicitly in the skill —
a future reader will otherwise conflate them.

- [ ] Done

### Step 9: Verify
```bash
cd ../spar-feat-roles
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test config
cargo test
```

Then:
```bash
cd /tmp && rm -rf spar-p8 && mkdir spar-p8 && cd spar-p8 && git init -q && \
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
# then a deliberate typo:
sed -i 's/cli:claude/claude/' spar.toml
spar doctor --json
```
Expected: the valid config loads clean; the typo produces a config error naming the role
key, **not** a panic.

- [ ] Done
