# spar: Acceptance Gate + Per-Slot Models + Role-Keyed Fleet — Progress Tracker

> Load this file into context to see current status and pick up where we left off.

## Instructions for Claude

1. Read this file first to understand where we are.
2. Look at the **Current Focus** section to see which priority we're working on.
3. Open the linked plan file for that priority — it has step-by-step instructions
   with checkboxes. Find the first unchecked `[ ]` step and start there.
4. After completing a step, update the checkbox to `[x]` in the plan file and append
   `— [what actually happened]` to create an audit trail.
5. After completing ALL steps in a priority, update the **Status** table below
   (`IN PROGRESS` → `VERIFY`), set **Current Focus** to verification, and open the
   corresponding verify file (e.g., `01-review-result-parser-verify.md`).
6. Complete every checkbox in the verify file. Only after all pass, set status to
   `DONE` and advance **Current Focus** to the next priority.
7. If a step is partially done or blocked, add a note next to the checkbox.
8. **Gates after every priority — there is no CI, you must run these yourself:**
   ```bash
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
9. **Worktree rule:** never work in the primary checkout `/home/sholom/projects/spar`
   and never switch its branch. Each workstream gets its own sibling worktree:
   - Priorities 1-3 → `../spar-feat-acceptance`
   - Priorities 4-7 → `../spar-feat-provider-model`
   - Priorities 8-9 → `../spar-feat-roles`
10. **Docs move in the same commit as the code.** `skills/core.md` is the canonical
    outer-agent skill embedded in the binary; any change to CLI surface, flags,
    config keys, artifact schemas, or exit codes must update it in the same change.

## What this task is

Three staged workstreams, in dependency order:

**A (Priorities 1-3) — Machine-checkable acceptance.** Today a reviewer's verdict is
decided by `text.to_ascii_lowercase().contains("request_changes")` — an unanchored
whole-document substring scan. Because `templates/reviewer_adversarial.md:20` literally
contains the line `approve | request_changes`, any reviewer that echoes the format block
is scored `request_changes`; approval is effectively unreachable. Meanwhile reviewers
never see `plan.md` or `test-contract.md` at all. This workstream gives the contract
stable `AC-n` criterion ids, hands the plan and contract to the reviewer, and replaces
the substring scan with a real parser plus an acceptance gate. Extends DECISIONS O16
("clean exit != success") into "unmet acceptance criterion != success".

**B (Priorities 4-7) — Per-slot models + OpenRouter breadth.** `ProviderRef` gains an
optional model split on the first `@` (not `:` or `/` — OpenRouter slugs contain both,
e.g. `tencent/hy3:free`). The model is stripped at resolution time into the existing
`SlotState.model`, so `name`, `storage_key()`, quota keys, and slot ids are unchanged.
Extends the **existing** `codex` adapter (merged as `f57d86e`, DECISIONS O18 — an earlier
draft wrongly called this a from-scratch write, having explored a stale checkout) so
`@slug` maps to `-c model_provider=openrouter -m <slug>`, and extends `spar model list`
with an OpenRouter listing filtered to tool-capable models.

**C (Priorities 8-9) — Role-keyed fleet config.** Replaces positional index assignment
(`providers[0]` = Implementer, `[1]`/`[2]` = Reviewers) with a `[roles]` config block.
Folds `[suite].provider` and `[spec].provider` into `[roles].tester` and
`[roles].test_author` — clean break, no compat shim.

## Status

| # | Priority | Plan File | Verify File | Status | Notes |
|---|----------|-----------|-------------|--------|-------|
| 1 | Review result parser | [01-review-result-parser.md](01-review-result-parser.md) | [01-review-result-parser-verify.md](01-review-result-parser-verify.md) | NOT STARTED | Pure new module: parse `## Verdict` + `## Acceptance` + contract `AC-n` ids |
| 2 | Criterion ids + reviewer context | [02-criterion-ids-and-reviewer-context.md](02-criterion-ids-and-reviewer-context.md) | [02-criterion-ids-and-reviewer-context-verify.md](02-criterion-ids-and-reviewer-context-verify.md) | NOT STARTED | `AC-n` in test_author template; plan+contract into reviewer vars; kill dead `suite_body` |
| 3 | Acceptance gate | [03-acceptance-gate.md](03-acceptance-gate.md) | [03-acceptance-gate-verify.md](03-acceptance-gate-verify.md) | NOT STARTED | New `[review]` config, replace substring scan, fix dead arena branch, update dry-run synth |
| 4 | Shared slot sanitizer | [04-shared-slot-sanitizer.md](04-shared-slot-sanitizer.md) | [04-shared-slot-sanitizer-verify.md](04-shared-slot-sanitizer-verify.md) | NOT STARTED | Promote `sanitize_slot`, widen charset, route ~12 sites; fixes live git-refname bug |
| 5 | ProviderRef @model | [05-provider-ref-model.md](05-provider-ref-model.md) | [05-provider-ref-model-verify.md](05-provider-ref-model-verify.md) | NOT STARTED | Split on first `@`; `storage_key()` drops model so quota buckets stay shared |
| 6 | Codex adapter | [06-codex-adapter.md](06-codex-adapter.md) | [06-codex-adapter-verify.md](06-codex-adapter-verify.md) | NOT STARTED | From scratch — no codex adapter exists today |
| 7 | OpenRouter model list | [07-openrouter-model-list.md](07-openrouter-model-list.md) | [07-openrouter-model-list-verify.md](07-openrouter-model-list-verify.md) | NOT STARTED | Extend existing `spar model list` with `--provider openrouter`, tool-capable filter |
| 8 | Roles config block | [08-roles-config.md](08-roles-config.md) | [08-roles-config-verify.md](08-roles-config-verify.md) | NOT STARTED | New `[roles]`; drop `[suite].provider`/`[spec].provider`; unify role vocabulary |
| 9 | Role resolution rewire | [09-role-resolution-rewire.md](09-role-resolution-rewire.md) | [09-role-resolution-rewire-verify.md](09-role-resolution-rewire-verify.md) | NOT STARTED | One `provider_for`; rewire plan/implement/widen/rotate; `[roles]` satisfies the invariant |

