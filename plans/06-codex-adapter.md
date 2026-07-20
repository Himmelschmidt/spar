# Priority 6: Codex adapter — OpenRouter `@slug` routing

## Goal

Extend the **existing** `codex` adapter so `cli:codex@<openrouter-slug>` routes to
OpenRouter per slot.

> **Correction to an earlier draft of this plan.** A previous version claimed the codex
> adapter "does not exist in the repo today" and scoped this priority as a from-scratch
> write. That was explored against a stale checkout (local `main` was one commit behind
> `origin/main`). The adapter **was merged** as `f57d86e` and is recorded as `DECISIONS`
> **O18**. This priority is an **edit**, not a new file.

The point is breadth: one `@slug` per slot gives spar access to every tool-capable model
on OpenRouter through a real, battle-tested agent runtime. **`api:openrouter` is
explicitly not being built** — codex is the better runtime and spar's thin in-tree API
loop gets no further investment.

Depends on Priority 5 (`ProviderRef` must carry the model) and Priority 4 (slot ids must
survive slug punctuation).

## What already exists (do not rebuild)

From `f57d86e`, verified present in `src/providers/codex.rs` and `src/process.rs`:

| Already done | Where |
|---|---|
| `CodexAdapter` implementing `ProviderAdapter`, registered in `all_adapters()` (now **4** adapters) | `src/providers/codex.rs`, `src/providers/mod.rs:182-185` |
| `codex exec --json --skip-git-repo-check [--dangerously-bypass-approvals-and-sandbox] [-p <profile>] [-m <model>] -- <prompt>` | `codex.rs` `build_headless` |
| `capabilities()`: headless=true, interactive=false, resume=false, skip_permissions=true, native_sandbox=false | `codex.rs` |
| `DeliveryStrategy::None` / `PresenceSource::None` | `codex.rs` |
| Env selection helpers `codex_profile()` / `codex_model()` (`SPAR_CODEX_PROFILE`, `SPAR_CODEX_MODEL`) | `codex.rs` |
| `build_interactive` delegates to `build_headless` (tmux never drops `--json`/bypass) | `codex.rs` |
| **codex JSONL parsing + real token tracking** — `item.completed` (agent_message/reasoning/tool) and `turn.completed.usage` incl. `cached_input_tokens` → `cache_read` | `src/process.rs` `StreamCoalescer` |
| Inline adapter tests + `process.rs` codex parser tests | both files |
| `cli:codex` documented in the operator skill | `skills/core.md` |
| Adapter decision recorded | `DECISIONS.md` O18 |

**codex is the only adapter using `-m`.** `claude.rs`, `grok.rs`, and `agy.rs` all use
`--model <value>`.

## Approach

One piece of real logic, isolated in a free function so it is testable without spawning:

```rust
fn model_args(model: &str) -> Vec<String> {
    if model.contains('/') {
        vec!["-c".into(), "model_provider=openrouter".into(), "-m".into(), model.into()]
    } else {
        vec!["-m".into(), model.into()]
    }
}
```

A `/` in the model marks it an OpenRouter slug (`openai/gpt-4o-mini`,
`tencent/hy3:free`); a bare model (`gpt-5`) goes to codex's own default provider.

**Profile interaction:** when an explicit slug is supplied, emit
`-c model_provider=openrouter -m <slug>` and **omit `-p`** — the invocation becomes
self-describing and no longer depends on the operator's `muse.config.toml`. The profile
applies only when no model is given, so *the `muse` profile degrades to "the default when
no `@model` is given"*, exactly as intended.

Empirically verified (this session):
`codex exec --json -c model_provider=openrouter -m openai/gpt-4o-mini` ran with **no
profile**, performed 2 real `command_execution` tool calls, created the file, and returned
real `turn.completed` usage (`input_tokens: 29504, cached_input_tokens: 14720`).

---

## Steps

### Step 1: Confirm the codex CLI surface (already established — no discovery needed)

The surface was determined empirically against `codex-cli 0.144.4`. Recorded for reference:

| Question | Answer |
|---|---|
| Prompt form | Trailing **positional**; flags must precede it. `--` separator is accepted and already emitted (guards prompts starting with `-` or equal to a subcommand like `review`/`resume`). |
| Non-interactive permission flag | `--dangerously-bypass-approvals-and-sandbox` (FullAuto). `TrustPolicy::Prompt` emits none, falling back to codex config defaults. |
| Sandbox by default | Yes — `-s read-only` unless overridden; FullAuto bypasses it, so `native_sandbox=false`. |
| Resume | `codex exec resume` exists but is **not wired**; `capabilities().resume=false`. |
| Repeated `-c key=value` | Accepted and composes. |
| Machine output | `--json` → JSONL (`thread.started`, `turn.started`, `item.completed`, `turn.completed`, `turn.failed`). |

