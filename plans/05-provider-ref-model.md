# Priority 5: ProviderRef @model

## Goal

Let any provider ref carry an optional model: `cli:codex@openai/gpt-4o-mini`,
`api:openai@gpt-5`. The model is split on the **first `@`** — it cannot be `:` or `/`
because OpenRouter slugs contain both (`tencent/hy3:free`). The model is stripped at
resolution time and stored in the **existing** `SlotState.model`, so `name`,
`storage_key()`, `adapter_named`, `is_provider_usable`, quota keys, and slot ids are all
unchanged and the model reaches the adapter over plumbing that already exists.

This also fixes the process-global-env limitation: with a per-slot model on the command
line, two slots can run different models in one run without fighting over environment.

Depends on Priority 4 (slot ids must survive `@`, `/`, and `.` in the ref). Priorities 6
and 8 depend on this.

## Approach

The critical line is `src/provider_ref.rs:~50`:

```rust
if name.contains(':') {
    bail!("invalid provider '{raw}' (use api:name)");
}
```

`cli:codex@x/y:free` must be split on `@` **before** this check runs, or the check accepts
it and silently yields `name == "codex@x/y:free"`, which then fails `adapter_named`
lookup with a confusing error. After the split, `name` must stay colon-free and `@`-free
while `model` may contain `:` and `/` freely.

The second decision that makes everything else fall out: **split `display()` from
`storage_key()`**. Today `storage_key()` (:79) just delegates to `display()` (:71).

| Method | Before | After |
|---|---|---|
| `display()` | `"cli:codex"` | `"cli:codex@openai/gpt-4o-mini"` (round-trips) |
| `storage_key()` | delegates to `display()` | `"cli:codex"` (model stripped) |

This keeps `cli:claude@opus` and `cli:claude@haiku` in **one shared quota bucket**, which
is correct: rate limits are enforced per account, not per model. Keying them separately
would let spar burn a limit while believing it had headroom in each bucket.

Then in `executor.rs`, `SlotState.provider` is stored **model-free** and the model goes
into `SlotState.model`, so the whole existing chain
`slot.model → slot_model_for() (executor.rs:1364-1374) → SpawnOpts.model → adapter`
carries it with **zero new plumbing**.

---

## Steps

### Step 1: Add the model field and split parsing
**File:** `/home/sholom/projects/spar/src/provider_ref.rs`

1a. Add the field to the struct (:28-33):

| Field | Type | Note |
|---|---|---|
| `backend` | `ExecBackend` | unchanged |
| `name` | `String` | unchanged semantics — **still the adapter id** |
| `model` | `Option<String>` | new |

1b. In `parse()` (:37-69): after stripping the `api:`/`cli:` prefix, `split_once('@')` on
the remainder into `(name, model)` — **before** the existing `name.contains(':')`
rejection. `split_once` takes the first occurrence, which is exactly the required
semantics: `cli:claude@a@b` yields model `"a@b"`.

1c. Reject an empty model after `@` with a clear message, e.g.
`invalid provider 'cli:codex@' (model after '@' is empty)`.

- [ ] Done

### Step 2: Split `display()` from `storage_key()`
**File:** `/home/sholom/projects/spar/src/provider_ref.rs:71-79`

| Method | Change |
|---|---|
| `display()` | append `@{model}` when `model.is_some()` |
| `storage_key()` | stop delegating — build `"{backend}:{name}"` directly, always model-free |

Then audit every caller to confirm each wants the right one:
```bash
grep -rn 'storage_key()\|\.display()' /home/sholom/projects/spar/src/
```
Rule of thumb: anything that is a **key** (quota buckets, map lookups, adapter lookup)
takes `storage_key()`; anything shown to a human or round-tripped takes `display()`.

Confirm `is_provider_usable` (`src/providers/mod.rs:203-228`) still works — its CLI branch
resolves via `adapter_named(&pref.storage_key())` falling back to `adapter_named(&pref.name)`,
and both are now model-free, so it needs **no change**. Verify this by reading, don't assume.

- [ ] Done

### Step 3: Store the split form on the slot
**File:** `/home/sholom/projects/spar/src/executor.rs:1380-1405` (`init_slot_model`)

| Field | Old | New |
|---|---|---|
| `SlotState.provider` | the raw ref string as passed | `pref.storage_key()` — **model-free** |
| `SlotState.model` | the explicit `model` argument | `pref.model.clone().or(model)` — ref model wins, explicit arg is the fallback |

