# Verify Priority 5: ProviderRef @model

> This file is for verifying the work done in [05-provider-ref-model.md](05-provider-ref-model.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

`ProviderRef` gained `model: Option<String>`, split on the **first `@`** before the
existing colon rejection in `parse()`. `display()` and `storage_key()` were split so the
former round-trips the model and the latter drops it, keeping `@model` variants in one
shared quota bucket. `init_slot_model` (`src/executor.rs:1380-1405`) now stores the
model-free ref in `SlotState.provider` and the model in `SlotState.model`, so the model
reaches the adapter over the pre-existing `slot.model → SpawnOpts.model` chain with no
new plumbing. The API path picks up the model via `ApiSlotRequest.model_override`.

## Deliverables

### D1: Parsing splits on the first `@`, before the colon check
**Expected:** OpenRouter slugs containing both `/` and `:` survive.
- [ ] `cli:codex@tencent/hy3:free` → backend Cli, name `codex`, model `Some("tencent/hy3:free")`
- [ ] Correct behavior: the split happens **before** the `name.contains(':')` rejection at
      `src/provider_ref.rs:~50`. Read the code — if the split came after, this ref would
      be rejected or would silently yield `name == "codex@tencent/hy3:free"`
- [ ] `cli:claude@a@b` → model `"a@b"` (first `@` only)
- [ ] `cli:claude` → `model: None`
- [ ] `cli:codex@` → `Err` with a clear message
- [ ] No regressions: `cli:foo:bar` is **still** rejected — the pre-existing colon guard
      survived the change
- [ ] `api:openai@gpt-5` parses symmetrically

### D2: `storage_key()` drops the model; `display()` keeps it
**Expected:** quota buckets are shared across `@model` variants.
- [ ] `ProviderRef::parse("cli:claude@opus").storage_key() == "cli:claude"`
- [ ] `ProviderRef::parse("cli:claude@opus").display() == "cli:claude@opus"`
- [ ] `parse(r.display()) == r` round-trips
- [ ] Correct behavior: `storage_key()` no longer delegates to `display()` — read
      `src/provider_ref.rs:71-79` and confirm they are independent implementations
- [ ] Correct behavior: every caller takes the right one. Run
      `grep -rn 'storage_key()\|\.display()' src/` and confirm quota lookups
      (`src/quota.rs`) and adapter lookups use `storage_key()`, while human-facing output
      uses `display()`
- [ ] `cli:claude@opus` and `cli:claude@haiku` map to the **same** quota key. This is
      correct — rate limits are per-account, not per-model, and separate buckets would let
      spar burn a limit while believing it had headroom

### D3: The model lands on the slot, not in the provider string
**Expected:** `SlotState.provider` is always model-free.
- [ ] `init_slot_model` stores `pref.storage_key()` as the provider
- [ ] It stores `pref.model.or(explicit_model)` as the model — the ref's `@model` beats a
      model chosen by `--select`
- [ ] Correct behavior: because the provider string is model-free, slot ids, worktree
      names, artifact names, and quota keys needed **no** further edits. Confirm no such
      edits appear in `git diff main -- src/worktree.rs src/quota.rs`

### D4: Adapter lookup and usability are untouched
**Expected:** `adapter_named` and `is_provider_usable` required no change.
- [ ] `git diff main -- src/providers/mod.rs` shows no change to `adapter_named`
      (:192-200) or `is_provider_usable` (:203-228), **or** the change is a justified
      correction rather than special-casing `@`
- [ ] Correct behavior: `is_provider_usable("cli:claude@opus")` returns the same result as
      `is_provider_usable("cli:claude")` — verify by test or by reading the resolution path

### D5: The panic is not reachable from user input
**Expected:** a malformed ref produces an error, not a panic.
- [ ] Either the `.expect()` in `init_slot_model` was converted to a `Result`, or every
      path into it is provably pre-validated (`src/model_select/mod.rs:37-39` loops
      `ProviderRef::parse(p)?`)
- [ ] Correct behavior: `spar implement --dry-run --providers 'cli:codex@' -t x` exits
      non-zero with a message — **not** a panic backtrace

## Automated checks

```bash
cd ../spar-feat-provider-model
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test provider_ref
cargo test
```
- [ ] All pass
- [ ] The `pick_providers` / `cycle_take` tests in `src/providers/mod.rs` (~:301-316) were
      extended with `@model` cases and pass
- [ ] `implement_dry_run_splits_provider_model` exists in
      `tests/scenarios/plan_implement.rs` and passes

## Integration checks

```bash
cd /tmp && rm -rf spar-v5 && mkdir spar-v5 && cd spar-v5 && git init -q && \
  git commit -q --allow-empty -m init
spar implement --dry-run --providers 'cli:claude@sonnet,cli:grok' -t "hello"
jq '.slots[] | {id, provider, model}' .spar/runs/*/state.json
```
- [ ] Slot 0: `provider == "cli:claude"`, `model == "sonnet"`
- [ ] No slot `id` contains `@` (Priority 4's sanitizer plus the model-free provider string)
- [ ] `skills/core.md` documents the `@model` form and the first-`@` split rule in
      `## Dual backend` (~:22-36), and notes the shared quota bucket
- [ ] Priority 4's sanitizer is still the only slot-id mangling path — this priority did
      not add a second one
- [ ] Architecture check: `@model` means the same thing on `cli:` and `api:` refs. No
      workflow branches on backend to interpret it

## Notes

[Leave blank — the verifier fills this in with findings]
