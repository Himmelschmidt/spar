# Priority 7: OpenRouter model list

## Goal

Extend `spar model list` with `--provider openrouter`, showing context length and pricing
and — critically — **filtering to tool-capable models by default**. This is a guardrail,
not a convenience: of 339 OpenRouter models, 268 declare `tools` in
`supported_parameters` and 71 do not. A model without tool support silently fails as an
agent — it will happily generate text and never call a tool, producing a slot that exits
0 with no artifact. Hiding those by default is the correct bias.

`spar model list` **already exists**: `ModelAction::List` at `src/cli.rs:383-397`,
implemented at `src/model_select/mod.rs:474-546`. This priority extends it; it does not
create a new command.

Depends on nothing strictly, but belongs with Priorities 5-6 — the listing exists to feed
`cli:codex@<slug>`.

## Approach

Blocking HTTP with `ureq` — **there is no tokio, no async runtime, and no reqwest in this
repo** (`Cargo.toml:33`, `ureq` 3.3.0 with the `json` feature). Copy the shape at
`src/model_select/vals.rs:60-67`:

```rust
let resp = ureq::get(url).header("User-Agent", "...").call().with_context(...)?;
let body = resp.into_body().read_to_string().with_context(...)?;
```

`https://openrouter.ai/api/v1/models` is **unauthenticated**, so there is no key handling.
`OPENROUTER_API_KEY` appears nowhere in this repo and should stay that way — the key is
codex's concern.

Cache mirrors `src/model_select/cache.rs` (57 lines): `~/.spar/cache/openrouter/models.json`
via `registry::spar_home()`, with mtime-based staleness against the existing
`ModelSelectConfig.cache_ttl_secs` (`src/config.rs:63-64`).

---

## Steps

### Step 1: Add the fetch module
**File:** `/home/sholom/projects/spar/src/model_select/openrouter.rs` (new)

```rust
pub struct OrModel {
    pub id: String,
    pub name: String,
    pub context_length: Option<u64>,
    pub pricing: OrPricing,
    pub supported_parameters: Vec<String>,
}
pub struct OrPricing { pub prompt: String, pub completion: String }

pub fn fetch_models() -> Result<Vec<OrModel>>;
pub fn tool_capable(m: &OrModel) -> bool;  // supported_parameters iter().any(|p| p == "tools")
```

Notes on the real response shape — verify against a live fetch before finalizing the structs:
- `pricing.prompt` and `pricing.completion` are **strings**, not numbers, and free models
  return the literal `"0"`. Deserialize as `String` and format for display.
- `supported_parameters` may be absent on some entries — use `#[serde(default)]`.
- `context_length` may be null — `Option<u64>`.

Use `#[derive(Deserialize)]` with `#[serde(default)]` liberally. An unexpected field
should never fail the whole fetch.

```bash
curl -s https://openrouter.ai/api/v1/models | jq '.data[0]'
```

- [ ] Done

### Step 2: Add caching
**File:** `/home/sholom/projects/spar/src/model_select/openrouter.rs`

Mirror `src/model_select/cache.rs` — `cache_path()` → `~/.spar/cache/openrouter/models.json`
via `registry::spar_home()`, plus `load_cached` / `save_cached` / mtime-based
`cache_age_secs`. Reuse the staleness helper at `src/model_select/mod.rs:320`
(`cache_is_stale()`) against `ModelSelectConfig.cache_ttl_secs` rather than inventing a
second TTL knob.

Do not add a new config key for this.

- [ ] Done

### Step 3: Add the CLI flags
**File:** `/home/sholom/projects/spar/src/cli.rs:383-397` (`ModelAction::List`)

| Flag | Type | Behavior |
|---|---|---|
| `--provider` | `Option<String>` | `openrouter` selects the new path; absent keeps today's vals path |
| `--all` | `bool` | escape hatch: include models without `tools` support |

Keep the existing `--json` flag on the variant working for both paths.

- [ ] Done

### Step 4: Branch the List arm
**File:** `/home/sholom/projects/spar/src/model_select/mod.rs:474-546`

Branch at the **top** of the `List` arm: when `provider == Some("openrouter")`, run the
OpenRouter path and return; otherwise fall through to the existing vals path **completely
unchanged**. An unknown `--provider` value should `bail!` listing the valid values.

Text table columns:

| ID | CTX | $/M IN | $/M OUT | TOOLS |
|----|-----|--------|---------|-------|

Filter to tool-capable by default. Print a footer stating how many were hidden, e.g.
`71 models without tool support hidden (--all to show)`. **Do not silently hide** — a
guardrail the user cannot see is a trap.

`--json` output should include the full model list with a `tool_capable` boolean per
entry, matching the arm's existing `serde_json::json!` dual-output style (:487-546).

`spar model` deliberately falls back to `Config::default()` when outside a project
(`src/main.rs:187-193`) so it works anywhere — preserve that.

- [ ] Done

### Step 5: Add inline tests
**File:** `/home/sholom/projects/spar/src/model_select/openrouter.rs`

Use a `&str` const fixture of a trimmed real API response — **do not add a fixture file**
and do not make the tests hit the network.

| Test | Asserts |
|------|---------|
| `deserializes_real_response_shape` | a 3-model fixture parses, including one with a null `context_length` and one with no `supported_parameters` key |
| `tool_capable_true` | a model listing `"tools"` → true |
| `tool_capable_false` | a model without it → false |
| `free_tier_pricing_formats` | `"0"` prompt/completion renders as free, not as a parse error |
| `slug_with_colon_preserved` | `tencent/hy3:free` survives round-trip — this is the id users paste into `cli:codex@…` |

- [ ] Done

### Step 6: Update the operator skill
**File:** `/home/sholom/projects/spar/skills/core.md`

Document `spar model list --provider openrouter [--all] [--json]` wherever the `model`
command surface is described, stating the tool-capability filter and its rationale
(non-tool models silently fail as agents). This is a CLI-surface change, so it is a
required same-commit doc update.

- [ ] Done

### Step 7: Record the guardrail decision
**File:** `/home/sholom/projects/spar/DECISIONS.md`

Append to the `## Model select (vals-backed)` table:

```
| MS16 | **`spar model list --provider openrouter` filters to `supported_parameters` containing `tools` by default** (268/339 models); the remaining 71 cannot function as agents and are hidden unless `--all` is passed. Guardrail, not a preference | DECIDED |
```

- [ ] Done

### Step 8: Verify
```bash
cd ../spar-feat-provider-model
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then exercise it for real:
```bash
spar model list --provider openrouter | head -20
spar model list --provider openrouter --json | jq '.models | length'
spar model list --provider openrouter --all --json | jq '.models | length'
spar model list --provider bogus   # expect a clear bail listing valid values
spar model list                    # expect the vals path, unchanged
ls -la ~/.spar/cache/openrouter/models.json
```
Expected: the filtered count is meaningfully smaller than `--all`; the footer names the
hidden count; the second invocation is fast (cache hit); the plain `spar model list` is
byte-identical to its pre-change output.

- [ ] Done