This is the key architectural move. Because `SlotState.provider` never carries `@model`,
every downstream consumer — slot ids, worktree names, artifact names, quota lookups,
`state.json` — is automatically correct with no further edits.

Decide the precedence deliberately and note it in the commit: an explicit `@model` on the
ref is a direct user instruction and should beat a model chosen by `--select`'s
model-select artifact.

- [ ] Done

### Step 4: Replace the panic with upstream validation
**File:** `/home/sholom/projects/spar/src/executor.rs:1380-1405`

`init_slot_model` currently does:
```rust
let pref = ProviderRef::parse(&provider).expect("slot provider must be cli:… or api:…");
```

That `.expect()` becomes reachable from user input the moment Priority 8 lets a config
file supply refs. Validation must happen upstream, where an error can carry context:

4a. `src/model_select/mod.rs:37-39` already loops `ProviderRef::parse(p)?` over
    `--providers`. Confirm it still catches malformed `@model` refs after Step 1.

4b. Leave the `expect` as a genuine invariant assertion **only if** every path into
    `init_slot_model` is proven pre-validated. Otherwise convert it to return `Result` and
    fix call sites. Priority 8 adds config-load validation for the `[roles]` path.

- [ ] Done

### Step 5: Thread the model on the API path
**File:** `/home/sholom/projects/spar/src/executor.rs:292` (`ApiSlotRequest.model_override`)

Pick up `pref.model` so `api:openai@gpt-5` works symmetrically with the CLI path. The
architecture rule is that workflows do not branch on backend — an `@model` ref must mean
the same thing on both.

- [ ] Done

### Step 6: Add inline tests
**File:** `/home/sholom/projects/spar/src/provider_ref.rs` (inline `mod tests`, ~:96+)

| Test | Asserts |
|------|---------|
| `parses_openrouter_slug_with_slash_and_colon` | `cli:codex@tencent/hy3:free` → name `codex`, model `Some("tencent/hy3:free")` |
| `storage_key_drops_model` | that ref's `storage_key()` == `"cli:codex"` |
| `display_round_trips` | `parse(r.display()) == r` for a ref with a model |
| `splits_on_first_at` | `cli:claude@a@b` → name `claude`, model `Some("a@b")` |
| `bare_ref_has_no_model` | `cli:claude` → `model: None` |
| `empty_model_errors` | `cli:codex@` → `Err` |
| `colon_in_name_still_rejected` | `cli:foo:bar` → `Err` (the pre-existing guard survives the split) |
| `api_backend_carries_model` | `api:openai@gpt-5` → backend Api, name `openai`, model `Some("gpt-5")` |

**File:** `/home/sholom/projects/spar/src/providers/mod.rs` (inline tests, ~:301-316)

Extend the `pick_providers` / `cycle_take` tests with `@model` inputs, asserting the model
survives fleet selection.

- [ ] Done

### Step 7: Add a scenario test
**File:** `/home/sholom/projects/spar/tests/scenarios/plan_implement.rs`

Reuse the existing file — **no new `Cargo.toml` `[[test]]` block needed.**

`implement_dry_run_splits_provider_model`: run
`spar implement --dry-run --providers cli:claude@sonnet,cli:grok,cli:agy` and assert on
`state.json`:
- slot 0 has `provider == "cli:claude"` (model stripped)
- slot 0 has `model == "sonnet"`
- slot 0's `id` contains no `@`

- [ ] Done

### Step 8: Update the operator skill
**File:** `/home/sholom/projects/spar/skills/core.md`

8a. `## Dual backend` (~:22-36) — document the `@model` form next to the existing
    provider-ref examples at :27 and :30. State the first-`@` split rule explicitly and
    note that the model may contain `:` and `/` (OpenRouter slugs) while the adapter name
    may not.

8b. Note that `@model` variants share one quota bucket with their bare provider.

- [ ] Done

### Step 9: Verify
```bash
cd ../spar-feat-provider-model
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then:
```bash
cd /tmp && rm -rf spar-p5 && mkdir spar-p5 && cd spar-p5 && git init -q && \
  git commit -q --allow-empty -m init
spar implement --dry-run --providers 'cli:claude@sonnet,cli:grok' -t "hello"
jq '.slots[] | {id, provider, model}' .spar/runs/*/state.json
```
Expected: `provider` values carry no `@`, `model` is populated on slot 0, `id` values are
filesystem-safe.

- [ ] Done
