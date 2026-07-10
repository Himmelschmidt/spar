---
id: 1
title: Dynamic model select (vals benchmarks)
status: done
milestone: later
effort: L
priority: medium
dependencies: []
---

# 001 — Dynamic model select

## Summary

Instead of only manual `--providers`, resolve fleet slots from [vals.ai](https://www.vals.ai/benchmarks) quality/cost/latency data, user preference profiles, and per-run urgency.

Decisions: `DECISIONS.md` **P6**, **MS0–MS14**.

## Problem

- Users must hardcode provider lists for every plan/implement/run.
- “Best” model changes quickly; cost vs speed tradeoffs depend on urgency and role (planner vs tester).
- spar already has dual backends and a cheap suite channel, but no shared scoring layer.

## Goals

- Opt-in path: `--select <profile>` (and/or `[model_select]`) resolves to real `cli:` / `api:` slots.
- Profiles weigh quality / cost / speed; urgency remaps weights per run.
- Data from vals coding benches (start: SWE-bench Verified); cache + refresh CLI.
- Deterministic scoring; audit trail in `artifacts/model-select.json`.
- Explicit `--providers` remains supported and default-required until select is opted in.

## Non-goals (v1)

- Official vals API (scrape + `BenchmarkSource` trait only).
- Replacing doctor / quota / suite semantics.
- Perfect global $/seat amortization for CLIs (CLI cost = 0 in score).
- Auto-enabling select for all runs.

## Phases

### A — Data + pick (no auto fleet wiring)

- Fetch/parse SWE-bench page payload (`accuracy`, `latency`, `cost_per_test`).
- Cache under `~/.spar/cache/vals/`; TTL + `spar model refresh`.
- `spar model list --bench swebench --profile value`
- `spar model pick --role implementer --urgency high --json`
- Parser unit tests against fixture HTML/JSON snapshots.

### B — Wire into workflows

- `--select value` (or multi-profile list) on plan / implement / run.
- Map winners → `ProviderRef` + optional model; filter by available backends.
- Write `artifacts/model-select.json`; emit run events.
- Keep `--providers` mutual exclusivity or precedence rules documented.

### C — Roles, urgency, diversity

- [x] Role → profile defaults in config.
- [x] Urgency multipliers.
- [x] Multi-slot diversity (provider family first).
- [x] Suite `tester` prefers fast via model-select.

### D — Adapter depth

- [x] Per-CLI model flags where supported.
- [x] Richer vals id → spar spawn mapping table.
- [x] Optional multi-bench blend.

## Config sketch

```toml
[model_select]
# source = "vals"
# benches = ["swebench"]
# cache_ttl = "24h"
# allow = ["cli:*", "api:openai", "api:xai"]

# [model_select.profiles.best]
# quality = 1.0
# cost = 0.1
# speed = 0.2

# [model_select.profiles.value]
# quality = 0.6
# cost = 0.8
# speed = 0.3
# min_accuracy = 70

# [model_select.profiles.fast]
# quality = 0.4
# cost = 0.3
# speed = 1.0

# [model_select.roles]
# planner = "best"
# implementer = "value"
# reviewer = "value"
# tester = "fast"
# critic = "best"
```

## CLI sketch

```bash
spar model list --bench swebench --profile value
spar model pick --role implementer --urgency high --json
spar model refresh

spar plan -t "…" --select value --urgency low
spar implement --run <id> --select auto --urgency high
```

## Scoring sketch

Within the filtered candidate set, min-max normalize accuracy / cost / latency:

```
score = w_q * acc_norm - w_c * cost_norm - w_s * latency_norm
```

- `cli:*`: cost treated as 0 (MS7).
- Drop candidates below `min_accuracy` or outside allowlist / unavailable backends.

## Acceptance (phase A)

- [x] Fixture-backed parse of SWE-bench overall scores for ≥3 known models
- [x] Cache write/read + refresh path
- [x] `model list` / `model pick --json` stable schema
- [x] Doctor or model subcommand surfaces cache age

## Acceptance (phase B)

- [x] `--select` produces a valid fleet for dry-run plan without `--providers`
- [x] `model-select.json` present on run; explains winner
- [x] Unavailable top model falls through to next scorer hit (via mapping + usability filter)
- [x] Explicit `--providers` still works unchanged

## Risks

- vals HTML/RSC shape changes → pin fixtures; abstract source.
- Model ids on vals ≠ spawn ids on OpenAI/Anthropic/xAI/CLIs → mapping table required.
- All top scores one lab → diversity rule (MS9) for fleets.
