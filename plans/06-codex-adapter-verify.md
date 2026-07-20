# Verify Priority 6: Codex adapter

> This file is for verifying the work done in [06-codex-adapter.md](06-codex-adapter.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

The **existing** `codex` adapter (`src/providers/codex.rs`, merged as `f57d86e`, recorded
as DECISIONS O18) was **extended** — this was an edit, not a from-scratch write. An
earlier draft of the plan claimed the adapter did not exist; that draft was explored
against a stale checkout. The change adds a `model_args` free function mapping a model
containing `/` to `-c model_provider=openrouter -m <slug>` and a bare model to
`-m <model>`, and makes `-p <profile>` conditional (omitted when an explicit model is
given). DECISIONS gained **MS15** (not MS12 — that id is taken by "role defaults in
config"), revising MS7's flat-rate `cli:*` costing assumption.

## Deliverables

### D1: The pre-existing adapter is still intact (regression check)
**Expected:** the adapter merged in `f57d86e` was edited, not replaced or broken.
> This priority is an **edit**. `src/providers/codex.rs` already existed before it.
- [ ] `src/providers/codex.rs` exists and implements `ProviderAdapter`
      (`src/providers/mod.rs:111-157`)
- [ ] `grep -n 'codex' src/providers/mod.rs` shows both `mod codex;` and an entry in
      `all_adapters()` — which now holds **four** adapters (claude, grok, agy, codex)
- [ ] `name()` returns `"codex"`, `binary_names()` returns `&["codex"]`
- [ ] Capabilities unchanged: headless=true, interactive=false, resume=false,
      skip_permissions=true, native_sandbox=false
- [ ] `build_interactive` still delegates to `build_headless` (so `--backend tmux` cannot
      drop `--json`/bypass — this was a review finding, do not regress it)
- [ ] The pre-existing `process.rs` codex parser tests still pass

### D2: The CLI surface matches the installed binary
**Expected:** flags match `codex-cli 0.144.4` (already established empirically).
> No discovery work was required here; the surface was determined in a prior session.
- [ ] `codex --version` is 0.144.x — if it differs, re-check `codex exec --help`
- [ ] `permission_args(TrustPolicy::FullAuto)` is
      `--dangerously-bypass-approvals-and-sandbox`; `Prompt` emits none
- [ ] The prompt is the trailing **positional**, with flags before it and a `--` separator
      preceding it (guards prompts starting with `-` or equal to `review`/`resume`)

### D3: Model mapping
**Expected:** a slug routes to OpenRouter; a bare model does not; no model uses the profile.
- [ ] Model `tencent/hy3:free` → argv contains `-c`, `model_provider=openrouter`, `-m`,
      `tencent/hy3:free`
- [ ] With a slug, argv contains **no** `-p` (the invocation is self-describing and does
      not depend on the operator's `muse.config.toml`)
- [ ] Model `gpt-5` → argv contains `-m gpt-5` and **no** `model_provider=openrouter`
- [ ] No model and no `SPAR_CODEX_*` env → argv contains `-p muse` and **no** `-m`.
      (Note: *not* "no flags at all" — the profile is the default when no model is given)
- [ ] Correct behavior: the mapping lives in a **free function** testable without
      spawning, not inline in `build_headless`
- [ ] Any new test that calls `build_headless` takes `ENV_LOCK` — the profile/model
      helpers read process env and other tests mutate it
- [ ] codex remains the only adapter using `-m`. Confirm `claude.rs`, `grok.rs`, and
      `agy.rs` still use `--model` and were not touched

### D4: Scope discipline
**Expected:** deliberately deferred work stayed deferred; existing work was not duplicated.
- [ ] No **new** event-stream parsing was added inside the adapter. codex JSONL parsing
      already exists in `src/process.rs` (`StreamCoalescer`) and is where it belongs;
      `src/providers/codex.rs` must not gain a second implementation. A `turn.completed`
      mention in a *comment* in `codex.rs` is fine — check for actual parsing logic
- [ ] spar does not read the OpenRouter key:
      `grep -rn 'env::var("OPENROUTER_API_KEY")' src/` returns nothing. (A bare
      `grep OPENROUTER_API_KEY src/` **will** match a test-fixture string in
      `process.rs` — that is expected, not a violation)
- [ ] No `api:openrouter` provider was added — `grep -rn 'openrouter' src/api/` is empty
- [ ] `src/model_select/map.rs:58-60` was **not** extended to route unmapped labs through
      `cli:codex` — that was explicitly deferred as it silently changes `--select` behavior

### D5: The cost decision is recorded
**Expected:** MS7's flat-rate assumption is revised.
- [ ] `DECISIONS.md` `## Model select` table has **MS15** with status `DECIDED`, stating
      that `cli:*` is zero-cost only when the adapter bills against its own subscription,
      and that `cli:codex@<slug>` bills per token
- [ ] The new row is **MS15**, not MS12 — MS12 is already taken ("role defaults in
      config"); the MS series ended at MS14 before this change
- [ ] MS7's cell was edited in place to note it is superseded for `@model` refs
- [ ] Correct behavior: the plan or commit message notes that MS15 records the decision
      only — **implementing** revised cost scoring is follow-up work. It is **not** blocked
      on usage parsing: `turn.completed.usage` is already parsed into `stats.json` by the
      merged adapter work, so real per-slot token counts exist today. The remaining gap is
      joining them to the OpenRouter price table. Confirm this is written down, not assumed

## Automated checks

```bash
cd ../spar-feat-provider-model
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test codex
cargo test
```
- [ ] All pass
- [ ] The `command_to_parts`-style tests (mirroring `grok.rs:88-100`) assert on argv
      without spawning a process

## Integration checks

```bash
which codex && codex --version
spar doctor --json | jq '.providers'
```
- [ ] `codex` appears in the provider list, `usable: true` when the binary is on `PATH`
- [ ] Correct behavior: with the binary **removed from `PATH`**, `is_provider_usable`
      reports it unusable rather than erroring — the CLI branch checks
      `resolve_binary().is_some()`

```bash
cd /tmp && rm -rf spar-v6 && mkdir spar-v6 && cd spar-v6 && git init -q && \
  git commit -q --allow-empty -m init
spar implement --dry-run --providers 'cli:codex@openai/gpt-4o-mini' -t "hello"
jq '.slots[] | {id, provider, model}' .spar/runs/*/state.json
```
- [ ] `provider == "cli:codex"`, `model == "openai/gpt-4o-mini"`
- [ ] The slot `id` contains no `@` or `/`
- [ ] **Live run (burns real tokens — only with the user's explicit approval):** a single
      small task with `--providers cli:codex@openai/gpt-4o-mini` and no `--dry-run`
      completes, the agent performs real tool calls, and the expected artifact exists.
      Note the token count — it is the evidence behind MS15

## Notes

[Leave blank — the verifier fills this in with findings]