Re-run `codex exec --help` only if the installed version differs from 0.144.4.

- [ ] Done (confirm version, no code change)

### Step 2: Add OpenRouter slug routing
**File:** `/home/sholom/projects/spar/src/providers/codex.rs`

2a. Add the `model_args` free function above.

2b. In `build_headless`, replace the current unconditional `-m <model>` emission with
`model_args(...)`, and make the `-p <profile>` emission conditional: skip the profile when
an explicit model is present.

2c. Preserve the existing precedence, now feeding `model_args`:
`ProviderRef @model` (Priority 5, arrives via `slot.model`) → `SPAR_CODEX_MODEL` →
none (profile default).

**Exceptions — do NOT:**
- Re-implement codex JSONL parsing. It already exists in `process.rs` and is tested.
- Add `OPENROUTER_API_KEY` handling. The key is codex's concern (read via `env_key` from
  codex's own config); spar does not proxy credentials. spar only requires it be exported
  in the environment spar is launched from.
- Branch on backend anywhere outside this adapter file.

- [ ] Done

### Step 3: Extend the inline tests
**File:** `/home/sholom/projects/spar/src/providers/codex.rs`

Existing tests already cover argv shape, `-m` from `opts.model`, `--` placement, the
`TrustPolicy::Prompt` case, and env profile/model precedence (under `ENV_LOCK`). **Take
`ENV_LOCK` in any new test that calls `build_headless`** — the helpers read process env.

| New test | Asserts |
|------|---------|
| `slug_model_routes_to_openrouter` | model `tencent/hy3:free` → argv contains `-c`, `model_provider=openrouter`, `-m`, `tencent/hy3:free`, in that order |
| `slug_model_omits_profile` | with a slug, argv contains **no** `-p` |
| `bare_model_omits_provider_override` | model `gpt-5` → `-m gpt-5`, **no** `model_provider=openrouter` |
| `no_model_uses_profile` | `opts.model == None` and no env → argv has `-p muse` and no `-m` |

- [ ] Done

### Step 4: Update the operator skill
**File:** `/home/sholom/projects/spar/skills/core.md`

`cli:codex` is **already documented** there. Add only: the `@<openrouter-slug>` form, the
profile-is-the-default-when-no-model rule, and a pointer to
`spar model list --provider openrouter` (Priority 7) for discovering tool-capable slugs.

- [ ] Done

### Step 5: Record the cost decision
**File:** `/home/sholom/projects/spar/DECISIONS.md`

MS7 currently reads: "**CLI economics**: treat `cli:*` cost as **0** for scoring (flat
sub); do not use vals $ against subscription CLIs | DECIDED". That breaks here:
`cli:codex@<openrouter-slug>` is a `cli:` ref that bills **per token**.

> **Correction:** an earlier draft said to append this as `MS12`. **MS12 is taken** (role
> defaults in config). The MS series currently ends at **MS14**, so use **MS15**.

Append to the `## Model select (vals-backed)` table:

```
| MS15 | **Revises MS7.** `cli:*` is zero-cost only when the adapter bills against its own subscription. `cli:codex@<slug>` routes to OpenRouter and bills per token (29.5k input on a trivial task; 489k on a real review run), so cost scoring keys on the **resolved model**, not the backend prefix. A ref carrying an `@model` that maps to a metered provider is costed per token from the OpenRouter price table | DECIDED |
```

Edit MS7's Decision cell in place to append `— superseded by MS15 for @model refs`.

**Scope note:** MS15 records the decision; *implementing* revised cost scoring is separate
follow-up work. It is **not** blocked on usage parsing — `turn.completed.usage` is already
parsed and persisted to `stats.json`, so real per-slot token counts are available today.
The remaining gap is joining those counts to the OpenRouter price table. Say so in the
commit message so the gap is not silently forgotten.

- [ ] Done

### Step 6: Verify
```bash
cd ../spar-feat-provider-model
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then confirm detection and slug routing:
```bash
spar doctor --json | jq '.providers'
```
Expected: `codex` appears, available `true` if the binary is on `PATH`.

```bash
cd /tmp && rm -rf spar-p6 && mkdir spar-p6 && cd spar-p6 && git init -q && \
  git commit -q --allow-empty -m init
spar implement --dry-run --providers 'cli:codex@openai/gpt-4o-mini' -t "hello"
jq '.slots[] | {id, provider, model}' .spar/runs/*/state.json
```
Expected: `provider == "cli:codex"`, `model == "openai/gpt-4o-mini"`, slot id free of
`@` and `/`.

**Live run (burns real OpenRouter tokens — only if the user approves):** one small task
with `--providers cli:codex@openai/gpt-4o-mini`, no `--dry-run`, confirming the agent
performs tool calls, writes its expected artifact, and lands non-zero
`input_tokens`/`output_tokens` in `logs/*.stats.json`.

- [ ] Done
