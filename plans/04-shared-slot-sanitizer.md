# Priority 4: Shared slot sanitizer

## Goal

Promote `sanitize_slot` from a private helper in `implement.rs` to one shared function
with a widened charset, and route every slot-id construction site through it. This is a
**hard prerequisite** for Priority 5: once provider refs carry `@model`, slot ids would
otherwise absorb `@`, `/`, and `.` (from slugs like `anthropic/claude-opus-4.5`) into
strings that become log filenames, prompt filenames, artifact names, and **git branch
names**.

This is a bug fix, not hygiene. `src/worktree.rs:9-23` applies only
`replace('/', "-")`, leaving `:` in git refnames — and git **rejects** `:` in refnames.
Arena's slot ids at `src/workflow/arena.rs:55` are built with `format!("arena-{i}-{prov}")`
with **no sanitization at all**, producing ids like `arena-0-cli:claude`. That is a latent
worktree-creation failure in the repo today.

Depends on nothing. Priority 5 depends on this.

## Approach

One function, ~12 call sites. Today there are **five different inconsistent variants**:

| Location | Current expression | Sanitizes |
|---|---|---|
| `src/workflow/implement.rs:381-383` (the private original) | `s.replace([':', '/'], "-")` | `:` `/` |
| `src/workflow/plan.rs:257-258` | `provider.replace(['/', ':'], "-")` | `:` `/` |
| `src/workflow/peer.rs:57-58` | `a.replace(':', "-")` | `:` only |
| `src/workflow/review.rs:62` | `prov.replace(':', "-")` | `:` only |
| `src/workflow/arena.rs:55, 66, 338, 375` | raw `format!` | **nothing** |
| `src/workflow/implement.rs:912` | `format!("review-{prov}-wide")` | **nothing** |
| `src/worktree.rs:9-23` | `slot_id.replace('/', "-")` | `/` only |

Target:

```rust
pub fn sanitize_slot(s: &str) -> String
```
Lowercase; replace every char not in `[a-z0-9_-]` with `-`; collapse runs of `-`; trim
leading and trailing `-`.

---

## Steps

### Step 1: Create the worktree

```bash
cd /home/sholom/projects/spar
git worktree add ../spar-feat-provider-model -b feat/provider-model
```

Priorities 4-7 happen in `../spar-feat-provider-model`. If Priorities 1-3 are merged,
branch from the updated `main`; otherwise this branch is independent of them (no file
overlap except `skills/core.md`).

- [ ] Done

### Step 2: Add the shared sanitizer
**File:** `/home/sholom/projects/spar/src/util.rs`

If `src/util.rs` does not exist, check first for an existing shared-helpers module:
```bash
ls /home/sholom/projects/spar/src/util.rs /home/sholom/projects/spar/src/paths.rs
```
If there is no `util.rs`, put `sanitize_slot` in `src/workflow/mod.rs` as a `pub fn` —
that module is already the shared workflow surface (`CommonOpts`, `resolve_fleet`) and
every call site is either in `src/workflow/` or in `src/worktree.rs`. Do not create a new
module for one function.

Add inline `#[cfg(test)] mod tests`:

| Test | Asserts |
|------|---------|
| `strips_provider_ref_punctuation` | `cli:claude` → `cli-claude` |
| `strips_model_slug_punctuation` | `cli:codex@anthropic/claude-opus-4.5` → `cli-codex-anthropic-claude-opus-4-5` |
| `collapses_runs` | `cli::claude` → `cli-claude` (no double dash) |
| `trims_edges` | `@claude@` → `claude` |
| `idempotent` | `sanitize_slot(sanitize_slot(x)) == sanitize_slot(x)` for the cases above |
| `preserves_safe_chars` | `review-0-a` unchanged |

- [ ] Done

### Step 3: Delete the private copy and update its call sites
**File:** `/home/sholom/projects/spar/src/workflow/implement.rs`

3a. Delete the private `fn sanitize_slot` at :381-383.

3b. Import the shared one and update the three existing call sites at :182, :188, :212.

3c. Add sanitization at :912 (`format!("review-{prov}-wide")`) — currently unsanitized.

```bash
grep -n 'sanitize_slot' /home/sholom/projects/spar/src/workflow/implement.rs
```
Expected after this step: 4 call sites, 0 definitions.

- [ ] Done

### Step 4: Route the remaining workflow sites
**Files:**

| File | Lines | Change |
|---|---|---|
| `/home/sholom/projects/spar/src/workflow/plan.rs` | :100-107, :257-258, :524-546 | replace the open-coded `replace(['/', ':'], "-")` with `sanitize_slot` |
| `/home/sholom/projects/spar/src/workflow/peer.rs` | :57-58 | replace `replace(':', "-")` (currently misses `/`) |
| `/home/sholom/projects/spar/src/workflow/review.rs` | :62 | replace `replace(':', "-")` |
| `/home/sholom/projects/spar/src/workflow/arena.rs` | :55, :66, :338, :375 | wrap the provider component — currently raw |

Sweep check:
```bash
grep -rn "replace(\[':'\|replace(':'\|replace(\['/'" /home/sholom/projects/spar/src/
```
Expected hits after the sweep: ~0 in `src/workflow/`, plus whatever `src/worktree.rs`
still has pending Step 5.

**Exceptions — do NOT change:**
- `ProviderRef::display()` / `storage_key()` — those must round-trip the real ref, not a
  filesystem-safe mangling.
- Any `replace` that is not building a slot id (grep hits in unrelated string handling).

- [ ] Done

### Step 5: Fix worktree path and branch naming
**File:** `/home/sholom/projects/spar/src/worktree.rs:9-23`

`worktree_path()` and `branch_name()` apply only `slot_id.replace('/', "-")` (lines 17
and 22), leaving `:` in git refnames. Route both through `sanitize_slot`.

This fixes the latent failure where an arena run with unsanitized ids
(`arena-0-cli:claude`) cannot create its branch.

- [ ] Done

### Step 6: Find tests asserting literal slot ids
Slot ids change shape for arena/peer/review. Before running the suite, find what will break:

```bash
grep -rn 'arena-\|peer-\|review-\|"impl"' /home/sholom/projects/spar/tests/ /home/sholom/projects/spar/src/ | grep -i test
```

Update any test asserting an old unsanitized id. Note `"impl"` (the implementer slot id at
`implement.rs:174`) is a bare literal with no punctuation and is **unaffected**.

**State compatibility note for the commit message:** slot ids change shape, so
`.spar/runs/*/state.json` from older runs will not match new ids. Runs are per-run-id with
no cross-run id lookup, so this is safe — but say so in the commit.

- [ ] Done

### Step 7: Verify
```bash
cd ../spar-feat-provider-model
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Then prove the arena refname bug is actually fixed:
```bash
cd /tmp && rm -rf spar-p4 && mkdir spar-p4 && cd spar-p4 && git init -q && \
  git commit -q --allow-empty -m init
spar run --workflow arena --dry-run --providers cli:claude,cli:grok -t "add a hello function"
git branch --list
ls .spar/runs/*/logs/
```
Expected: branch names and log filenames contain no `:`, `/`, `@`, or `.`; no duplicate
dashes.

- [ ] Done
