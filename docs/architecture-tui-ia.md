# TUI information architecture: nouns, Home, and gate evidence

**Status:** DECIDED (product planning)  
**Decisions:** U6-U13. Supersedes U3 in part. Extends U1, U4, U5, X2.

---

## Why now

If the session is disposable (see `docs/architecture-operator-model.md`), the TUI becomes
the primary human interface, not a dashboard. It is not currently good enough for that, in
four distinct ways.

1. **You cannot make an informed gate decision.** The Diff tab falls back to dumping the
   artifacts directory, commented "the run's artifacts for now (no new plumbing in Stage
   A)" (`src/tui.rs:3208`). Nothing renders `plan.md` as a document, nothing shows the
   critique, nothing shows `test-contract.md`, and `grep -n "criteria\|verdict" src/tui.rs`
   returns nothing. The ship gate's whole premise is O19: a run cannot pass while any
   `AC-n` is fail or unmentioned, and `unverified` blocks too unless
   `[review].require_all_criteria` relaxes it. The TUI cannot show those criteria. Gate
   buttons without evidence are a rubber stamp.
2. **The vocabulary is conflated.** "Session" is overloaded four ways, all live (see
   Retiring "session" below), and `docs/PRODUCT.md:43` names the home view "Session / run
   home", so the conflation is baked in at the product level.
3. **There is no central place to land.** Launched outside a project you open onto
   `BrowseLevel::Projects`, a list of registered repos; launched inside one
   (`start_in_project`, `src/tui.rs:477-482`) you skip straight to that project's
   `BrowseLevel::Runs`. Either way there is no cross-project view: one is a file browser,
   the other is scoped to a single repo, and neither is a home. The attention data already
   exists: `runs_needing_attention` computes the per-project flag roll-up, but it is used as
   a decoration on a browser row rather than as the organizing principle.
4. **There is no clean way to start a new run.** The `:plan` palette verb takes a task
   (`arg_hint: "<task>"`, `src/tui.rs:167-171`) but has no fleet picker: it bails "select a
   run to reuse its fleet, or use the CLI" (`src/tui.rs:1718-1720`), so creating a run
   requires already having one selected to borrow its fleet from. Per U3 this was
   deliberate: "a fresh fleet needs a provider picker a text palette can't offer, so those
   error to the CLI." Under the operator model that punt is no longer acceptable.

---

## The noun set (U6)

| Noun | Definition | Architectural anchor |
|---|---|---|
| Project | A repo with a `.spar/` | `registry::ProjectEntry` |
| Run | One task, one id, plan through ship | O1, `state.json` |
| Agent | One process doing work: a run's slot or a bare spawned one, bus-addressable by id | W5 |
| Shell | The operator's terminal, attached to a project or an agent | W7, W8 |

Four nouns, not five. "Session" is retired as a user-facing term.

---

## Retiring "session"

"Session" is overloaded four ways in the running product, plus once more in the product doc.
All five are live:

| Current use | Becomes |
|---|---|
| Per-run tmux session `spar-<run_id>` (`tmux::session_name`) | Implementation detail of the tmux backend, never shown to the operator |
| Project workspace shell the Shell tab falls back to (`src/tui.rs:4036-4043`) | Shell |
| `/spawn` agents (`workspace::spawn_agent`) | Agent |
| Provider session logs (`skills/core.md:141`) | Provider-internal, not a spar noun |
| `docs/PRODUCT.md:43` "Session / run home" | Home |

---

## Code drift cleanup

`src/tui.rs:55` documents "Three focus targets, not an N-way ring: the drill-down rail, the
one main area, and the composer. `1` / `2` / `3` jump straight to one," but `Focus` has two
variants (Rail, Main), `3` is unbound, and `App` has no composer field. `src/tui.rs:4161`
still says "Resolve a composer mention to a unique bus id," a holdover from before U3
replaced the Composer focus target with the `:` command palette. Both comments describe a
focus model that no longer exists and get corrected as part of the noun-vocabulary pass
(004).

---

## Home landing view (U7)

Home replaces `BrowseLevel::Projects` as the front door. Four bands, ranked by what the
operator actually needs to see first:

1. **What needs me**, runs at a gate, ranked by wait time. Promotes U5's attention level
   from a rail decoration to the organizing principle.
2. **Running**, active runs, phase visible.
3. **Finished since last look**, a watermark of what landed while the operator was away.
4. **Start something new**, the new-run entry point.

Drill-down survives as navigation, not the front door: `Enter` opens a project's runs or a
run's agents, `Esc` pops back toward Home, exactly as U1's rail already does, but Home is
where you land, not `BrowseLevel::Projects` or `BrowseLevel::Runs`. That holds for the
`start_in_project` launch path too: Home is cross-project by definition, so `spar` run
inside a repo lands on Home scoped to that project's runs and attention state, not on the
project's raw run list. Phase C settles the exact scoping.

---

## New-run flow

`n` on Home opens brief entry: a text field written to `.spar/briefs/<slug>.md`, plus a
fleet picker over the roster. This supersedes U3's punt ("a fresh fleet needs a provider
picker a text palette can't offer, so those error to the CLI") by building the picker rather
than continuing to error to the CLI.

