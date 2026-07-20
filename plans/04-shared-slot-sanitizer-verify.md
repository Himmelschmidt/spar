# Verify Priority 4: Shared slot sanitizer

> This file is for verifying the work done in [04-shared-slot-sanitizer.md](04-shared-slot-sanitizer.md).
> Load this file into a fresh chat to perform independent verification.

## What was done

`sanitize_slot` was promoted from a private helper in `src/workflow/implement.rs:381-383`
to one shared `pub fn` with a widened charset (lowercase, replace every char outside
`[a-z0-9_-]` with `-`, collapse runs, trim edges). Roughly twelve slot-id construction
sites across `implement.rs`, `plan.rs`, `peer.rs`, `review.rs`, `arena.rs`, and
`worktree.rs` were routed through it, replacing five inconsistent open-coded variants and
fixing a latent git-refname failure.

## Deliverables

### D1: One sanitizer, one definition
**Expected:** exactly one `fn sanitize_slot` in the tree.
- [ ] `grep -rn 'fn sanitize_slot' src/` returns exactly **one** hit
- [ ] The private copy at `src/workflow/implement.rs:381-383` is gone
- [ ] Correct behavior: it is `pub` and importable from `src/worktree.rs` (outside
      `src/workflow/`)

### D2: The widened charset
**Expected:** all of `:`, `/`, `@`, and `.` are neutralized.
- [ ] `cli:claude` → `cli-claude`
- [ ] `cli:codex@anthropic/claude-opus-4.5` → `cli-codex-anthropic-claude-opus-4-5`
- [ ] Correct behavior: runs collapse — `cli::claude` yields no `--`
- [ ] Correct behavior: edges trim — no leading or trailing `-`
- [ ] Correct behavior: idempotent — `sanitize_slot(sanitize_slot(x)) == sanitize_slot(x)`
- [ ] No regressions: an already-safe id like `review-0-a` passes through unchanged

### D3: Every call site is routed
**Expected:** no open-coded variants remain in slot-id construction.
- [ ] `grep -rn "replace(\[':'\|replace(':'\|replace(\['/'" src/` returns no hits that
      build a slot id, worktree path, or branch name
- [ ] `src/workflow/arena.rs:55, :66, :338, :375` — previously **fully unsanitized** —
      now sanitize the provider component
- [ ] `src/workflow/implement.rs:912` (`review-{prov}-wide`) — previously unsanitized —
      now sanitized
- [ ] `src/workflow/peer.rs:57-58` and `src/workflow/review.rs:62` — previously `:` only —
      now handle `/` and `@`
- [ ] `src/workflow/plan.rs:100-107, :257-258, :524-546` routed

### D4: Worktree paths and git branch names are safe
**Expected:** `src/worktree.rs:9-23` uses the shared sanitizer.
- [ ] `worktree_path()` and `branch_name()` both call it — previously only
      `replace('/', "-")`, leaving `:` in refnames, which **git rejects**
- [ ] Correct behavior: this fixes a live bug, not a hypothetical one. An arena run
      previously produced ids like `arena-0-cli:claude`. Confirm via the integration
      check below

### D5: No behavioral surprises in state
**Expected:** the slot-id shape change is understood and recorded.
- [ ] The commit message notes that slot ids change shape and that pre-existing
      `.spar/runs/*/state.json` will not match new ids — safe because runs are per-run-id
      with no cross-run lookup
- [ ] The implementer slot id `"impl"` (`implement.rs:174`) is **unchanged** — it has no
      punctuation and must not have been gratuitously altered

## Automated checks

```bash
cd ../spar-feat-provider-model
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
- [ ] All pass
- [ ] Any test that asserted an old unsanitized slot id was updated, not deleted —
      check `git diff main -- tests/` for removed assertions

## Integration checks

```bash
cd /tmp && rm -rf spar-v4 && mkdir spar-v4 && cd spar-v4 && git init -q && \
  git commit -q --allow-empty -m init
spar run --workflow arena --dry-run --providers cli:claude,cli:grok -t "add a hello function"
git branch --list
ls .spar/runs/*/logs/
ls .spar/runs/*/prompt-*.md
jq -r '.slots[].id' .spar/runs/*/state.json
```
- [ ] No branch name, log filename, prompt filename, or slot id contains `:`, `/`, `@`,
      or `.`
- [ ] No id contains a doubled `--`
- [ ] The arena run completes rather than failing on worktree/branch creation
- [ ] `spar plan --dry-run --providers cli:claude,cli:grok -t "x"` and
      `spar implement --dry-run --providers cli:claude,cli:grok,cli:agy -t "x"` both still
      work — the sweep did not break the normal flows
- [ ] This priority added **no** new behavior beyond sanitization: `git diff main --stat`
      shows no changes to `provider_ref.rs`, `config.rs`, or any template

## Notes

[Leave blank — the verifier fills this in with findings]
