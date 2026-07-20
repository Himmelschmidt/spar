# Verify Priority 7: OpenRouter model list

> This file is for verifying the work done in [07-openrouter-model-list.md](07-openrouter-model-list.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

`spar model list` gained `--provider openrouter` and `--all`. A new
`src/model_select/openrouter.rs` fetches `https://openrouter.ai/api/v1/models` over
blocking `ureq`, caches to `~/.spar/cache/openrouter/models.json`, and filters to models
declaring `tools` in `supported_parameters` by default. DECISIONS gained MS16.

## Deliverables

### D1: The command extends, not replaces
**Expected:** the existing vals path is untouched.
- [ ] `spar model list` with no `--provider` produces output identical to `main`'s —
      capture both and diff
- [ ] `src/model_select/mod.rs:474-546` branches at the **top** of the `List` arm and
      falls through to the existing code unchanged
- [ ] `spar model list --provider bogus` exits non-zero with a message listing valid values
- [ ] Correct behavior: `spar model` still works outside a git project — the deliberate
      `Config::default()` fallback at `src/main.rs:187-193` was preserved. Test from `/tmp`

### D2: The tool-capability guardrail
**Expected:** non-tool models are hidden by default and the hiding is visible.
- [ ] `spar model list --provider openrouter --json | jq '.models | length'` is
      meaningfully **smaller** than the `--all` count (roughly 268 vs 339)
- [ ] The text output prints a footer naming how many were hidden. **A guardrail the user
      cannot see is a trap** — confirm the footer exists, not just the filter
- [ ] `--all` includes them
- [ ] `--json` entries carry a `tool_capable` boolean
- [ ] Correct behavior: the rationale is documented — a model without tool support
      silently fails as an agent, exiting 0 with no artifact

### D3: Response handling is tolerant
**Expected:** the real API shape parses without surprises.
- [ ] `pricing.prompt` / `pricing.completion` are handled as **strings** (free models
      return the literal `"0"`), not deserialized as numbers
- [ ] `context_length` is `Option` — a null does not fail the fetch
- [ ] `supported_parameters` uses `#[serde(default)]` — an absent key does not fail
- [ ] Correct behavior: a slug with a colon (`tencent/hy3:free`) survives round-trip
      intact. This is the exact string users paste into `cli:codex@…`
- [ ] Correct behavior: an unexpected new field does not fail the whole fetch

### D4: No async, no new HTTP stack
**Expected:** blocking `ureq`, consistent with the repo.
- [ ] `git diff main -- Cargo.toml` adds **no** `reqwest`, `tokio`, or async runtime
- [ ] `src/model_select/openrouter.rs` uses the `ureq` pattern from
      `src/model_select/vals.rs:60-67`
- [ ] `grep -n 'api_key\|Authorization\|Bearer' src/model_select/openrouter.rs` returns
      nothing — the endpoint is unauthenticated
- [ ] `grep -rn 'OPENROUTER_API_KEY' src/` still returns nothing

### D5: Caching reuses existing machinery
**Expected:** no second TTL knob.
- [ ] `~/.spar/cache/openrouter/models.json` is written on first fetch
- [ ] Staleness uses the existing `ModelSelectConfig.cache_ttl_secs` (`src/config.rs:63-64`)
      — `git diff main -- src/config.rs` adds **no** new cache config key
- [ ] Correct behavior: a second invocation is visibly faster (cache hit)

### D6: Docs and decision
- [ ] `skills/core.md` documents `spar model list --provider openrouter [--all] [--json]`
      and states the filter rationale. This is a CLI-surface change, so it is a required
      same-commit update
- [ ] `DECISIONS.md` `## Model select` table has MS16 with status `DECIDED`

## Automated checks

```bash
cd ../spar-feat-provider-model
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test openrouter
cargo test
```
- [ ] All pass
- [ ] Correct behavior: the tests use an inline `&str` fixture and do **not** hit the
      network. Confirm by running with networking disabled, or read the tests

## Integration checks

```bash
spar model list --provider openrouter | head -20
spar model list --provider openrouter --json | jq '.models | length'
spar model list --provider openrouter --all --json | jq '.models | length'
spar model list --provider openrouter --json | jq -r '.models[0].id'
```
- [ ] The table shows ID, context length, and prompt/completion pricing columns
- [ ] Taking an id from this listing and using it works end to end:
      `spar implement --dry-run --providers "cli:codex@$(spar model list --provider openrouter --json | jq -r '.models[0].id')" -t hello`
      produces a slot with that model and a clean slot id
- [ ] This closes the loop the workstream exists for: the listing feeds Priority 6's
      adapter, which consumes Priority 5's `@model` refs

## Notes

[Leave blank — the verifier fills this in with findings]