**Amended (U16).** The text field becomes a conversation. `n` opens the resident
orchestrator, which interviews the operator, writes the same brief to the same path, and
proposes a fleet; the picker survives as the manual path that costs no tokens. This settles
X10 yes: the TUI hosts a conversation. It does not disturb the operator model, because the
conversation *is* the disposable session P7 already describes — its prompt is the embedded
core skill, it gets current by reading `.spar/runs/<id>/`, and killing it loses nothing. The
orchestrator is an **Agent** under U6, not a fifth noun, and its surface is a fifth Main tab
whose content is `f(rail selection)` like every other tab. Its authority is bounded by U17:
it proposes, the operator disposes. See `roadmap/features/008-orchestrator-conversation.md`.

---

## Gate evidence (U9)

The Plan tab renders `plan.md`, the critique, and `test-contract.md` as documents rather
than an artifacts-directory dump. The Review tab renders per-`AC-n` pass/fail/unverified
status alongside reviewer verdicts and the git diff. Both are tied directly to the
acceptance predicate O19 enforces (relaxable for `unverified` via `require_all_criteria`):
the gate cannot show that predicate today, and a gate that cannot show its own predicate is
a rubber stamp, not a decision aid.

---

## Rendering discipline

**Time-based motion.** `FRAME = Duration::from_millis(100)` reads as a slideshow (10fps).
`app.animated = animating(&app, &snap)` already exists, so the "burn frames only while
something moves" machinery is in place; it should ramp the frame clock to ~16ms (~60fps)
while animating and idle low otherwise. Animation itself is tick-modulo today, not
time-based: the spinner is `SPINNER[tick % len]` and the cursor blink is
`(app.tick / 6).is_multiple_of(2)` (`src/tui.rs:3746`), so both are frame-rate dependent by
construction and change speed the moment the frame rate varies. This needs an
`Instant`-based motion module: easing curves, `Tween<T>`, enter and exit transitions.

**Layout space reservation.** Four shift sources, one rule fixes all four. Gate buttons are
right-aligned from summed label widths (`src/tui.rs:2740-2745`), so a label change slides
them. The rail re-sorts by attention as data arrives; the cursor-glued-to-run-id fix
(`src/tui.rs:1168-1175`) keeps selection stable but rows still move under the eye. Lists
grow as runs and slots appear, pushing content down. The narrow-tab strip flips mode at a
width breakpoint. The rule: reserve space from the layout, never from the content; content
clips into fixed slots; skeleton placeholders hold space before data lands; reorder becomes
an animated transition.

**Off-thread `Snapshot` scans (U13).** `rail_project_items` calls
`registry::list_visible_project_runs(&p.root)` once per project per redraw to compute the
flag roll-up (`src/tui.rs:2830`). That is a directory scan inside `draw`, the one real
render-path offender. `project_overview`'s call at `src/tui.rs:1312` is not: it runs inside
`build_snapshot` (`src/tui.rs:869`), the refresher thread's off-thread builder, alongside the
correct call at `src/tui.rs:873`. `rail_project_items` moves onto `Snapshot` so `draw` never
touches disk. This bites at the scale already hit once, when run dirs accumulated into the
thousands.

**Design tokens and stability assertions.** A token set replaces scattered color consts,
held to one accent and one alert colour, a consistent border and weight language, and
braille/half-block density where it earns its place. "Rock solid" is asserted, not vibed: a
`TestBackend` snapshot harness covers rendering at 20x5 without panicking, an empty project
list, 400 runs, and "this region does not move when that value changes."

---

## DECIDED rows reconciliation

| Row | Disposition |
|---|---|
| U1 | Extended. Rail + one main area stays; Home replaces the browser as what the rail opens onto |
| U2 | Untouched. The embedded-terminal mode boundary is orthogonal to this doc |
| U3 | Partially superseded. Its palette-verb shape stands; its "a fresh fleet needs a provider picker a text palette can't offer" punt is superseded by U8's new-run flow |
| U4 | Extended. Diff-tab and quit-path behavior stand; U9 adds the Plan/Review evidence rendering on top |
| U5 | Extended. The attention level stops being a rail decoration and becomes Home's organizing principle (U7) |
| X2 | Untouched. Rail + one main area, k9s/lazygit shape, still the answer to "which product does this mimic" |

---

## Sequencing

004 (this doc's IA work) precedes 005 (gate evidence) because evidence rendered into a
confusing layout is still confusing. 006's **motion** half (phases A and B) is last,
because animating a confusing layout gives you a confusing layout at 60fps.

**Amended (U14):** 006's **chrome and tokens** half (phases C and D) landed *first*,
before this doc's 004. The ordering argument above is about motion; chrome is the surface
004 and 005 render into, so building Home and the criteria grid into the old bordered
shell would have meant building them twice. What landed: pane borders replaced by chrome
bands over one seam, the run stepper, the token set in `src/theme.rs`, a terminal-native
background, role-first agent identity in the rail, and the `TestBackend` stability suite.
"Design tokens and stability assertions" under **Rendering discipline** is done, minus
U12's braille/half-block clause: no density glyph earned its place in this pass. Two of
the four layout-shift sources are fixed (width-summed gate buttons, growing lists); the
attention re-sort needs the motion engine, and the narrow tab strip still flips mode at
the 80-column breakpoint. `rail_project_items`' render-path directory scan (U13) is
untouched and remains 004 Phase B's job.