## Current Focus

**Priority 1 — Review result parser** → [01-review-result-parser.md](01-review-result-parser.md)

## Quick Context

**Project:** `/home/sholom/projects/spar` — multi-agent coding orchestrator. Rust,
single binary, TUI-first. Runs fleets of coding agents across providers, isolating each
coding slot in its own git worktree.

**Tech stack:** Rust 2021. `clap` for CLI, `ratatui` TUI, `serde`/`serde_json`/`toml`,
`ureq` 3.3.0 (blocking HTTP — **there is no tokio, no async runtime, no reqwest**),
`assert_cmd` for scenario tests.

**Build / check:**
```bash
cargo build
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

**Test backend:** `--dry-run` / `SPAR_DRY_RUN=1` gives a real `.spar/` layout with no
provider spawn and no tokens burned. Use it for anything touching run lifecycle.

**Scenario test trap:** scenario tests live at `tests/scenarios/*.rs`, NOT `tests/*.rs`.
Cargo only sees them because `Cargo.toml` declares a `[[test]]` block per file with an
explicit `path` (Cargo.toml:47-72). **Adding a new scenario file does nothing until you
add its `[[test]]` block** — it will silently never run.

**Key file paths:**

| Path | Role |
|---|---|
| `src/workflow/implement.rs` | 1226 lines. The heart of A and C. Suite parsing, reviewer dispatch, the verdict gate, escalation ladder |
| `src/workflow/plan.rs` | Planner/critic/test-author slots. Has a **duplicated** positional block at :91-121 and :524-546 |
| `src/workflow/arena.rs` | Has a dead anchored-verdict branch at :400-417 |
| `src/provider_ref.rs` | 111 lines. `ProviderRef { backend, name }`. Target of B1 |
| `src/providers/mod.rs` | `ProviderAdapter` trait (:111-157), `all_adapters()` (:178), `adapter_named` (:192), `is_provider_usable` (:203). **This is the adapter registry — `src/registry.rs` is NOT** |
| `src/config.rs` | 683 lines. Shadow-struct overlay pattern: every section needs a `…ConfigFile` twin + a merge arm in `apply_file` |
| `src/templates.rs` | `render_str` (:37-43) — silent on missing AND unused vars. `base_vars` (:58-90) pre-seeds defaults |
| `src/executor.rs` | Slot dispatch. `init_slot_model` (:1380), dry-run artifact synthesis (~:945-978) |
| `src/quota.rs` | `HashMap<String, ProviderQuota>` keyed by `storage_key()` |
| `templates/` | 11 prompt templates. `reviewer_adversarial.md`, `test_author.md`, `implementer.md`, `tester.md` |
| `skills/core.md` | 245 lines. Canonical outer-agent skill, `include_str!`'d into the binary |
| `DECISIONS.md` | GFM tables `\| ID \| Decision \| Status \|`, status `OPEN`/`LEANING`/`DECIDED` |

**Conventions:**
- No comments except where the *why* is non-obvious. No emojis anywhere.
- Early returns and guard clauses over nested if/else.
- No premature abstraction; three similar lines beat an abstraction.
- No backwards-compat shims, no deprecated wrappers, no `_v2`/`_new` suffixes — change
  functions in place and fix all call sites.
- Conventional commits (`feat:`, `fix:`, `refactor:`).
- Architecture split is non-negotiable: **workflows must not branch on backend.**
  Orchestrator owns lifecycle/phases/gates; Backend owns how a slot thinks; Adapter owns
  one provider on one backend.
- Don't extend `src/mailbox.rs` — legacy, superseded by `src/bus.rs`.

**Corrections to premises — confirmed by exploration, do not re-litigate:**
1. There is no `src/implement.rs`. It is `src/workflow/implement.rs` (line numbers match).
2. There is **no codex adapter**, no `muse` profile, no `model_provider` handling
   anywhere in the repo. Priority 6 writes one from scratch.
3. `spar model list` **already exists** (`src/cli.rs:383-397`,
   `src/model_select/mod.rs:474-546`). Priority 7 extends it.
4. There is **no `[review]` config section**. Priority 3 creates it.
5. All three existing adapters use `--model <value>`, **not `-m`**. Only codex will
   use `-m`.
