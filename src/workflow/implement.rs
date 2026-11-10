use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers::{self, HealthSource};
use crate::state::{
    CeilingPartialKind, CeilingPartialRound, FleetSeat, Phase, PoolOrigin, ReviewerSeatKind,
    RunState, SeatSource, SlotRole, SlotState, SlotStatus, SuiteOutcome,
};
use crate::util::{self, sanitize_slot};
use crate::workflow::review_result::{self, AcStatus, ReviewResult};
use crate::worktree;
use anyhow::{bail, Result};
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Amend flags a bound `implement --run <id>` carries into the dispatch. The run's
/// frozen config is amended under the dispatch's own lock, after the launch
/// preconditions pass, so a refused launch changes nothing.
#[derive(Debug, Clone, Default)]
pub struct BoundAmendInputs {
    pub role: Vec<String>,
    pub backup: Vec<String>,
    pub effort: Vec<String>,
    pub without: Vec<String>,
    pub fleet: Option<crate::config::FleetPreset>,
    pub suite_command: Vec<String>,
    pub recapture_env: bool,
}

impl BoundAmendInputs {
    /// No seat shaper was passed. Whether the dispatch reloads the file config
    /// is not part of this: callers pair it with the live config's presence
    /// (`live.is_none() && amend.is_noop()`), so a reload alone still replays
    /// the recorded pool.
    pub fn is_noop(&self) -> bool {
        self.role.is_empty()
            && self.backup.is_empty()
            && self.effort.is_empty()
            && self.without.is_empty()
            && self.suite_command.is_empty()
            && matches!(
                self.fleet,
                None | Some(crate::config::FleetPreset::Standard)
            )
    }
}

/// An amend-phase refusal: the frozen config and run state were left untouched,
/// so the caller reports it as a launch refusal (exit 1), never an internal error.
#[derive(Debug)]
pub struct AmendRefused(pub String);

impl std::fmt::Display for AmendRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for AmendRefused {}

/// The amended config plus the reviewer ordinals the amend dropped, computed
/// read-only from the frozen config, the run state the surgery will apply to,
/// and the live file config when that is present (its presence means reload).
/// Both the `--validate-only` preview and the under-lock dispatch path use it;
/// the dispatch path calls it after acquiring the run lock so seat-id
/// resolution and the panel pin see the state the surgery is about to change.
pub struct BoundAmend {
    pub cfg: Config,
    pub dropped: Vec<usize>,
    /// The pin came from the run's own slots: a reassign that moves reviewer
    /// seats afterwards must refresh it before the freeze.
    pub panel_pinned: bool,
    /// The reviewer panel the amend will leave, simulated read-only: `--validate-only`
    /// shows these seats instead of the stale live ones. `None` when the amend
    /// touches no reviewer seat. Freshly minted seats are `projected`.
    pub resynced_panel: Option<Vec<FleetSeat>>,
    /// Live seats the re-seat retired beyond the explicit drops (a replaced
    /// provider, a panel position past the pin): `--validate-only` hides
    /// exactly these, so widened extras sitting outside the panel survive the
    /// splice.
    pub resync_removed: Vec<String>,
}

/// Which reviewer seat a slot holds. A panel seat owns a backup-list position
/// and a `roles.reviewer` config entry; a widened extra sits past the panel
/// and owns neither. The kind is recorded on the slot when the seat is minted;
/// ids written before that field existed still parse, so they fall back to the
/// `-wide` suffix here, the one place that read remains.
fn reviewer_seat_kind(slot: &SlotState) -> Option<ReviewerSeatKind> {
    if slot.role != SlotRole::Reviewer {
        return None;
    }
    if let Some(kind) = slot.reviewer_kind {
        return Some(kind);
    }
    if slot.id.ends_with("-wide") {
        Some(ReviewerSeatKind::Wide)
    } else {
        Some(ReviewerSeatKind::Panel)
    }
}

/// The one panel-membership predicate. A widened extra is a reviewer slot but
/// never a panel seat: it holds no backup-list position and no config entry.
fn is_panel_reviewer(slot: &SlotState) -> bool {
    reviewer_seat_kind(slot) == Some(ReviewerSeatKind::Panel)
}

/// The run's own reviewer panel in live panel order, never a widened extra: the
/// same slot order `panel_position` and `panel_seat_with_ordinal` index, so an
/// ordinal into this list is a live panel position.
fn reviewer_panel_by_ordinal(state: &RunState) -> Vec<String> {
    state
        .slots
        .iter()
        .filter(|s| is_panel_reviewer(s))
        .map(|s| s.provider_ref().display())
        .collect()
}

pub fn preview_bound_amend(
    frozen: &Config,
    live: Option<&Config>,
    state: &RunState,
    inputs: &BoundAmendInputs,
) -> Result<BoundAmend> {
    let mut cfg = if let Some(live) = live {
        let mut cfg = live.clone();
        cfg.reapply_cli_overrides(frozen);
        // A reload re-reads the file, not the shell: names still captured keep
        // the creation values, a name removed from `capture` is dropped, and
        // only a name newly added to `capture` takes this process's value (O130).
        // `--recapture-env` is the deliberate exception: everything is retaken.
        if !inputs.recapture_env {
            for (k, v) in &frozen.env.captured {
                if cfg.env.capture.contains(k) {
                    cfg.env.captured.insert(k.clone(), v.clone());
                }
            }
        }
        cfg
    } else {
        frozen.clone()
    };
    if let Some(preset) = inputs.fleet {
        cfg.apply_fleet_preset(preset);
    }
    let without = resolve_without_seat_ids(state, &inputs.without)?;
    let mut panel_pinned = false;
    if without
        .iter()
        .any(|w| crate::config::parse_reviewer_without(w).is_ok())
    {
        let panel = reviewer_panel_by_ordinal(state);
        if !panel.is_empty() {
            cfg.roles.reviewer = panel;
            cfg.cli_role_keys.insert(SlotRole::Reviewer);
            panel_pinned = true;
        }
    }
    let (pool, pool_origin, dry) = if !state.providers.is_empty() {
        (state.providers.clone(), state.pool_origin, state.dry_run)
    } else {
        (
            cfg.providers.order.clone(),
            PoolOrigin::Synth,
            state.dry_run,
        )
    };
    let dropped = cfg.apply_without(&pool, pool_origin, dry, &without)?;
    cfg.apply_role_overrides(&inputs.role)?;
    if crate::config::role_pins_reviewer(&inputs.role) {
        // The role replaces the panel outright: the live-slot pin is no longer
        // the panel's source, so a later reassign must not refresh it back.
        panel_pinned = false;
    }
    // The refusal judges the panel the amend will leave: the live panel after
    // the planned retirements, re-seated to the pin when one applies. The
    // simulation runs on a throwaway clone, so both this refusal and
    // `--validate-only` read the same outcome the dispatch will apply.
    let resync_applies = bound_panel_resync_applies(state, &inputs.role, cfg.roles.reviewer.len());
    let mut resync_removed = Vec::new();
    let resynced_panel: Option<Vec<FleetSeat>> = if !dropped.is_empty() || resync_applies {
        let mut sim = state.clone();
        retire_reviewer_seats(&mut sim, &dropped)?;
        let mut sim_backups = cfg.backups.reviewer.clone();
        if resync_applies {
            let pin = parse_reviewer_pin(&cfg.roles.reviewer)?;
            resync_removed = resync_reviewer_panel_to_pin(&mut sim, &mut sim_backups, &pin)?;
        }
        Some(
            sim.slots
                .iter()
                .filter(|s| is_panel_reviewer(s))
                .map(|s| FleetSeat {
                    seat: s.id.clone(),
                    role: SlotRole::Reviewer,
                    provider: s.provider.clone(),
                    model: s.model.clone(),
                    source: s.source.unwrap_or(SeatSource::Unknown),
                    projected: !state.slots.iter().any(|live| live.id == s.id),
                })
                .collect(),
        )
    } else {
        None
    };
    // The refusal judges the panel the dispatch will actually seat: the
    // simulated live panel where seats already exist, the amended config
    // panel on a first dispatch that mints it once from the config.
    let sim_empty = resynced_panel
        .as_ref()
        .is_some_and(|panel| panel.is_empty());
    let emptied = if state.slots.iter().any(is_panel_reviewer) {
        sim_empty
    } else {
        cfg.roles.reviewer.is_empty()
    };
    if !dropped.is_empty() && emptied {
        anyhow::bail!("{}", crate::config::emptied_reviewer_panel_message());
    }
    crate::config::warn_if_role_supersedes_reviewer_drop(&inputs.role, &dropped, true);
    cfg.apply_backup_overrides(&inputs.backup)?;
    cfg.apply_effort_overrides(&inputs.effort)?;
    cfg.apply_suite_command(&inputs.suite_command, &without)?;
    Ok(BoundAmend {
        cfg,
        dropped,
        panel_pinned,
        resynced_panel,
        resync_removed,
    })
}

/// Seat ids in `--without` resolved to `reviewer[N]` ordinals against one run
/// state. Channels and ordinals pass through; anything else is refused naming
/// the run's own seats.
pub fn resolve_without_seat_ids(state: &RunState, without: &[String]) -> Result<Vec<String>> {
    without
        .iter()
        .map(|w| {
            let item = w.trim();
            if crate::config::DroppedChannel::parse(item).is_some()
                || crate::config::parse_reviewer_without(item).is_ok()
            {
                return Ok(w.clone());
            }
            let slot = state.slots.iter().find(|s| s.id == item).ok_or_else(|| {
                let mut seats: Vec<&str> = state
                    .slots
                    .iter()
                    .filter(|s| s.role == SlotRole::Reviewer)
                    .map(|s| s.id.as_str())
                    .collect();
                seats.sort();
                anyhow::anyhow!(
                    "--without {item:?}: no such seat on run {} (valid: critic, spec, suite, reviewer[N], or one of: {})",
                    state.id,
                    seats.join(", ")
                )
            })?;
            if slot.role != SlotRole::Reviewer {
                anyhow::bail!("--without {item:?}: only reviewer seats can be dropped by seat id");
            }
            if reviewer_seat_kind(slot) == Some(ReviewerSeatKind::Wide) {
                anyhow::bail!("--without {item:?}: extra seats from widening cannot be dropped; drop a panel seat as reviewer[N]");
            }
            // The id names a live panel seat; the ordinal is its position in the
            // current panel, not the one baked into the id.
            let position = panel_position(state, item).ok_or_else(|| {
                anyhow::anyhow!("--without {item:?}: seat {item} vanished from the panel")
            })?;
            Ok(format!("reviewer[{position}]"))
        })
        .collect()
}

#[cfg(test)]
mod preview_amend_tests {
    use super::*;

    fn dry_stateless_run() -> RunState {
        let mut st = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        st.dry_run = true;
        st
    }

    /// A seat id names a live panel seat by position, not by the ordinal baked
    /// into the id: after a drop retires review-1, review-2-cli-codex sits at
    /// panel position 1.
    #[test]
    fn seat_id_without_resolves_to_the_live_panel_position() {
        let mut st = dry_stateless_run();
        st.slots.push(executor::init_slot(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        st.slots.push(executor::init_slot(
            "review-2-cli-codex",
            "cli:codex",
            SlotRole::Reviewer,
        ));
        let out = resolve_without_seat_ids(&st, &["review-2-cli-codex".to_string()]).unwrap();
        assert_eq!(out, vec!["reviewer[1]".to_string()]);
    }

    /// A dry-run run with no recorded pool resolves the reviewer
    /// panel with the run's own dry-run state, never a hard `false`, so the
    /// order-derived panel survives availability filtering.
    #[test]
    fn pool_less_dry_run_amend_indexes_the_order_panel() {
        let inputs = BoundAmendInputs {
            without: vec!["reviewer[0]".to_string()],
            ..BoundAmendInputs::default()
        };
        let amend =
            preview_bound_amend(&Config::default(), None, &dry_stateless_run(), &inputs).unwrap();
        assert_eq!(amend.dropped, vec![0]);
        assert_eq!(amend.cfg.roles.reviewer, vec!["cli:grok".to_string()]);
    }

    /// The bound pin is the run's panel in live slot order, the
    /// same order `panel_position` and `panel_seat_with_ordinal` index, never a
    /// second id-ordinal sort; widened extras are not panel seats. Slots stored
    /// codex-first pin codex-first, and the `-wide` extra is neither pinned nor
    /// counted.
    #[test]
    fn bound_pin_follows_slot_order_and_ignores_wide_extras() {
        let mut st = dry_stateless_run();
        for (id, provider) in [
            ("review-1-cli-codex", "cli:codex"),
            ("review-0-cli-claude", "cli:claude"),
            ("review-cli-grok-wide", "cli:grok"),
        ] {
            st.slots
                .push(executor::init_slot(id, provider, SlotRole::Reviewer));
        }
        let inputs = BoundAmendInputs {
            without: vec!["reviewer[1]".to_string()],
            ..BoundAmendInputs::default()
        };
        let amend = preview_bound_amend(&Config::default(), None, &st, &inputs).unwrap();
        assert_eq!(amend.dropped, vec![1]);
        assert_eq!(amend.cfg.roles.reviewer, vec!["cli:codex".to_string()]);
    }

    /// The last-seat refusal judges the final panel: `--role reviewer=` re-seats
    /// it, so a drop plus a re-seat is allowed, while a drop alone is refused.
    #[test]
    fn drop_plus_role_reseat_is_allowed() {
        let mut st = dry_stateless_run();
        st.slots.push(executor::init_slot(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        let inputs = BoundAmendInputs {
            without: vec!["reviewer[0]".to_string()],
            role: vec!["reviewer=cli:codex".to_string()],
            ..BoundAmendInputs::default()
        };
        let amend = preview_bound_amend(&Config::default(), None, &st, &inputs).unwrap();
        assert_eq!(amend.dropped, vec![0]);
        assert_eq!(amend.cfg.roles.reviewer, vec!["cli:codex".to_string()]);
        let panel = amend.resynced_panel.unwrap();
        assert_eq!(panel.len(), 1);
        assert_eq!(panel[0].seat, "review-0-cli-codex");
        assert!(panel[0].projected);
        let bare = BoundAmendInputs {
            without: vec!["reviewer[0]".to_string()],
            ..BoundAmendInputs::default()
        };
        match preview_bound_amend(&Config::default(), None, &st, &bare) {
            Ok(_) => panic!("a drop with no re-seat must be refused"),
            Err(e) => assert!(
                format!("{e:#}").contains("cannot drop the last reviewer panel seat"),
                "{e:#}"
            ),
        }
    }

    /// A drop-all on a run with no live panel seats refuses in the preview,
    /// before anything persists: with no seats to retire and no pin to re-seat
    /// from, the amend would leave a config no reviewer can review under.
    #[test]
    fn drop_all_with_no_live_panel_refuses() {
        let inputs = BoundAmendInputs {
            without: vec!["reviewer[0]".to_string(), "reviewer[1]".to_string()],
            ..BoundAmendInputs::default()
        };
        match preview_bound_amend(&Config::default(), None, &dry_stateless_run(), &inputs) {
            Ok(_) => panic!("a drop-all with no live panel and no pin must be refused"),
            Err(e) => assert!(
                format!("{e:#}").contains("cannot drop the last reviewer panel seat"),
                "{e:#}"
            ),
        }
    }

    /// A pin on a first dispatch simulates nothing: no seats exist yet, so the
    /// fresh dispatch mints the panel once from the amended config.
    #[test]
    fn role_pin_on_a_first_dispatch_simulates_nothing() {
        let inputs = BoundAmendInputs {
            role: vec!["reviewer=cli:codex".to_string()],
            ..BoundAmendInputs::default()
        };
        let amend =
            preview_bound_amend(&Config::default(), None, &dry_stateless_run(), &inputs).unwrap();
        assert_eq!(amend.cfg.roles.reviewer, vec!["cli:codex".to_string()]);
        assert!(amend.resynced_panel.is_none());
    }

    /// A pin over a live two-seat panel simulates the single re-seated seat the
    /// dispatch will leave: the retired id is never reminted.
    #[test]
    fn role_pin_simulates_the_reseated_panel() {
        let mut st = dry_stateless_run();
        for (id, provider) in [
            ("review-0-cli-claude", "cli:claude"),
            ("review-1-cli-grok", "cli:grok"),
        ] {
            st.slots
                .push(executor::init_slot(id, provider, SlotRole::Reviewer));
        }
        let inputs = BoundAmendInputs {
            role: vec!["reviewer=cli:codex".to_string()],
            ..BoundAmendInputs::default()
        };
        let amend = preview_bound_amend(&Config::default(), None, &st, &inputs).unwrap();
        let panel = amend.resynced_panel.unwrap();
        assert_eq!(panel.len(), 1);
        assert_eq!(panel[0].seat, "review-0-cli-codex");
        assert_eq!(panel[0].provider, "cli:codex");
        assert!(panel[0].projected);
    }

    /// O130: a reload re-reads the file, not the shell. Names still captured keep
    /// the creation values, a name removed from `capture` is dropped, and only a
    /// name newly added to `capture` takes this process's value; `--recapture-env`
    /// retakes everything.
    #[test]
    fn reload_merges_env_per_name() {
        fn env(capture: &[&str], captured: &[(&str, &str)]) -> crate::config::EnvConfig {
            crate::config::EnvConfig {
                capture: capture.iter().map(|s| s.to_string()).collect(),
                captured: captured
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        }
        let frozen = Config {
            env: env(
                &["KEEP", "GONE"],
                &[("KEEP", "frozen"), ("GONE", "frozen-gone")],
            ),
            ..Config::default()
        };
        let live = Config {
            env: env(&["KEEP", "NEW"], &[("KEEP", "shell"), ("NEW", "shell-new")]),
            ..Config::default()
        };
        let kept = preview_bound_amend(
            &frozen,
            Some(&live),
            &dry_stateless_run(),
            &BoundAmendInputs::default(),
        )
        .unwrap();
        assert_eq!(
            kept.cfg.env.captured.get("KEEP").map(String::as_str),
            Some("frozen")
        );
        assert_eq!(
            kept.cfg.env.captured.get("NEW").map(String::as_str),
            Some("shell-new")
        );
        assert!(!kept.cfg.env.captured.contains_key("GONE"));
        let retaken = preview_bound_amend(
            &frozen,
            Some(&live),
            &dry_stateless_run(),
            &BoundAmendInputs {
                recapture_env: true,
                ..BoundAmendInputs::default()
            },
        )
        .unwrap();
        assert_eq!(
            retaken.cfg.env.captured.get("KEEP").map(String::as_str),
            Some("shell")
        );
        assert_eq!(
            retaken.cfg.env.captured.get("NEW").map(String::as_str),
            Some("shell-new")
        );
    }
}

#[cfg(test)]
mod base_merge_tests {
    use super::*;

    /// A repo with a bare `origin` plus a clone standing in for the run's
    /// implementer worktree.
    struct MergeSetup {
        _tmp: tempfile::TempDir,
        paths: SparPaths,
        repo: std::path::PathBuf,
        run: std::path::PathBuf,
        origin_main: String,
    }

    fn tgit(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn setup() -> MergeSetup {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let repo = tmp.path().join("repo");
        let run = tmp.path().join("run");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        tgit(&origin, &["init", "-q", "--bare"]);
        tgit(&repo, &["init", "-q", "-b", "main"]);
        tgit(&repo, &["config", "user.email", "t@t.com"]);
        tgit(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        tgit(&repo, &["add", "."]);
        tgit(&repo, &["commit", "-q", "-m", "init"]);
        tgit(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        tgit(&repo, &["push", "-q", "origin", "main"]);
        tgit(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-b",
                "main",
                origin.to_str().unwrap(),
                run.to_str().unwrap(),
            ],
        );
        tgit(&run, &["config", "user.email", "t@t.com"]);
        tgit(&run, &["config", "user.name", "t"]);
        tgit(&run, &["checkout", "-q", "-b", "spar/r1/impl"]);
        let origin_main = tgit(&repo, &["rev-parse", "HEAD"]);
        let paths = SparPaths::new(repo.clone());
        MergeSetup {
            _tmp: tmp,
            paths,
            repo,
            run,
            origin_main,
        }
    }

    /// States in these tests load the way production loads them: through the
    /// same JSON the run file uses, so only the four fields the merge reads
    /// are named.
    fn round_state(fx: &MergeSetup, round: u32) -> RunState {
        let mut st = RunState::new("r1", crate::cli::WorkflowKind::Loop, fx.repo.clone());
        st.base_ref = Some("main".to_string());
        st.base_commit = Some(fx.origin_main.clone());
        st.merged_base = Some(fx.origin_main.clone());
        st.round = round;
        st.slots = vec![serde_json::from_value(serde_json::json!({
            "id": "impl",
            "provider": "cli:claude",
            "role": "implementer",
            "status": "done",
            "cwd": fx.run,
        }))
        .unwrap()];
        st
    }

    fn advance_origin(fx: &MergeSetup, contents: &str) -> String {
        std::fs::write(fx.repo.join("file.txt"), contents).unwrap();
        tgit(&fx.repo, &["add", "."]);
        tgit(&fx.repo, &["commit", "-q", "-m", "advance base"]);
        tgit(&fx.repo, &["push", "-q", "origin", "main"]);
        tgit(&fx.repo, &["rev-parse", "HEAD"])
    }

    /// #229: round 1 never merges; the run branch stays behind the moved base.
    #[test]
    fn round_one_never_merges_the_base_forward() {
        let fx = setup();
        let tip = advance_origin(&fx, "base\nmore\n");
        let mut st = round_state(&fx, 1);
        let cfg = Config::default();
        assert!(merge_base_at_round_start(&mut st, &fx.paths, &cfg)
            .unwrap()
            .is_none());
        let head = tgit(&fx.run, &["rev-parse", "HEAD"]);
        assert_ne!(head, tip);
        assert!(st.merged_base == Some(fx.origin_main.clone()));
    }

    /// #229: round 2 merges a cleanly moved base and records the new tip.
    #[test]
    fn round_two_merges_a_moved_base_and_records_it() {
        let fx = setup();
        std::fs::write(fx.run.join("run.txt"), "run work\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        let tip = advance_origin(&fx, "base\nmore\n");
        let mut st = round_state(&fx, 2);
        let cfg = Config::default();
        assert!(merge_base_at_round_start(&mut st, &fx.paths, &cfg)
            .unwrap()
            .is_none());
        assert_eq!(st.merged_base.as_deref(), Some(tip.as_str()));
        assert!(tgit(&fx.run, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_empty());
    }

    /// #229: a conflicted round-start merge is handed to the implementer as a
    /// prompt section naming the base and the conflicting files, with the
    /// pending tip the pre-review guard verifies a resolution against.
    #[test]
    fn round_two_conflict_hands_a_section_to_the_implementer() {
        let fx = setup();
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        let tip = advance_origin(&fx, "base side\n");
        let mut st = round_state(&fx, 2);
        let cfg = Config::default();
        let (section, pending) = merge_base_at_round_start(&mut st, &fx.paths, &cfg)
            .unwrap()
            .expect("conflict hands a section to the implementer");
        assert!(section.starts_with("## Base merge conflict"));
        assert!(section.contains("`main`"));
        assert!(section.contains("`file.txt`"));
        assert_eq!(pending, tip);
        assert!(crate::worktree::merge_in_progress(&fx.run));
        assert_eq!(st.merged_base.as_deref(), Some(fx.origin_main.as_str()));
    }

    /// #229: a merge the previous round left open is re-handed, not committed
    /// over and not cleared: the section returns with the open merge's tip and
    /// `merged_base` stays on the stale base until a resolution lands.
    #[test]
    fn round_three_rehands_a_merge_left_open_with_its_tip() {
        let fx = setup();
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        advance_origin(&fx, "base side\n");
        let mut st = round_state(&fx, 2);
        let cfg = Config::default();
        let (_, first_tip) = merge_base_at_round_start(&mut st, &fx.paths, &cfg)
            .unwrap()
            .expect("round two hands the conflict over");
        assert!(crate::worktree::merge_in_progress(&fx.run));

        let mut st3 = round_state(&fx, 3);
        let (section, pending) = merge_base_at_round_start(&mut st3, &fx.paths, &cfg)
            .unwrap()
            .expect("round three re-hands the open merge");
        assert!(section.starts_with("## Base merge conflict"));
        assert_eq!(pending, first_tip);
        assert_eq!(
            pending,
            tgit(&fx.run, &["rev-parse", "MERGE_HEAD"]),
            "the pending tip is the open merge's other parent"
        );
        assert_eq!(st3.merged_base.as_deref(), Some(fx.origin_main.as_str()));
    }

    /// #229: resolving the handed merge (all conflicts staged and committed)
    /// reads as resolved against the pending tip.
    #[test]
    fn base_merge_resolution_accepts_a_committed_resolution() {
        let fx = setup();
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        let tip = advance_origin(&fx, "base side\n");
        let mut st = round_state(&fx, 2);
        let (_, pending) = merge_base_at_round_start(&mut st, &fx.paths, &Config::default())
            .unwrap()
            .expect("conflict hands a section to the implementer");
        assert_eq!(
            base_merge_resolution(&fx.run, &pending),
            BaseMergeResolution::InProgress
        );
        std::fs::write(fx.run.join("file.txt"), "resolved\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "--no-edit"]);
        assert_eq!(
            base_merge_resolution(&fx.run, &tip),
            BaseMergeResolution::Resolved
        );
    }

    /// #229: aborting the handed merge reads as aborted: the tip is no longer
    /// in the branch's history and the base is unmerged again.
    #[test]
    fn base_merge_resolution_rejects_an_abort() {
        let fx = setup();
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        advance_origin(&fx, "base side\n");
        let mut st = round_state(&fx, 2);
        let (_, pending) = merge_base_at_round_start(&mut st, &fx.paths, &Config::default())
            .unwrap()
            .expect("conflict hands a section to the implementer");
        tgit(&fx.run, &["merge", "--abort"]);
        assert!(!crate::worktree::merge_in_progress(&fx.run));
        assert_eq!(
            base_merge_resolution(&fx.run, &pending),
            BaseMergeResolution::Aborted
        );
    }

    /// #229: the pre-review verdict records a resolution the implementer
    /// committed during the round, and fails the round while the merge is
    /// still open. This is the in-round path the dry-run suite cannot reach
    /// through the CLI (the resolution lands mid-round during a real
    /// dispatch), driven here against a real repo instead.
    #[test]
    fn pre_review_verdict_records_a_committed_resolution() {
        let fx = setup();
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        let tip = advance_origin(&fx, "base side\n");
        let mut st = round_state(&fx, 2);
        let (_, pending) = merge_base_at_round_start(&mut st, &fx.paths, &Config::default())
            .unwrap()
            .expect("conflict hands a section to the implementer");
        let err = pre_review_merge_verdict(&fx.run, "main", Some(&pending)).unwrap_err();
        assert!(
            err.contains("unresolved") && err.contains("file.txt"),
            "{err}"
        );
        std::fs::write(fx.run.join("file.txt"), "resolved\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "--no-edit"]);
        assert_eq!(
            pre_review_merge_verdict(&fx.run, "main", Some(&pending)),
            Ok(Some(tip))
        );
    }

    /// #229: the pre-review verdict fails a merge the implementer aborted
    /// instead of resolving.
    #[test]
    fn pre_review_verdict_fails_an_aborted_merge() {
        let fx = setup();
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        advance_origin(&fx, "base side\n");
        let mut st = round_state(&fx, 2);
        let (_, pending) = merge_base_at_round_start(&mut st, &fx.paths, &Config::default())
            .unwrap()
            .expect("conflict hands a section to the implementer");
        tgit(&fx.run, &["merge", "--abort"]);
        let err = pre_review_merge_verdict(&fx.run, "main", Some(&pending)).unwrap_err();
        assert!(err.contains("aborted"), "{err}");
    }

    /// #229: the pre-review verdict passes a round with nothing handed, but
    /// still fails a merge left open from outside the handoff.
    #[test]
    fn pre_review_verdict_passes_a_round_with_nothing_handed() {
        let fx = setup();
        assert_eq!(pre_review_merge_verdict(&fx.run, "main", None), Ok(None));
        std::fs::write(fx.run.join("file.txt"), "run side\n").unwrap();
        tgit(&fx.run, &["add", "."]);
        tgit(&fx.run, &["commit", "-q", "-m", "run work"]);
        advance_origin(&fx, "base side\n");
        let conflicted = crate::worktree::merge_base_forward(&fx.run, Some("main")).unwrap();
        assert!(matches!(
            conflicted,
            crate::worktree::BaseMerge::Conflict { .. }
        ));
        let err = pre_review_merge_verdict(&fx.run, "main", None).unwrap_err();
        assert!(err.contains("unresolved"), "{err}");
    }

    /// #229: `[rounds] merge_base = false` keeps the run on its creation base.
    #[test]
    fn disabled_merge_base_skips_the_forward_merge() {
        let fx = setup();
        let tip = advance_origin(&fx, "base\nmore\n");
        let mut st = round_state(&fx, 2);
        let mut cfg = Config::default();
        cfg.rounds.merge_base = false;
        assert!(merge_base_at_round_start(&mut st, &fx.paths, &cfg)
            .unwrap()
            .is_none());
        let head = tgit(&fx.run, &["rev-parse", "HEAD"]);
        assert_ne!(head, tip);
    }

    /// #229: dry runs never touch git.
    #[test]
    fn dry_run_skips_the_forward_merge() {
        let fx = setup();
        let tip = advance_origin(&fx, "base\nmore\n");
        let mut st = round_state(&fx, 2);
        st.dry_run = true;
        let cfg = Config::default();
        assert!(merge_base_at_round_start(&mut st, &fx.paths, &cfg)
            .unwrap()
            .is_none());
        let head = tgit(&fx.run, &["rev-parse", "HEAD"]);
        assert_ne!(head, tip);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_from_cli(
    run_id: Option<String>,
    plan: Option<PathBuf>,
    task: Option<String>,
    new: bool,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
    amend: BoundAmendInputs,
) -> Result<ExitCode> {
    if let Some(t) = &task {
        crate::workflow::validate_task(t).map_err(anyhow::Error::msg)?;
    }
    if let Some(id) = run_id {
        return run_from_approved(&id, task, opts, paths, amend);
    }
    if let Some(plan_path) = plan {
        // A plan belongs to the run that produced it. Implementing it is that run
        // continuing, not a new one (O45) — this is the bypass that split biddesk's
        // work across two ids 35 times.
        // `--new` says this is separate work, so it wins over the trace.
        if let Some(id) = run_id_for_plan(&plan_path, paths).filter(|_| !new) {
            if opts.key.is_some() {
                bail!(
                    "--key names a new run; this plan belongs to run {id}, which keeps its own key"
                );
            }
            if !opts.json {
                eprintln!("continuing run {id} (it wrote this plan)");
            }
            return run_from_approved(&id, task, opts, paths, amend);
        }
        if !new {
            bail!("{}", unlinked_plan_error(&plan_path, paths));
        }
        let body = std::fs::read_to_string(&plan_path)?;
        let task =
            task.unwrap_or_else(|| format!("Implement approved plan from {}", plan_path.display()));
        return run_with_task(task, Some(body), opts, paths, cfg, None);
    }
    let task =
        task.ok_or_else(|| anyhow::anyhow!("implement requires --run, --plan, or --task"))?;
    run_with_task(task, None, opts, paths, cfg, None)
}

/// Put a resumed run's interrupted slots back to `Pending`.
///
/// `spar stop` reconciles every slot it interrupts from `Running` to `Failed`. The
/// non-plan workflows rebuild their job list from *every* retained slot
/// (`review.rs`'s `state.slots.iter().map(..)`) and `executor::run_slot` flips each
/// one back to `Running`, so those slots are about to run — but
/// `daemon::run_demand` counts only `Pending`/`Running`, so the run reads as needing
/// nothing and `maybe_enqueue` admits it however full the bucket is. The cap is then
/// overrun the moment dispatch starts.
///
/// This is a rule, not a tidy-up: what the cap reserves has to be what is about to
/// run.
fn restore_interrupted_slots(state: &mut RunState) {
    for slot in &mut state.slots {
        if slot.status == SlotStatus::Failed {
            slot.status = SlotStatus::Pending;
        }
    }
}

/// `spar resume <id>`: dispatch on what the run actually is, rather than aliasing
/// `implement --run`. A gate is refused (a decision is waiting, and resume is not an
/// operator); a live owner is refused naming its pid; an abandoned in-flight run goes
/// to `continue_run` in the foreground, or (with `--detach`) through the same
/// `detach_implement` a fresh `implement --detach` uses, since `continue_run` already
/// dispatches on `state.workflow` regardless of which process calls it. A stopped
/// `Plan` run — the one workflow with a plan-approval gate — either refuses (never
/// approved) or falls through to `run_from_approved` (approved, or `Loop`, which has no
/// approval step at all). A stopped `Arena`/`Roles`/`Peer`/`Review` run has no approval
/// gate to check at all, so it is routed straight to `continue_run` instead: those
/// workflows are not `run_from_approved`'s business, since it only knows approved plans
/// and `Loop`, and `prepare_implement_slots` would otherwise rewrite the run's workflow
/// to `Loop`, silently changing what the run is.
pub fn resume(
    paths: &SparPaths,
    cfg: &Config,
    run_id: &str,
    detach: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if let Some(owner) = crate::runlock::RunLock::owner(paths, run_id) {
        if owner.alive() {
            return Err(crate::runlock::OrchestratorBusy {
                run_id: run_id.to_string(),
                owner_pid: owner.pid,
            }
            .into());
        }
    }
    if state.amend_in_progress.is_some() {
        let lock = crate::runlock::RunLock::acquire(paths, run_id)?;
        state = RunState::load(paths, run_id)?;
        crate::workflow::plan::recover_interrupted_amend(&mut state, paths)?;
        drop(lock);
    }
    if state.phase.is_gate() {
        bail!(
            "run {run_id} is waiting on a human ({:?}); resolve it with: {}",
            state.phase,
            crate::notify::next_command(&state)
        );
    }
    if matches!(state.phase, Phase::Done | Phase::PlanRejected) {
        bail!(
            "run {run_id} is finished ({:?}); nothing to resume. Start a new round with \
             `spar plan --run {run_id} -t \"…\"`.",
            state.phase
        );
    }
    // `spar stop` parks a plan run at `Stopped` before it was ever approved just as
    // readily as it parks an approved run. Resuming that straight into `run_from_approved`
    // would drive an unapproved plan through to ship — the same hazard `Phase::Quota`
    // already guards against there. Refuse here, by name, instead of falling through to
    // `run_from_approved`'s generic "plan is not approved" bail: resume's contract is that
    // it names the human command for every state it refuses.
    //
    // `Plan` is the *only* workflow with an approval gate, so it is the only workflow this
    // check applies to. `Loop` has none and resumes below via `run_from_approved`'s own
    // `Stopped`+`Loop` disjunct. `Arena`/`Roles`/`Peer`/`Review` have none either, and are
    // routed to their own continuation just below — never through `run_from_approved`,
    // which would treat them as unapproved plans and refuse, or worse, rewrite them.
    if state.phase == Phase::Stopped
        && state.workflow == crate::cli::WorkflowKind::Plan
        && !state.gates.plan_approved
    {
        bail!(
            "run {run_id} is a stopped plan run that was never approved; resuming it would \
             drive an unapproved plan through to ship. Replan it with \
             `spar plan --run {run_id} -t \"…\"`, or if a plan is already written and \
             awaiting a decision, `spar approve {run_id}`."
        );
    }
    if state.phase == Phase::Stopped
        && !matches!(
            state.workflow,
            crate::cli::WorkflowKind::Plan | crate::cli::WorkflowKind::Loop
        )
    {
        let lock = crate::runlock::RunLock::acquire(paths, run_id)?;
        let mut state = RunState::load(paths, run_id)?;
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
        restore_interrupted_slots(&mut state);
        state.save(paths)?;
        drop(lock);
        if detach {
            return detach_implement(&state, paths, json);
        }
        return continue_run(paths, cfg, run_id);
    }
    // Shipping is the operator's step, never a supervisor's (O70): a run that died
    // mid-ship goes back to the ship gate instead of re-running its rounds (O117).
    if state.phase == Phase::Shipping {
        return park_interrupted_ship(paths, run_id, json);
    }
    // An approved plan run has finished planning, so an in-flight park continues into
    // implementation through `run_from_approved` rather than into a second plan round
    // that would ask for the approval again.
    let approved_plan =
        state.workflow == crate::cli::WorkflowKind::Plan && state.gates.plan_approved;
    if !state.phase.is_waitable_stop() && !approved_plan {
        // `continue_run` has no detach knob of its own: it always runs to completion
        // in the foreground of whoever calls it. `--detach` here means the same thing
        // it means for `plan`/`implement`: spawn `__internal_continue` into a session
        // of its own and hand back once it holds the run's lock, rather than running
        // it in this process.
        if detach {
            return detach_implement(&state, paths, json);
        }
        return continue_run(paths, cfg, run_id);
    }
    // `run_from_approved` always re-decides `state.dry_run` from `opts.resolve_dry_run()`
    // (it is not sticky across `implement --run` re-entries, by design, so a test can
    // force a live run into stub mode). `resume` offers no `--dry-run` flag of its own —
    // resuming does not get to change what kind of run this is — so it carries the run's
    // own recorded flag through instead of silently defaulting to `false` and dispatching
    // a dry-run's later rounds against real providers and real git merges.
    let opts = CommonOpts {
        detach,
        json,
        dry_run: state.dry_run,
        ..CommonOpts::default()
    };
    run_from_approved(run_id, None, opts, paths, BoundAmendInputs::default())
}

/// The run that wrote this plan, if spar wrote it: plan artifacts live at
/// `.spar/runs/<id>/artifacts/plan*.md`, so the id is in the path.
fn run_id_for_plan(plan: &std::path::Path, paths: &SparPaths) -> Option<String> {
    let plan = plan.canonicalize().ok()?;
    if plan.extension()?.to_str()? != "md" {
        return None;
    }
    let runs = paths.runs_dir().canonicalize().ok()?;
    let rest = plan.strip_prefix(&runs).ok()?;
    let mut parts = rest.components();
    let id = parts.next()?.as_os_str().to_str()?.to_string();
    // Exactly `<id>/artifacts/<something>.md`. Anything else under the run dir — a
    // log, a marker, a nested project's `.spar` — is not a plan this run wrote, and
    // attaching to it would re-dispatch the whole implement fleet on a typo.
    if parts.next()?.as_os_str() != "artifacts" {
        return None;
    }
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    paths.state_file(&id).is_file().then_some(id)
}

/// How this dispatch re-entered an existing run. Two independent facts, together
/// because a pair of bare `bool` arguments at a call site says nothing about which
/// is which.
#[derive(Debug, Clone, Copy)]
struct Reentry {
    /// Lifted off the round ceiling (O52): the same escalation continuing, so the
    /// stuck ladder's rotate/widen budget is not reset.
    from_round_gate: bool,
    /// An amend was applied, so seats that already exist are re-resolved from
    /// the amended config rather than keeping what they were minted with.
    repoint: bool,
    /// The amend moved the implementer pin, so the existing implementer seat
    /// moves too. Any other amend leaves it alone.
    repoint_implementer: bool,
}

/// The implementer seat a config resolves under the pool it dispatches from:
/// the two sides of the pin-change comparison.
fn resolved_implementer(
    cfg: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    health: &dyn crate::providers::HealthSource,
) -> Option<(String, Option<String>)> {
    crate::workflow::roles_resolve::resolve_seat(
        SlotRole::Implementer,
        0,
        0,
        pool,
        pool_origin,
        cfg,
        health,
        &[],
    )
    .map(|(raw, _)| crate::workflow::roles_resolve::split_model(&raw, None))
}

/// The re-point gate: an amend was applied *and* it moved the implementer pin.
/// The pin is compared resolved, not raw: `--role implementer=grok` restated
/// with the seat already there resolves the same and re-points nothing, while
/// a genuinely new pin resolves differently and moves the seat. Any other
/// amend (reviewer restatements, drops, suite) resolves the same pin and leaves
/// the seat, and any stuck-ladder rotation off it, alone.
#[allow(clippy::too_many_arguments)]
fn should_repoint_implementer(
    amend_applied: bool,
    frozen: &Config,
    prev_pool: &[String],
    prev_origin: PoolOrigin,
    amended: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    health: &dyn crate::providers::HealthSource,
) -> bool {
    amend_applied
        && implementer_pin_changed(
            frozen,
            prev_pool,
            prev_origin,
            amended,
            pool,
            pool_origin,
            health,
        )
}

/// Whether the amend moved the implementer pin: the frozen and amended configs
/// resolving it differently under their own pools.
fn implementer_pin_changed(
    frozen: &Config,
    prev_pool: &[String],
    prev_origin: PoolOrigin,
    amended: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    health: &dyn crate::providers::HealthSource,
) -> bool {
    resolved_implementer(frozen, prev_pool, prev_origin, health)
        != resolved_implementer(amended, pool, pool_origin, health)
}

/// Re-resolve the provider and model of seats that already exist, from the amended
/// config (an amend was applied, or `--reload-config` reloaded the file).
///
/// A reviewer config entry pairs with the live seat at its own panel position,
/// never with the id the formula stamps on it: a drop shifts later entries
/// down while slot ids stay stable, so the entry now at config position
/// `r` governs the survivor at live panel position `r`. The provider must agree
/// for the pairing to hold; on a mismatch the entry is skipped, since a
/// replaced panel re-seats rather than re-points. Skipping also keeps one slot
/// from being claimed by two entries: the stamped id of a shifted entry can
/// name a survivor another entry already governs. A seat the new config no
/// longer produces is left alone rather than guessed at: dropping it is
/// `--without`'s job, and this must not become a second path that reaps slots.
///
/// The implementer moves only when the amend changed its pin
/// (`repoint_implementer`): any other amend leaves the seat alone, so a
/// stuck-ladder rotation off the pin survives it. A reviewer seat's id encodes
/// its provider, so the implementer is the only seat this can move.
///
/// **A `Running` seat is never re-pointed.** Its provider is the one that is mid-flight;
/// rewriting it would make the run's own record disagree with the process it is
/// waiting on, and the slot's pid marker is filed against what actually spawned.
fn repoint_existing_seats(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
    repoint_implementer: bool,
) -> Result<()> {
    let pool = state.providers.clone();
    let positions: HashMap<String, usize> = state
        .slots
        .iter()
        .filter(|s| is_panel_reviewer(s))
        .enumerate()
        .map(|(r, s)| (s.id.clone(), r))
        .collect();
    let run_id = state.id.clone();
    // Never touch the seat that is mid-flight: its provider is the one that
    // spawned, and its pid marker is filed against exactly that. A hand-moved
    // seat (O136) is not governed by the config: it never named its provider.
    // Neither is a seat serving its engaged backup: the failover picked the
    // provider deliberately, and the resolved seat names the primary, so
    // re-pointing would undo the failover.
    for slot in state.slots.iter_mut().filter(|s| {
        s.status != SlotStatus::Running
            && s.source != Some(SeatSource::Reassigned)
            && s.source != Some(SeatSource::Backup)
    }) {
        // Resolve the seat this slot fills raw and unsettled: settling here (a
        // backup or order substitution) would resolve a different provider on
        // every continue whose primary went unhealthy, without changing
        // anything the seat machinery owns. Reviewer entries pair positionally;
        // see above. A widened extra holds no panel position and stays with the
        // seat machinery.
        let (role, pool_index, role_index) = match slot.role {
            SlotRole::Implementer if slot.id == "impl" => {
                if !repoint_implementer {
                    continue;
                }
                (SlotRole::Implementer, 0, 0)
            }
            SlotRole::Reviewer => match positions.get(&slot.id) {
                Some(&r) => (SlotRole::Reviewer, r + 1, r),
                None => continue,
            },
            _ => continue,
        };
        let Some((raw, source)) = crate::workflow::roles_resolve::resolve_seat(
            role,
            pool_index,
            role_index,
            &pool,
            state.pool_origin,
            cfg,
            health,
            &[],
        ) else {
            continue;
        };
        let (provider, model) = crate::workflow::roles_resolve::split_model(&raw, None);
        if slot.provider != provider {
            // A reviewer seat's id encodes its provider: writing a new one over
            // it would make the id contradict its own contents, collapsing a
            // deliberate two-vendor panel to one vendor without anybody being
            // told. A different reviewer provider is the re-seat's job, so the
            // pairing is skipped. Only the implementer's id names no provider.
            if role == SlotRole::Reviewer {
                continue;
            }
            // A provider change goes through the same primitive as every
            // other one: the old provider's outcome stays off the seat, a
            // level the new CLI refuses is dropped, and a stale session
            // marker for the new provider is cleared.
            let pin = crate::provider_ref::Pin {
                provider: provider.clone(),
                model,
            };
            let display = pin.display();
            slot.point_at(&display);
            if executor::check_slot_effort(&display, slot.effort).is_err() {
                slot.effort = None;
            }
            slot.source = Some(source);
            if let Some(cli) = crate::provider_ref::ProviderRef::parse(&provider)
                .ok()
                .and_then(|p| p.cli_name().map(str::to_string))
            {
                crate::markers::clear_session_id(paths, &run_id, &slot.id, &cli);
            }
            continue;
        }
        if slot.model == model {
            continue;
        }
        slot.model = model;
        slot.source = Some(source);
    }
    Ok(())
}

/// One `--reassign <role|seat>=<provider[@model]>`, checked before the run is touched.
#[derive(Debug, Clone)]
pub struct Reassignment {
    target: String,
    pin: crate::provider_ref::Pin,
}

/// Parse `--reassign` and refuse a provider the run could not dispatch anyway: unknown,
/// manually paused, or paused on a trusted quota signal (O106, O136). Read before the run
/// lock, so a typo costs nothing. The pause check keys the route the next dispatch will
/// stamp (this process's environment): a model-less codex seat reassigned onto a paused
/// ambient bucket is refused even when the run's stale record would key elsewhere.
pub fn parse_reassignments(
    raw: &[String],
    paths: &SparPaths,
    route: crate::providers::RecordedRoute<'_>,
) -> Result<Vec<Reassignment>> {
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let mut out = Vec::new();
    for r in raw {
        let Some((target, provider)) = r.split_once('=') else {
            bail!("--reassign {r:?}: expected <role|seat>=<provider[@model]>");
        };
        let target = target.trim();
        if target.is_empty() {
            bail!("--reassign {r:?}: name a role (implementer, reviewer, tester) or a seat id");
        }
        let pin = crate::provider_ref::Pin::parse(provider)
            .map_err(|e| anyhow::anyhow!("--reassign {r:?}: {e}"))?;
        let pref = crate::provider_ref::ProviderRef::parse(&pin.provider)?;
        if let Some(cli) = pref.cli_name() {
            if providers::adapter_named(cli).is_none() {
                bail!("--reassign {r:?}: no adapter named {cli}");
            }
        }
        let seat = crate::provider_ref::ProviderRef::for_seat(&pin.provider, pin.model.as_deref());
        let key = seat.quota_key(route);
        if store.blocks_live_dispatch(&key) {
            bail!(
                "--reassign {r:?}: {key} is paused; resume it with `spar provider resume {key}` \
                 or reassign to another provider"
            );
        }
        if out.iter().any(|o: &Reassignment| o.target == target) {
            bail!("--reassign names {target} twice");
        }
        out.push(Reassignment {
            target: target.to_string(),
            pin,
        });
    }
    Ok(out)
}

/// The seat a `--reassign` target names: a role with exactly one seat, or a seat id.
fn reassign_target(state: &RunState, target: &str) -> Result<String> {
    let movable = |s: &&SlotState| {
        matches!(
            s.role,
            SlotRole::Implementer | SlotRole::Reviewer | SlotRole::Tester
        )
    };
    if let Some(s) = state.slots.iter().filter(movable).find(|s| s.id == target) {
        return Ok(s.id.clone());
    }
    let role = match target {
        "implementer" | "impl" => SlotRole::Implementer,
        "reviewer" => SlotRole::Reviewer,
        "tester" => SlotRole::Tester,
        _ => {
            let seats: Vec<&str> = state
                .slots
                .iter()
                .filter(movable)
                .map(|s| s.id.as_str())
                .collect();
            bail!(
                "--reassign {target}: run {} has no such seat (seats: {})",
                state.id,
                if seats.is_empty() {
                    "none yet".to_string()
                } else {
                    seats.join(", ")
                }
            );
        }
    };
    let seats: Vec<&SlotState> = state.slots.iter().filter(|s| s.role == role).collect();
    match seats.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => bail!(
            "--reassign {target}: run {} has no {target} seat yet; `--reassign` moves a seat \
             that exists. For a run that has not dispatched its implement panel, use \
             `--reload-config --role {target}=…`",
            state.id
        ),
        many => bail!(
            "--reassign {target}: run {} has {} {target} seats; name one: {}",
            state.id,
            many.len(),
            many.iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve every `--reassign` against the run before anything moves, so one bad
/// target leaves every seat as it was.
pub(crate) fn resolve_reassignments(
    state: &RunState,
    wanted: &[Reassignment],
) -> Result<Vec<(String, crate::provider_ref::Pin)>> {
    let mut out: Vec<(String, crate::provider_ref::Pin)> = Vec::new();
    for r in wanted {
        let seat = reassign_target(state, &r.target)?;
        if out.iter().any(|(s, _)| *s == seat) {
            bail!("--reassign names seat {seat} twice");
        }
        let slot = state
            .slots
            .iter()
            .find(|s| s.id == seat)
            .ok_or_else(|| anyhow::anyhow!("seat {seat} vanished"))?;
        if slot.provider == r.pin.provider && slot.model == r.pin.model {
            bail!(
                "--reassign {}: seat {seat} is already on {}",
                r.target,
                r.pin.display()
            );
        }
        let new_id = moved_seat_id(slot.role, reviewer_seat_kind(slot), &seat, &r.pin.provider);
        if new_id != seat && state.slots.iter().any(|s| s.id == new_id) {
            bail!(
                "--reassign {}: seat {seat} would become {new_id}, which this run already has",
                r.target
            );
        }
        out.push((seat, r.pin.clone()));
    }
    Ok(out)
}

/// Retire a seat's provider and put another in it (O136, an operator override of O80).
/// The worktree, branch, carry-forward brief, round count and review history stay; a
/// reviewer seat is renamed through `rename_slot` (O85) and its last review artifact
/// follows it, so the blockers it raised still reach the implementer next round. The new
/// provider starts cold: a session marker left under the new seat id by an earlier
/// rotation belongs to a conversation this seat never had.
fn reassign_seat(
    state: &mut RunState,
    paths: &SparPaths,
    seat: &str,
    pin: &crate::provider_ref::Pin,
) -> Result<crate::state::SeatReassignment> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.id == seat)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown seat {seat}"))?;
    let from_provider = crate::provider_ref::Pin {
        provider: slot.provider.clone(),
        model: slot.model.clone(),
    }
    .display();
    let new_id = move_seat(state, paths, seat, pin, SeatSource::Reassigned)?;
    if new_id != seat {
        let from = paths.artifact(&state.id, &format!("review-{seat}.md"));
        if from.is_file() {
            let _ = std::fs::rename(
                &from,
                paths.artifact(&state.id, &format!("review-{new_id}.md")),
            );
        }
    }
    if let Some(cli) = crate::provider_ref::ProviderRef::parse(&pin.provider)
        .ok()
        .and_then(|p| p.cli_name().map(str::to_string))
    {
        crate::markers::clear_session_id(paths, &state.id, &new_id, &cli);
    }
    let record = crate::state::SeatReassignment {
        at: chrono::Utc::now(),
        round: state.round,
        role: slot.role,
        from_seat: seat.to_string(),
        from_provider,
        to_seat: new_id,
        to_provider: pin.display(),
    };
    state.seat_reassignments.push(record.clone());
    let msg = format!(
        "seat {} reassigned by the operator: {} -> {} (now seat {})",
        record.from_seat, record.from_provider, record.to_provider, record.to_seat
    );
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    Ok(record)
}

/// What to say when someone hands us a plan we cannot trace to a run. Naming the
/// candidates is the point: the answer is almost always one of them.
fn unlinked_plan_error(plan: &std::path::Path, paths: &SparPaths) -> String {
    let mut msg = format!(
        "--plan {} does not belong to a run, so spar cannot tell which unit of work this continues.\n\
         A run covers a task from brief to draft PR; implementing its plan is that run continuing (O45).\n",
        plan.display()
    );
    let mut candidates: Vec<crate::state::RunSummary> = crate::state::list_runs(paths)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            matches!(
                r.phase,
                Phase::PlanApproved
                    | Phase::AwaitingPlanApproval
                    | Phase::PlanReady
                    | Phase::Stopped
                    | Phase::Quota
            )
        })
        .collect();
    candidates.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    if candidates.is_empty() {
        msg.push_str(
            "\nNo run here is waiting to be implemented. If this really is new work: --new",
        );
    } else {
        msg.push_str("\nContinue one of these instead:\n");
        for r in candidates.iter().take(5) {
            msg.push_str(&format!(
                "  spar implement --run {}   # {} · {}\n",
                r.id,
                serde_json::to_string(&r.phase)
                    .unwrap_or_default()
                    .trim_matches('"'),
                r.task
                    .as_deref()
                    .unwrap_or("(no task)")
                    .chars()
                    .take(56)
                    .collect::<String>()
            ));
        }
        msg.push_str("\nOr, if this really is a new unit of work: --new");
    }
    msg
}

pub fn run_loop(opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let task = opts
        .task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--task required for loop workflow"))?;
    run_with_task(task, None, opts, paths, cfg, None)
}

/// Whether any slot dispatch started in the run's current round. Dispatch stamps
/// `dispatched_round` and saves before it spawns, so a round it never reached spent
/// nothing.
pub fn current_round_dispatched(state: &RunState) -> bool {
    state.dispatched_round >= state.round
}

fn run_from_approved(
    run_id: &str,
    amendment: Option<String>,
    mut opts: CommonOpts,
    paths: &SparPaths,
    amend: BoundAmendInputs,
) -> Result<ExitCode> {
    // Loaded once before the lock only so a mistyped id fails without the lock minting
    // a run directory for it. Everything below mutates the run, so it all happens under
    // the lock: a live orchestrator refuses us before its state.json can be overwritten.
    let unlocked = RunState::load(paths, run_id)?;
    // Key the route this process's dispatch will stamp (continuing entries
    // stamp on resumption), not the previous record: a stale record would
    // accept a bucket the dispatch now bills, or refuse one it no longer does.
    let reassignments = parse_reassignments(
        &opts.reassign,
        paths,
        crate::providers::codex::dispatch_route().as_recorded(),
    )?;
    let mut patient = None;
    if let Some(mr) = opts.max_rounds {
        let target = mr.resolve(unlocked.round);
        check_round_extension(&unlocked, target, !current_round_dispatched(&unlocked))?;
        let extension_only = amendment.is_none()
            && opts.providers.is_empty()
            && opts.select.is_empty()
            && opts.base.is_none()
            && !opts.reload_config
            && !opts.accept_contract
            && !opts.re_review
            && opts.reassign.is_empty()
            && amend.is_noop();
        let live = crate::runlock::RunLock::owner(paths, run_id).filter(|o| o.alive());
        if let (true, Some(owner)) = (extension_only, live) {
            if let Some(code) =
                queue_round_extension(paths, &unlocked, target, owner.pid, opts.json)?
            {
                return Ok(code);
            }
            patient = Some((target, owner.pid));
        }
    }
    let lock = match patient {
        None => crate::runlock::RunLock::acquire(paths, run_id)?,
        Some((target, owner_pid)) => {
            match acquire_for_extension(paths, run_id, target, owner_pid, opts.json)? {
                Ok(lock) => lock,
                Err(code) => return Ok(code),
            }
        }
    };
    // The live file config loads before the run's captured env is re-applied:
    // `Config::load` fills `captured` from this process, and the re-apply below
    // overwrites the process env with the creation values (O130), so loading
    // after it would freeze the creation values as this shell's.
    let live = if opts.reload_config {
        Some(Config::load(&paths.project_root)?)
    } else {
        None
    };
    // Every way back into a run (`--run`, `resume`, a `--plan` the run wrote) dispatches
    // under the environment the run was created in (O130), from its own snapshot.
    // `cfg` was read before the lock for the cheap pre-checks in `implement_ctx`;
    // re-read the frozen snapshot under the lock: another dispatch may have amended
    // it since, and the preview below builds from the frozen base (O27), so the
    // stale read would silently overwrite the newer snapshot.
    let frozen = Config::for_run(paths, run_id)?;
    frozen.reapply_captured_env();
    let cfg = &frozen;
    let mut state = RunState::load(paths, run_id)?;
    // Read before anything below resets slot statuses for re-dispatch.
    let round_dispatched = current_round_dispatched(&state);
    let resumable = crate::state::is_resumable(&state);
    if !resumable {
        let hint = if state.phase.is_waitable_stop() {
            String::new()
        } else {
            format!("; an in-flight run nobody is driving is picked up with `spar resume {run_id}`")
        };
        bail!(
            "run {run_id} is not resumable (workflow={:?}, phase={:?}){hint}",
            state.workflow,
            state.phase
        );
    }
    if let Some(mr) = opts.max_rounds {
        check_round_extension(&state, mr.resolve(state.round), !round_dispatched)?;
    }
    let moves = resolve_reassignments(&state, &reassignments)
        .map_err(|e| ReassignRefused(format!("{e:#}")))?;
    // The amend previews against the post-move state: the reassigns below apply
    // after the preconditions, and the amend applies after them, so ordinals,
    // pins and the re-seat simulation must read the seats the dispatch will
    // actually amend. Simulating on a clone keeps the preview read-only; a bad
    // move fails here, before any side effect below.
    let moved = simulate_moves(&state, &moves).map_err(|e| ReassignRefused(format!("{e:#}")))?;
    // The amend previews here, read-only, before any side effect below (the
    // stopped-marker removal, the terminal-slot clearing, the base worktrees,
    // the reassigns). A refused amend (a bad seat id, a malformed override, the
    // last-seat drop) changes nothing on disk or in the saved state. It is
    // applied after the preconditions, further down.
    let preview = if live.is_none() && amend.is_noop() {
        None
    } else {
        Some(
            preview_bound_amend(cfg, live.as_ref(), &moved, &amend)
                .map_err(|e| AmendRefused(format!("{e:#}")))?,
        )
    };
    // Resuming a stopped run: drop the marker so execute_loop dispatches instead
    // of halting again at its first boundary.
    if state.phase == Phase::Stopped {
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
    }
    // Resuming a terminal run (a failed/stuck/quota-parked attempt): clear the dead
    // verdict so the detach snapshot reads as a fresh re-dispatch, not `failed` with
    // `exit_code: 1` (which reads as a refusal). This is purely for snapshot coherence —
    // `execute_loop` re-dispatches the coding slots regardless of their incoming status;
    // here we just make the persisted state it briefly emits honest. Per-slot terminal
    // markers are cleared at dispatch by the executor. A still-paused provider is caught
    // again by the `ensure_usable` gate below, which re-parks at `Quota` with a fresh
    // error rather than silently retrying into the same wall.
    if matches!(state.phase, Phase::Failed | Phase::Stuck | Phase::Quota) {
        for s in &mut state.slots {
            if matches!(s.status, SlotStatus::Failed | SlotStatus::Stuck) {
                s.status = SlotStatus::Pending;
                s.error = None;
                s.exit_code = None;
                s.signal = None;
                s.pid = None;
                s.quota_hit = false;
            }
        }
        state.error = None;
        state.set_phase(Phase::PrepareIsolation);
    }
    // Resuming a run parked by a conflicted pre-ship base merge: the remedy
    // round supersedes the park, so drop its flag and error now. A stale park
    // error would otherwise outlive the resolution and misdirect the next gate
    // (the same hazard the round-gate lift below guards against, #229).
    if state.gates.ship_parked {
        state.gates.ship_parked = false;
        if state.phase == Phase::AwaitingShipConfirm {
            state.error = None;
        }
    }
    // `-t` on an approved run is a directive for THIS round only. It never rewrites
    // the run's task; absent `-t`, any prior amendment is cleared so it never silently
    // re-applies to a later round.
    state.amendment = amendment;
    if !opts.json {
        match &state.amendment {
            Some(a) => println!("amendment applied for this round: {a}"),
            None => println!("no amendment (running the original task)"),
        }
    }
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = opts.resolve_dry_run();
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    // A resumed run keeps the base its plan phase was cut from. `--base` may only
    // re-point a run with no worktrees yet: the plan phase's test-author worktree is
    // reused as-is and its whole tree is overlaid onto the implementer, so a run
    // straddling two bases produces a branch parented on the new base carrying the old
    // base's file contents — a diff that reads as reverting the difference.
    if opts.base.is_some() {
        let previous = state.base_commit.clone();
        worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
        if !state.worktrees.is_empty() && previous != state.base_commit {
            bail!(
                "run {run_id} is already based on {}; a run's base is fixed when it is \
                 created — plan a new run with `--base` instead",
                previous.as_deref().unwrap_or("an unrecorded base")
            );
        }
    }
    for (seat, pin) in &moves {
        let r = reassign_seat(&mut state, paths, seat, pin)?;
        if !opts.json {
            println!(
                "reassigned seat {} ({}) to {}{}",
                r.from_seat,
                r.from_provider,
                r.to_provider,
                if r.to_seat == r.from_seat {
                    String::new()
                } else {
                    format!(", now seat {}", r.to_seat)
                }
            );
        }
    }
    // Bound amend, applied under this same lock and after every launch
    // precondition passed (resumable, round extension, reassign resolution,
    // base): the frozen config is amended, reviewer seats are dropped with
    // their slots, a `--role reviewer=` pin re-seats the live panel to match
    // it, and the snapshot is re-frozen, so a refused launch changes nothing.
    // The amended config shadows the frozen one for the rest of this dispatch.
    // The `preview` above already passed every check, so a refusal here means
    // the surgery itself failed mid-apply.
    let amend_applied = preview.is_some();
    let amended;
    let cfg: &Config = if let Some(mut preview) = preview {
        if preview.panel_pinned {
            // The preview already pinned post-move seats, but the pin is rebuilt
            // here from the live state anyway: the re-seat and the config it
            // freezes must read the same panel, never a stale copy.
            let doomed: BTreeSet<String> = plan_reviewer_drop(&state, &preview.dropped)
                .map_err(|e| AmendRefused(format!("{e:#}")))?
                .map(|plan| plan.removed)
                .unwrap_or_default()
                .into_iter()
                .collect();
            preview.cfg.roles.reviewer = state
                .slots
                .iter()
                .filter(|s| is_panel_reviewer(s) && !doomed.contains(&s.id))
                .map(|s| s.provider_ref().display())
                .collect();
        }
        // The seats retire in state first; their files go only after both
        // saves below land, so neither a failed save nor a failed deletion
        // leaves the config naming seats whose checkouts are gone.
        let mut removed = if !preview.dropped.is_empty() {
            let removed = retire_reviewer_seats(&mut state, &preview.dropped)
                .map_err(|e| AmendRefused(format!("{e:#}")))?;
            disengage_spent_reviewer_backups(&state, &mut preview.cfg.backups.reviewer);
            removed
        } else {
            Vec::new()
        };
        // A `--role reviewer=` pin replaces the panel outright: the live seats
        // are moved or minted to match it, so the live panel and the frozen
        // config panel stay one fact. A drop-all plus pin re-seats the panel
        // instead of reaching the ship gate with no reviewer. The same
        // predicate the preview simulated with, so a first dispatch mints the
        // panel once from the amended config instead of twice.
        if bound_panel_resync_applies(&state, &amend.role, preview.cfg.roles.reviewer.len()) {
            let pin = parse_reviewer_pin(&preview.cfg.roles.reviewer)
                .map_err(|e| AmendRefused(format!("{e:#}")))?;
            let resynced =
                resync_reviewer_panel_to_pin(&mut state, &mut preview.cfg.backups.reviewer, &pin)
                    .map_err(|e| AmendRefused(format!("{e:#}")))?;
            removed.extend(resynced);
        }
        if !removed.is_empty() {
            let _ = crate::events::append(
                paths,
                run_id,
                &crate::events::Event::info(format!(
                    "operator dropped reviewer seat(s) {} on re-entry",
                    removed.join(", ")
                )),
            );
            eprintln!(
                "run {run_id}: dropped reviewer seat(s) {} (the amended config re-freezes this dispatch)",
                removed.join(", ")
            );
        }
        save_amended_pair(paths, run_id, &preview.cfg, &state)
            .map_err(|e| AmendRefused(format!("{e:#}")))?;
        if !removed.is_empty() {
            if let Err(e) = delete_dropped_reviewer_files(&state, paths, &removed) {
                eprintln!("run {run_id}: dropped seats retired but their files could not be deleted: {e:#}");
            }
        }
        preview.cfg.reapply_captured_env();
        amended = preview;
        state.isolation = amended.cfg.isolation;
        state.autonomy = amended.cfg.autonomy;
        state.message_budget = amended.cfg.message_budget;
        &amended.cfg
    } else {
        cfg
    };
    if state.dry_run {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    // A bare `implement --run <id>` (no flags at all) is a continuation, not a fresh
    // request. Reuse the run's frozen pool only when it was a genuine operator override
    // (`--providers`/`--select`) — `pool_intent` when the plan phase captured one wider
    // than its own slot count needed, else `providers`. When the frozen pool was itself
    // synthesized from `[roles]`/`[providers].order` (`PoolOrigin::Synth`), there is
    // nothing of the operator's to preserve, so this resolves fresh from `cfg` instead —
    // the only way a `--reload-config` continuation can pick up an edited `[roles]`.
    // Reusing unconditionally (ignoring `pool_origin`) would silently drop an earlier
    // `--providers`/`--select` the moment any `[roles]` entry (e.g. a CLI-pinned
    // reviewer) also happens to be frozen — the opposite bug.
    let is_explicit_frozen_pool = matches!(
        state.pool_origin,
        PoolOrigin::CliProviders | PoolOrigin::Selected
    );
    let frozen_pool: Vec<String> = if !state.pool_intent.is_empty() {
        state.pool_intent.clone()
    } else {
        state.providers.clone()
    };
    let reuses_frozen_pool = opts.providers.is_empty()
        && opts.select.is_empty()
        && is_explicit_frozen_pool
        && !frozen_pool.is_empty();
    let (n, roles) = crate::workflow::roles_resolve::reviewer_select_question(cfg);
    if reuses_frozen_pool {
        // The frozen pool may be narrower than this round's panel — e.g. a plan phase
        // that ran with `--without critic,spec` only ever needed one provider. Refusing
        // here regressed the documented `plan -> approve -> implement --run` flow itself
        // (any plan pool narrower than the implement panel broke its own continuation);
        // `pick_providers` below already cycles a short explicit pool to fill `n`, the
        // same fallback an unpinned panel already relies on, so let it.
        opts.providers = frozen_pool.clone();
    }
    let requested = opts.resolve_pool(n, &roles, paths, cfg, &state.id)?;
    // `opts.providers` was just filled from the frozen pool above when reusing a
    // continuation's pool — that is not a fresh CLI `--providers`, so the source it
    // reports has to stay whatever the run already froze (`state.pool_origin`), not get
    // relabelled `cli-providers` for having reached the resolver as a non-empty pool.
    let pool_origin = if reuses_frozen_pool {
        state.pool_origin
    } else {
        crate::workflow::roles_resolve::pool_origin_for(&opts)
    };
    let prev_pool = state.providers.clone();
    let prev_origin = state.pool_origin;
    state.pool_origin = pool_origin;
    // A replacement pool this round (an explicit `--providers`/`--select` on the
    // continuation) becomes the new frozen intent for any later round; a reused one
    // needs no change, and a fresh non-explicit resolution has no intent to freeze.
    if !reuses_frozen_pool {
        state.pool_intent =
            if matches!(pool_origin, PoolOrigin::CliProviders | PoolOrigin::Selected) {
                requested.clone()
            } else {
                Vec::new()
            };
    }
    let health = providers::LiveHealth::for_dry(state.dry_run);
    state.providers = providers::pick_providers(
        &requested,
        n,
        Some(&requested),
        state.dry_run,
        state.pool_origin,
        &health,
    );
    // The implementer re-points only when the amend moved its pin: the frozen
    // and amended configs resolving it differently under their own pools. Any
    // other amend leaves the seat alone, so a stuck-ladder rotation off the pin
    // survives it.
    let repoint_implementer = should_repoint_implementer(
        amend_applied,
        &frozen,
        &prev_pool,
        prev_origin,
        cfg,
        &state.providers,
        state.pool_origin,
        &health,
    );
    // Quota gate is now backup-aware; it runs after seats are materialized.
    // A bare pool gate before prepare_implement_slots would see the pool entry as paused
    // and park, even though the seat's CLI pin would be covered by its backup.
    let from_round_gate = state.phase == Phase::AwaitingRoundExtension;
    // An extension queued for an orchestrator that exited before its ceiling check
    // still counts; an explicit flag on this re-entry outranks it.
    apply_queued_round_extension(&mut state, paths);
    if let Some(mr) = opts.max_rounds {
        let target = mr.resolve(state.round);
        // A prior implement entry minted implementer slots; a first entry after
        // plan approval has only planner slots, so the flag still sets the
        // ceiling the way a fresh launch does.
        let implement_entered = state.slots.iter().any(|s| s.role == SlotRole::Implementer);
        let kept = gate_max_rounds(state.max_rounds, target, implement_entered);
        if kept != target && !opts.json {
            if state.max_rounds == 0 {
                println!("note: --max-rounds {target} ignored: the run has no round ceiling");
            } else {
                println!(
                    "note: --max-rounds {target} is at or under the current ceiling {}; keeping it",
                    state.max_rounds
                );
            }
        }
        state.max_rounds = kept;
    }
    // A round that never dispatched is still the current round, not a spent one: an
    // orchestrator killed between opening it and spawning anything (a launching shell
    // that exited, a quota park before dispatch) cost nothing, so re-entry reuses it,
    // and reusing it is not a step past the ceiling.
    let reuse_round = state.round > 0 && !round_dispatched;
    if !reuse_round && state.round_ceiling_reached() && escalate_round_ceiling(&mut state, paths)? {
        finish_out(&state, opts.json)?;
        return Ok(state.exit_code());
    }
    // The ceiling has been lifted: drop the verdict that parked the run, or a run that
    // goes on to reach the ship gate still reports "round ceiling reached" as its
    // reason, and an outer agent reading `error` at exit 2 re-issues `--max-rounds`
    // instead of shipping.
    if from_round_gate {
        state.error = None;
        // A stale stall count must not survive the gate: a lift grants fresh
        // rounds, so the next stall warns again instead of parking on a count
        // from before the lift. Clamped to one, not zero, so a re-entry into a
        // still-unchanged tree costs one more implementer dispatch and parks
        // again with no panel.
        state.no_progress_rounds = state.no_progress_rounds.min(1);
    }
    let round = if reuse_round {
        state.round
    } else {
        state.begin_round()
    };
    // A re-entered round number is not a reused dispatch: this entry is about to
    // read pauses and spawn, so the pre-existing/this-round line starts now.
    // A fix pass below opens rounds without re-stamping: it continues this
    // dispatch, and its seats must not trip over this dispatch's own pauses.
    state.stamp_dispatch();
    if !opts.json {
        println!("run {run_id} · round {round}");
    }
    let dry = state.dry_run;
    prepare_implement_slots(
        &mut state,
        Some(&requested),
        pool_origin,
        dry,
        cfg,
        paths,
        Reentry {
            from_round_gate,
            repoint: amend_applied,
            repoint_implementer,
        },
    )?;
    // Only the implementer must run next at run start, so only it can park the
    // run here: a paused reviewer or tester seat is reseated onto its backup
    // or stood down at its own dispatch.
    if !dry {
        if let Some(msg) = gate_implementer_seat(&mut state, paths, cfg)? {
            state.error = Some(msg.clone());
            state.set_phase(crate::state::Phase::Quota);
            state.save(paths)?;
            // The round already bumped above: refresh so poll files drop what the
            // new round ages out even though no slot dispatches.
            crate::providers::delivery::refresh_run_poll_files(paths, run_id);
            if opts.json {
                crate::executor::emit_run_json(&state)?;
            } else {
                eprintln!("error: {msg}");
            }
            return Ok(crate::exit_codes::ExitCode::Quota);
        }
    }
    if state.slots.iter().all(|s| s.role != SlotRole::Implementer) {
        bail!("no implementer slot after provider pick");
    }
    // Readiness: a run with no live reviewer seats must never reach the ship
    // confirm through the loop below, so it fails here, before any dispatch,
    // instead of shipping unreviewed.
    if !state.slots.iter().any(is_panel_reviewer) {
        // `fail` returns before any `--json` output for a refused launch; a
        // run that reaches this gate is already failed, so record that first
        // and let the emitted run JSON say so, like the quota gate above.
        let err = anyhow::anyhow!(
            "run {run_id} has no reviewer seats; the run would reach the ship gate unreviewed"
        );
        state.set_phase(crate::state::Phase::Failed);
        state.error = Some(err.to_string());
        state.save(paths)?;
        if opts.json {
            crate::executor::emit_run_json(&state)?;
        }
        return Err(err);
    }
    state.record_unhealthy_pin_warnings();
    // `--re-review` arms the reviewer-is-wrong exit (O180) here, at loop entry:
    // assigning (not OR-ing) keeps a stale arm from an entry that never reached
    // the loop from leaking into a later resume, which carries no flag of its
    // own. The quota and ceiling parks above return before this point, so that
    // entry's flag dies with it; detach carries it, since the detached child is
    // the same entry continuing.
    state.panel_rerun = opts.re_review;
    state.save(paths)?;
    crate::providers::delivery::refresh_run_poll_files(paths, run_id);
    if opts.detach {
        // The detached child takes the lock itself, and flock refuses a second holder.
        drop(lock);
        return detach_implement(&state, paths, opts.json);
    }
    execute_loop(&mut state, paths, cfg, opts.accept_contract)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

fn prepare_implement_slots(
    state: &mut RunState,
    requested: Option<&[String]>,
    pool_origin: PoolOrigin,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
    reentry: Reentry,
) -> Result<()> {
    let Reentry {
        from_round_gate: resuming_from_round_gate,
        repoint,
        repoint_implementer,
    } = reentry;
    state.workflow = crate::cli::WorkflowKind::Loop;
    state.max_fix_rounds = 3;
    state.child_run = None;
    // A run lifted off the round ceiling is the *same* escalation continuing: the
    // operator bought rounds, not a fresh stuck ladder. Resetting these here made
    // `stuck` unreachable in a lift loop, because every lift handed the run another
    // rotate + widen it had already spent (O52).
    if !resuming_from_round_gate {
        state.fix_rounds = 0;
        state.rotated_implementer = false;
        state.widened_reviewers = false;
    }

    // Keep planner slots as historical; add impl/review if missing.
    let has_impl = state.slots.iter().any(|s| s.role == SlotRole::Implementer);
    if has_impl {
        // Seats minted by an earlier round are re-resolved from the amended
        // config whenever an amend was applied; only an amend that moved the
        // implementer pin moves the implementer seat, so a stuck-ladder
        // rotation off the pin survives any other amend.
        if repoint {
            repoint_existing_seats(
                state,
                paths,
                cfg,
                &crate::providers::LiveHealth::for_dry(dry),
                repoint_implementer,
            )?;
        }
        ensure_suite_slot(state, dry, cfg, paths)?;
        return Ok(());
    }

    let req = requested.unwrap_or_default();
    let n = crate::workflow::roles_resolve::pool_width(cfg);
    let health = providers::LiveHealth::for_dry(dry);
    state.providers = providers::pick_providers(req, n, Some(req), dry, state.pool_origin, &health);
    if state.providers.is_empty() {
        // A pool emptied by failed readiness probes fails naming them (exit 1):
        // there is nothing paused, so the quota-only exit 4 must not fire.
        let skips: Vec<(String, String)> = req
            .iter()
            .filter_map(|p| health.check(p).probe_message.map(|m| (p.clone(), m)))
            .collect();
        if let Some(err) = crate::backup::readiness_error(&skips) {
            return Err(err);
        }
        bail!("no usable providers (pass --providers, set a [roles] block, or --select)");
    }
    let pool = state.providers.clone();

    // Apply model-select choices onto slots when artifact exists.
    let art = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten();
    let model_for = |idx: usize| -> Option<String> {
        art.as_ref().and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.slot == idx)
                .and_then(|c| c.model.clone())
        })
    };

    // One resolver for slot creation and for the plan gate's projection of this same
    // panel (feature 011, item C): the id formula cannot drift between the two. Every
    // seat settles through the single resolution point, so an unhealthy non-pin
    // provider yields to its backup or the order rung here exactly as the projection
    // shows; when nothing healthy remains the builder fails with a readiness error
    // (exit 1, never the quota-only exit 4).
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let seats = crate::workflow::roles_resolve::build_implement_seats(
        cfg,
        &pool,
        pool_origin,
        false,
        &health,
        &store,
        paths,
        dry,
        state.dry_run,
        state.recorded_route(),
        &model_for,
    )?;
    for seat in seats {
        let mut slot = executor::init_slot_model(&seat.seat, &seat.provider, seat.role, seat.model);
        slot.source = Some(seat.source);
        if seat.role == SlotRole::Reviewer {
            slot.reviewer_kind = Some(ReviewerSeatKind::Panel);
        }
        state.slots.push(slot);
    }
    // The projected panel has graduated into real slots; a stale projection would only
    // risk drifting from what was actually just resolved above.
    state.projected_fleet.clear();
    ensure_suite_slot(state, dry, cfg, paths)?;
    Ok(())
}

/// Kill and drop every tester slot: the pid marker is filed under the slot id, so it
/// must be reaped before the slot record disappears or a still-running agent becomes an
/// orphan nothing can find (O28).
fn reap_tester_slots(state: &mut RunState, paths: &SparPaths) {
    for slot in state.slots.iter().filter(|s| s.role == SlotRole::Tester) {
        if let Some(token) = crate::markers::read_pid(paths, &state.id, &slot.id) {
            if token.alive() {
                crate::process::terminate_tree(token.pid, true);
            }
        }
        crate::markers::clear_pid(paths, &state.id, &slot.id);
    }
    state.slots.retain(|s| s.role != SlotRole::Tester);
}

/// Ensure a tester slot exists when suite is enabled. Fail closed if no provider.
fn ensure_suite_slot(
    state: &mut RunState,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
) -> Result<()> {
    if cfg.suite.is_builtin() {
        // A run that had an agent tester before `[suite].command` was added (or before a
        // `--reload-config`) carries a slot nothing will ever dispatch again. Reap before
        // dropping: the slot id is the only key its pid marker is filed under, so
        // removing the record first is how a still-running agent becomes an orphan
        // nothing can find (O28). Keeping the slot instead is worse — nothing in the
        // built-in round settles it, so it reads `Running` forever, trips the stall
        // check every round, and goes on writing the same `suite.md` spar writes.
        // Killing it is this command's call: it holds the run lock, the slot is its own,
        // and the channel that slot belonged to no longer exists.
        reap_tester_slots(state, paths);
        return Ok(());
    }
    if !cfg.suite.enabled {
        // `--without suite` on a run that already has a live tester slot (a resumed
        // round) must not leave it in place for the next dispatch to pick back up —
        // same reap as the built-in-suite branch above, for the same reason (O28).
        reap_tester_slots(state, paths);
        return Ok(());
    }
    if state.slots.iter().any(|s| s.role == SlotRole::Tester) {
        return Ok(());
    }
    let used: Vec<String> = state.slots.iter().map(|s| s.provider.clone()).collect();
    let (suite_prov, suite_model, source) = resolve_suite_provider(
        cfg,
        dry,
        &crate::providers::LiveHealth::for_dry(dry),
        &state.providers,
        state.pool_origin,
        Some(paths),
        Some(&state.id),
        &used,
    )?;
    let mut slot = executor::init_slot_model(
        format!("suite-{}", sanitize_slot(&suite_prov)),
        &suite_prov,
        SlotRole::Tester,
        suite_model,
    );
    slot.source = Some(source);
    state.slots.push(slot);
    Ok(())
}

/// The tester seat a plan gate has not dispatched yet, projected the same way the
/// implementer/reviewer panel is (feature 011, item C). A built-in `[suite].command` or
/// `--without suite` never spawns an agent seat, so neither projects one — the plan
/// gate's `fleet` array must not promise a paid seat the run will never create.
///
/// `paths`/`run_id`, when given, are read-only: if a `tester`-role choice is already
/// recorded in `model-select.json` (e.g. from an earlier `--select`), the projection
/// reports that exact provider/model/`ModelSelect` source, matching what
/// `ensure_suite_slot`'s own artifact lookup will find at dispatch. Never calls
/// `pick_one_for_role` / `write_select_artifact` here — a projection must not choose a
/// model or mutate the artifact as a side effect of showing a gate, so a run with no
/// prior tester choice still reports the same `SuitePreferences` guess dispatch would
/// fall back to, honestly labelled as not yet decided.
#[allow(clippy::too_many_arguments)]
pub fn project_tester_seat(
    cfg: &Config,
    dry: bool,
    health: &dyn crate::providers::HealthSource,
    pool: &[String],
    pool_origin: PoolOrigin,
    paths: Option<&SparPaths>,
    run_id: Option<&str>,
    used: &[String],
) -> Option<FleetSeat> {
    if cfg.suite.is_builtin() || !cfg.suite.enabled {
        return None;
    }
    if let (Some(paths), Some(run_id)) = (paths, run_id) {
        if let Ok(Some(art)) = crate::model_select::load_select_artifact(paths, run_id) {
            if let Some(c) = art
                .choices
                .iter()
                .find(|c| c.role.as_deref() == Some("tester"))
            {
                return Some(FleetSeat {
                    seat: format!("suite-{}", sanitize_slot(&c.provider)),
                    role: SlotRole::Tester,
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    source: SeatSource::ModelSelect,
                    projected: true,
                });
            }
        }
    }
    let (provider, model, source) =
        resolve_suite_provider(cfg, dry, health, pool, pool_origin, None, None, used).ok()?;
    Some(FleetSeat {
        seat: format!("suite-{}", sanitize_slot(&provider)),
        role: SlotRole::Tester,
        provider,
        model,
        source,
        projected: true,
    })
}

/// Cheap suite-channel provider: config override, model-select (tester/fast), prefs, fleet.
/// Returns the provider, an optional explicit model, and the honest source of the pick:
/// the fleet fallback is `state.providers`, so its source follows `pool_origin` rather
/// than an unconditional `ProvidersOrder`.
#[allow(clippy::too_many_arguments)]
fn resolve_suite_provider(
    cfg: &Config,
    dry: bool,
    health: &dyn crate::providers::HealthSource,
    fleet: &[String],
    pool_origin: PoolOrigin,
    paths: Option<&SparPaths>,
    run_id: Option<&str>,
    used: &[String],
) -> Result<(String, Option<String>, SeatSource)> {
    if let Some(p) = &cfg.roles.tester {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid [roles].tester {p:?}: {e}"))?;
        let pinned = cfg.cli_role_keys.contains(&SlotRole::Tester);
        if dry
            || (if pinned {
                providers::is_provider_present(p, false)
            } else {
                health.check(p).usable
            })
        {
            let source = if pinned {
                SeatSource::CliRole
            } else {
                SeatSource::RolesFile
            };
            return Ok((p.clone(), None, source));
        }
        // Fall through to model-select / prefs / fleet if the override is unusable
        // (missing CLI / paused). A `--role` pin falls through only when the binary
        // is missing; probe-failed pins still dispatch, with a launch warning.
    }
    // Prefer model-select artifact / fresh pick with tester role (fast profile).
    if let (Some(paths), Some(run_id)) = (paths, run_id) {
        if let Ok(Some(art)) = crate::model_select::load_select_artifact(paths, run_id) {
            if let Some(c) = art
                .choices
                .iter()
                .find(|c| c.role.as_deref() == Some("tester"))
            {
                return Ok((c.provider.clone(), c.model.clone(), SeatSource::ModelSelect));
            }
            let exclude: Vec<String> = art.choices.iter().map(|c| c.vals_id.clone()).collect();
            let urgency = crate::model_select::Urgency::parse(&art.urgency)
                .unwrap_or(crate::model_select::Urgency::Normal);
            if let Ok(c) =
                crate::model_select::pick_one_for_role("tester", urgency, cfg, dry, &exclude)
            {
                // Append to artifact for audit trail.
                let mut art = art;
                let mut c = c;
                c.slot = art.choices.len();
                art.choices.push(c.clone());
                let _ = crate::model_select::write_select_artifact(paths, run_id, &art);
                return Ok((c.provider, c.model, SeatSource::ModelSelect));
            }
        }
    }
    const PREFS: &[&str] = &["cli:claude", "cli:grok", "cli:agy", "api:xai", "api:openai"];
    if dry {
        return Ok((PREFS[0].into(), None, SeatSource::SuitePreferences));
    }
    // Parallel singletons spread: prefer a usable entry no seated slot holds,
    // collapsing onto the first usable one only when all are seated.
    let usable_prefs: Vec<String> = PREFS
        .iter()
        .filter(|p| health.check(p).usable)
        .map(|s| (*s).to_string())
        .collect();
    if let Some(p) = usable_prefs
        .iter()
        .find(|p| !crate::workflow::roles_resolve::seated(used, p))
        .or_else(|| usable_prefs.first())
    {
        return Ok((p.clone(), None, SeatSource::SuitePreferences));
    }
    // A `--providers` pool is pinned member-by-member; every other pool stays a
    // constrained auto-pick, like the spec resolver's explicit pool. Only the
    // auto-pick spreads across unseated providers; a pinned pool keeps its
    // positional first pick exactly as before.
    let pool_pinned = matches!(pool_origin, PoolOrigin::CliProviders);
    let usable_fleet: Vec<String> = fleet
        .iter()
        .filter(|p| {
            if pool_pinned {
                providers::is_provider_present(p, false)
            } else {
                health.check(p).usable
            }
        })
        .cloned()
        .collect();
    if let Some(p) = usable_fleet
        .iter()
        .find(|p| pool_pinned || !crate::workflow::roles_resolve::seated(used, p))
        .or_else(|| usable_fleet.first())
    {
        return Ok((p.clone(), None, pool_origin.as_seat_source()));
    }
    let probes = PREFS
        .iter()
        .filter_map(|p| {
            let verdict = health.check(p);
            (!verdict.usable).then(|| {
                verdict
                    .probe_message
                    .unwrap_or_else(|| format!("{p} unavailable"))
            })
        })
        .collect::<Vec<_>>()
        .join("; ");
    bail!("suite.enabled but no usable suite provider ({probes}; set [roles].tester or install a CLI)")
}

#[cfg(test)]
mod round_reuse_tests {
    use super::*;

    fn run(workflow: crate::cli::WorkflowKind, phase: Phase) -> RunState {
        let mut st = RunState::new("r1", workflow, std::path::PathBuf::from("/x"));
        st.phase = phase;
        st
    }

    /// #179: only a dispatch that started in the current round charges it. A slot put
    /// back to `Pending` by a backup failover after it really ran still counts.
    #[test]
    fn a_round_counts_as_dispatched_only_once_a_dispatch_started_in_it() {
        let mut st = run(crate::cli::WorkflowKind::Loop, Phase::Stopped);
        st.round = 1;
        st.slots.push(executor::init_slot(
            "impl",
            "cli:claude",
            SlotRole::Implementer,
        ));
        assert!(!current_round_dispatched(&st), "created, never dispatched");
        st.dispatched_round = 1;
        assert!(
            current_round_dispatched(&st),
            "dispatched, then failed over to Pending"
        );
        st.round = 2;
        assert!(
            !current_round_dispatched(&st),
            "round 2 opened, nothing ran in it"
        );
    }
}

#[cfg(test)]
mod suite_reap_tests {
    use super::*;

    fn run_with_impl(provider: &str, model: &str, status: SlotStatus) -> RunState {
        let mut st = RunState::new(
            "repoint1",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        let mut slot = executor::init_slot_model(
            "impl",
            provider,
            SlotRole::Implementer,
            Some(model.to_string()),
        );
        slot.status = status;
        st.slots.push(slot);
        st.providers = vec![provider.to_string()];
        st
    }

    fn cfg_with_implementer(pin: &str) -> Config {
        let mut cfg = Config::default();
        cfg.roles.implementer = Some(pin.to_string());
        cfg
    }

    /// `--reload-config` is documented in `skills/core.md` as the way to change a
    /// running run's fleet. Before this, it reached `config.json` and stopped:
    /// `prepare_implement_slots` early-returned on `has_impl`, so a seat minted by an
    /// earlier round kept its original provider and the re-dispatch used it. That is
    /// how a bad model pin survived three separate corrections and had to be fixed by
    /// editing `state.json` by hand.
    #[test]
    fn reload_config_repoints_a_seat_that_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Failed);
        let cfg = cfg_with_implementer("cli:muse@muse-spark-1.2");

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            true,
        )
        .unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(impl_slot.model.as_deref(), Some("muse-spark-1.2"));
    }

    /// A reviewer-model-only amend must still re-point reviewer seats: the flag
    /// that decides whether the implementer moves must not also decide whether
    /// `repoint_existing_seats` runs at all. This passes exactly what the dispatch
    /// passes when the amend did not move the pin, and failed while the call was
    /// gated on the pin flag: the reviewer kept its stale model.
    #[test]
    fn an_amend_without_an_implementer_pin_move_still_repoints_reviewer_models() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Failed);
        let mut review_slot = executor::init_slot_model(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
            Some("opus".into()),
        );
        review_slot.status = SlotStatus::Failed;
        st.slots.push(review_slot);
        st.providers = vec!["cli:muse".into(), "cli:claude".into()];

        let mut cfg = cfg_with_implementer("cli:muse@muse-spark-1.3");
        cfg.roles.reviewer = vec!["cli:claude@sonnet".into()];

        prepare_implement_slots(
            &mut st,
            None,
            PoolOrigin::Synth,
            true,
            &cfg,
            &paths,
            Reentry {
                from_round_gate: false,
                repoint: true,
                repoint_implementer: false,
            },
        )
        .unwrap();

        let review = st
            .slots
            .iter()
            .find(|s| s.id == "review-0-cli-claude")
            .unwrap();
        assert_eq!(
            review.model.as_deref(),
            Some("sonnet"),
            "reviewer model follows the amend even when the implementer pin did not move"
        );
    }

    /// A drop shifts later config entries down while slot ids stay stable,
    /// so after `--without reviewer[0]` the entry now at config position 0 governs
    /// the survivor at live panel position 0. Id-only pairing re-points the
    /// survivor from its neighbour's entry instead: with
    /// `[claude@opus, codex@gpt-5, codex@gpt-5.6-terra]` the `gpt-5` seat silently
    /// becomes `gpt-5.6-terra` on a no-op amend. Failed before position pairing:
    /// `review-1-cli-codex` came back `gpt-5.6-terra`.
    #[test]
    fn a_drop_then_amend_repoints_each_survivor_from_its_own_position() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Failed);
        for (id, provider, model) in [
            ("review-0-cli-claude", "cli:claude", "opus"),
            ("review-1-cli-codex", "cli:codex", "gpt-5"),
            ("review-2-cli-codex", "cli:codex", "gpt-5.6-terra"),
        ] {
            let mut slot =
                executor::init_slot_model(id, provider, SlotRole::Reviewer, Some(model.into()));
            slot.status = SlotStatus::Failed;
            st.slots.push(slot);
        }
        st.providers = vec!["cli:muse".into(), "cli:claude".into(), "cli:codex".into()];

        let mut cfg = cfg_with_implementer("cli:muse@muse-spark-1.3");
        cfg.roles.reviewer = vec![
            "cli:claude@opus".into(),
            "cli:codex@gpt-5".into(),
            "cli:codex@gpt-5.6-terra".into(),
        ];
        cfg.cli_role_keys.insert(SlotRole::Reviewer);
        let dropped = cfg
            .apply_without(
                &st.providers.clone(),
                PoolOrigin::Synth,
                true,
                &["reviewer[0]".to_string()],
            )
            .unwrap();
        let retired = retire_reviewer_seats(&mut st, &dropped).unwrap();
        delete_dropped_reviewer_files(&st, &paths, &retired).unwrap();

        prepare_implement_slots(
            &mut st,
            None,
            PoolOrigin::Synth,
            true,
            &cfg,
            &paths,
            Reentry {
                from_round_gate: false,
                repoint: true,
                repoint_implementer: false,
            },
        )
        .unwrap();

        let model_of = |id: &str| {
            st.slots
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("survivor {id} still seated"))
                .model
                .clone()
        };
        assert_eq!(
            model_of("review-1-cli-codex").as_deref(),
            Some("gpt-5"),
            "the first survivor keeps the entry at its live panel position"
        );
        assert_eq!(
            model_of("review-2-cli-codex").as_deref(),
            Some("gpt-5.6-terra"),
            "the second survivor keeps the entry at its live panel position"
        );
    }

    /// A drop plus a backup failover must not hand a survivor its neighbour's
    /// entry: with `[codex@gpt-b, codex@gpt-c]` left by the drop, the entry at
    /// config position 1 governs the seat at live panel position 1, which now
    /// serves its backup. Falling back to the stamped id `review-1-cli-codex`
    /// instead re-points the `gpt-b` survivor sitting at position 0, and the
    /// later entry wins: the survivor silently becomes `gpt-c` on a no-op
    /// amend. Failed before the fallback was removed.
    #[test]
    fn a_drop_then_backup_never_repoints_a_survivor_from_its_neighbours_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Failed);
        for (id, provider, model) in [
            ("review-0-cli-codex", "cli:codex", "gpt-a"),
            ("review-1-cli-codex", "cli:codex", "gpt-b"),
            ("review-2-cli-codex", "cli:codex", "gpt-c"),
        ] {
            let mut slot =
                executor::init_slot_model(id, provider, SlotRole::Reviewer, Some(model.into()));
            slot.status = SlotStatus::Failed;
            st.slots.push(slot);
        }
        st.providers = vec!["cli:muse".into(), "cli:codex".into()];

        let mut cfg = cfg_with_implementer("cli:muse@muse-spark-1.3");
        cfg.roles.reviewer = vec![
            "cli:codex@gpt-a".into(),
            "cli:codex@gpt-b".into(),
            "cli:codex@gpt-c".into(),
        ];
        cfg.cli_role_keys.insert(SlotRole::Reviewer);
        let dropped = cfg
            .apply_without(
                &st.providers.clone(),
                PoolOrigin::Synth,
                true,
                &["reviewer[0]".to_string()],
            )
            .unwrap();
        let retired = retire_reviewer_seats(&mut st, &dropped).unwrap();
        delete_dropped_reviewer_files(&st, &paths, &retired).unwrap();
        let pin = crate::provider_ref::Pin::parse("cli:claude@opus").unwrap();
        let moved = move_seat(
            &mut st,
            &paths,
            "review-2-cli-codex",
            &pin,
            SeatSource::Backup,
        )
        .unwrap();
        assert_eq!(moved, "review-2-cli-claude");

        prepare_implement_slots(
            &mut st,
            None,
            PoolOrigin::Synth,
            true,
            &cfg,
            &paths,
            Reentry {
                from_round_gate: false,
                repoint: true,
                repoint_implementer: false,
            },
        )
        .unwrap();

        let slot_of = |id: &str| {
            st.slots
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("{id} still seated"))
        };
        assert_eq!(
            slot_of("review-1-cli-codex").model.as_deref(),
            Some("gpt-b"),
            "the first survivor keeps its own entry, not its neighbour's"
        );
        assert_eq!(
            slot_of("review-2-cli-claude").model.as_deref(),
            Some("opus"),
            "the backup-serving seat keeps the backup model"
        );
    }

    /// A work-failure rotation re-points a pinned seat's provider but keeps its
    /// id (`point_at` stamps the provider, not the id), so the preview must pair
    /// the way the apply does: by live panel position with a provider-agrees
    /// skip. Id pairing refuses the amend (`review-1-cli-codex` resolves
    /// `cli:codex` while the slot carries `cli:grok`) even though the apply
    /// would skip that seat and carry on. Failed before the preview paired
    /// positionally: the amend was refused.
    #[test]
    fn preview_allows_what_a_rotation_left_the_apply_to_skip() {
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Failed);
        for (id, provider) in [
            ("review-0-cli-claude", "cli:claude"),
            ("review-1-cli-codex", "cli:grok"),
        ] {
            let mut slot = executor::init_slot(id, provider, SlotRole::Reviewer);
            slot.status = SlotStatus::Failed;
            slot.source = Some(SeatSource::CliRole);
            st.slots.push(slot);
        }
        st.providers = vec!["cli:muse".into(), "cli:claude".into(), "cli:codex".into()];
        st.pool_origin = PoolOrigin::CliProviders;

        let mut cfg = cfg_with_implementer("cli:muse@muse-spark-1.3");
        cfg.roles.reviewer = vec!["cli:claude".into(), "cli:codex".into()];
        cfg.cli_role_keys.insert(SlotRole::Reviewer);

        let inputs = BoundAmendInputs {
            effort: vec!["reviewer=high".into()],
            ..BoundAmendInputs::default()
        };
        preview_bound_amend(&cfg, None, &st, &inputs).unwrap();
    }

    /// A re-point never writes a provider over a seat whose id names another one:
    /// a reviewer seat's id encodes its provider, so a genuinely different provider
    /// resolves to a seat id that does not exist yet and the entry is skipped.
    /// Every reviewer seat's provider still matches the provider its id names.
    #[test]
    fn a_repoint_keeps_every_seat_matching_the_provider_its_id_names() {
        let mut st = RunState::new(
            "panel001",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        for (id, prov, model) in [
            ("review-0-cli-claude", "cli:claude", "opus"),
            ("review-1-cli-codex", "cli:codex", "gpt-5.6-terra"),
        ] {
            let mut slot =
                executor::init_slot_model(id, prov, SlotRole::Reviewer, Some(model.into()));
            slot.status = SlotStatus::Done;
            st.slots.push(slot);
        }
        st.providers = vec!["cli:claude@opus".into(), "cli:codex@gpt-5.6-terra".into()];

        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:claude@opus".into(), "cli:codex@gpt-5.6-terra".into()];
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            false,
        )
        .unwrap();

        let panel: Vec<(&str, &str)> = st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Reviewer)
            .map(|s| (s.id.as_str(), s.provider.as_str()))
            .collect();
        assert_eq!(
            panel,
            vec![
                ("review-0-cli-claude", "cli:claude"),
                ("review-1-cli-codex", "cli:codex"),
            ],
            "every seat's provider must still match the provider its id names"
        );
        for s in st.slots.iter().filter(|s| s.role == SlotRole::Reviewer) {
            assert!(
                s.id.ends_with(&crate::util::sanitize_slot(&s.provider)),
                "seat {} carries {}, so its id is a lie",
                s.id,
                s.provider
            );
        }
    }

    /// The seat that is mid-flight is the one whose provider is load-bearing: its pid
    /// marker is filed against what actually spawned, so rewriting the record would
    /// make the run disagree with the process it is waiting on.
    #[test]
    fn a_running_seat_is_never_repointed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Running);
        let cfg = cfg_with_implementer("cli:muse@muse-spark-1.2");

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            true,
        )
        .unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(
            impl_slot.model.as_deref(),
            Some("muse-spark-1.3"),
            "a live dispatch keeps the provider that spawned it"
        );
    }

    /// The implementer's seat id names no provider, so a provider change on amend
    /// re-points the seat instead of refusing after the amend is already saved.
    #[test]
    fn an_implementer_provider_change_repoints_instead_of_refusing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:claude", "opus", SlotStatus::Pending);
        let cfg = cfg_with_implementer("cli:grok");

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            true,
        )
        .unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(impl_slot.provider.as_str(), "cli:grok");
    }

    /// The stuck ladder rotates the implementer off the pin and stamps the rung
    /// it drew from, never Backup: an amend that does not touch the implementer
    /// pin must leave that seat alone instead of silently moving it back.
    #[test]
    fn an_unrelated_amend_leaves_a_ladder_rotated_implementer_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:codex", "gpt-5", SlotStatus::Pending);
        st.slots[0].source = Some(SeatSource::CliRole);
        let cfg = cfg_with_implementer("cli:claude");

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            false,
        )
        .unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(
            impl_slot.provider.as_str(),
            "cli:codex",
            "only an amend that changes the implementer pin moves the seat"
        );
    }

    /// The implementer-branch gate is the resolved pin, not the amend: restating
    /// the same pin resolves identically and moves nothing, while a new pin
    /// resolves differently. No amend means no re-point either way.
    #[test]
    fn only_a_moved_implementer_pin_repoints() {
        let pool = vec!["cli:claude".to_string(), "cli:codex".to_string()];
        let frozen = cfg_with_implementer("cli:claude");
        let restated = cfg_with_implementer("cli:claude");
        assert!(
            !should_repoint_implementer(
                true,
                &frozen,
                &pool,
                PoolOrigin::Synth,
                &restated,
                &pool,
                PoolOrigin::Synth,
                &crate::providers::StubHealth::allow_all(),
            ),
            "restating the pin resolves the same seat"
        );
        let moved = cfg_with_implementer("cli:codex");
        assert!(
            should_repoint_implementer(
                true,
                &frozen,
                &pool,
                PoolOrigin::Synth,
                &moved,
                &pool,
                PoolOrigin::Synth,
                &crate::providers::StubHealth::allow_all(),
            ),
            "a new pin resolves a different seat"
        );
        assert!(
            !should_repoint_implementer(
                false,
                &frozen,
                &pool,
                PoolOrigin::Synth,
                &moved,
                &pool,
                PoolOrigin::Synth,
                &crate::providers::StubHealth::allow_all(),
            ),
            "no amend means no re-point even when the configs disagree"
        );
    }

    /// The re-point is a provider change, so it goes through the same primitive
    /// as every other one: the old provider's outcome stays off the seat, a
    /// level the new CLI refuses is dropped, and a stale session marker for the
    /// new provider is cleared.
    #[test]
    fn an_implementer_repoint_resets_the_old_providers_outcome() {
        use crate::effort::EffortLevel;
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:claude", "opus", SlotStatus::Failed);
        {
            let s = &mut st.slots[0];
            s.quota_hit = true;
            s.error = Some("rate limited".into());
            s.usage = Some(crate::state::SlotUsage {
                slot_id: "impl".into(),
                provider: "cli:claude".into(),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                context_tokens: 0,
                context_semantics: crate::state::ContextSemantics::Unknown,
                billed_tokens: 2,
                tools: 0,
                model: None,
                cost_usd: None,
                subagent_stats: None,
                model_usage: Default::default(),
            });
            s.effort = Some(EffortLevel::Max);
        }
        let marker = paths.marker("repoint1", "impl.grok.session_id");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "stale-session").unwrap();
        let cfg = cfg_with_implementer("cli:grok");

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            true,
        )
        .unwrap();

        let s = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(s.provider.as_str(), "cli:grok");
        assert!(!s.quota_hit, "claude's quota hit must not ride along");
        assert_eq!(s.error, None);
        assert!(s.usage.is_none(), "claude's usage must not ride along");
        assert_eq!(s.status, SlotStatus::Pending);
        assert_eq!(
            s.effort, None,
            "grok takes no max, and a recorded level beats config"
        );
        assert_eq!(
            crate::markers::read_session_id(&paths, "repoint1", "impl", "grok"),
            None,
            "a marker from an earlier grok rotation belongs to a conversation this seat never had"
        );
    }

    /// A seat on its engaged backup keeps the failover provider: the config names
    /// the primary, so re-pointing would undo the failover and then refuse, since
    /// the backup provider disagrees with the resolved seat.
    #[test]
    fn a_backup_served_seat_is_never_repointed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = run_with_impl("cli:grok", "grok-model", SlotStatus::Pending);
        st.slots[0].source = Some(SeatSource::Backup);
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        let cfg = cfg_with_implementer("cli:claude");

        repoint_existing_seats(
            &mut st,
            &paths,
            &cfg,
            &crate::providers::StubHealth::allow_all(),
            true,
        )
        .unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(impl_slot.provider.as_str(), "cli:grok");
    }

    /// The cap must reserve what is about to run.
    ///
    /// `spar stop` leaves every interrupted slot at `Failed`, and
    /// `daemon::run_demand` counts only `Pending`/`Running` — so a stopped
    /// Review/Arena/Roles/Peer run read as needing *nothing*, was admitted past a
    /// full cross-run cap, and then overran it as `executor::run_slot` flipped each
    /// retained slot back to `Running`.
    ///
    /// Asserted against `run_demand` itself, not against slot statuses after a
    /// resume: a dry-run workflow completes its slots to `done`, so an end-state
    /// assertion passes with or without the fix and guarantees nothing. (It did.)
    #[test]
    fn a_stops_interrupted_slots_are_demand_again_before_admission() {
        let mut state = RunState::new(
            "capbypass",
            crate::cli::WorkflowKind::Review,
            std::path::PathBuf::from("/x"),
        );
        for i in 0..3 {
            let mut slot =
                crate::executor::init_slot(format!("review-{i}"), "cli:claude", SlotRole::Reviewer);
            // The state `stop_one` leaves behind: interrupted, reconciled to `Failed`.
            slot.status = SlotStatus::Failed;
            state.slots.push(slot);
        }

        let before: u32 = crate::daemon::run_demand(&state).values().copied().sum();
        assert_eq!(
            before, 0,
            "precondition: this is exactly why the cap could be bypassed"
        );

        restore_interrupted_slots(&mut state);

        let after: u32 = crate::daemon::run_demand(&state).values().copied().sum();
        assert_eq!(
            after, 3,
            "every slot the workflow is about to re-dispatch has to count as demand"
        );
    }

    /// `--without suite --reload-config` on a run whose earlier round already dispatched
    /// an agent tester must reap that slot, not leave it for the next round to
    /// re-dispatch (review `review-0-cli-claude`, finding 5).
    #[test]
    fn disabling_suite_reaps_an_existing_tester_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(executor::init_slot(
            "suite-cli-claude",
            "cli:claude",
            SlotRole::Tester,
        ));
        let mut cfg = Config::default();
        cfg.suite.enabled = false;

        ensure_suite_slot(&mut state, true, &cfg, &paths).unwrap();

        assert!(
            state.slots.iter().all(|s| s.role != SlotRole::Tester),
            "a resumed run with --without suite must not keep dispatching its old tester: {:?}",
            state.slots
        );
    }
}

enum SuiteResult {
    Pass,
    Fail,
}

/// Parse the `## Result` line. `None` means the file has no parsable verdict — the
/// agent wrote garbage, which is a runner problem, not a code failure.
fn parse_suite_result(body: &str) -> Option<SuiteResult> {
    let lower = body.to_ascii_lowercase();
    let idx = lower.find("## result")?;
    let after = &lower[idx..];
    let line = after
        .lines()
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_start_matches(['*', '`', '_', '-', ' ']);
    if line.starts_with("pass") || line.starts_with("skipped") {
        return Some(SuiteResult::Pass);
    }
    if line.starts_with("fail") {
        return Some(SuiteResult::Fail);
    }
    None
}

/// Tri-state suite verdict. `Fail` requires a clean tester exit AND a `## Result: fail`;
/// anything else uncertain (signal death, timeout, missing/garbled report) is `Inconclusive`.
fn derive_suite_outcome(slot_ok: bool, exit_code: Option<i32>, body: Option<&str>) -> SuiteOutcome {
    if !slot_ok || exit_code.is_none() {
        return SuiteOutcome::Inconclusive;
    }
    let Some(body) = body else {
        return SuiteOutcome::Inconclusive;
    };
    match parse_suite_result(body) {
        Some(SuiteResult::Pass) => SuiteOutcome::Pass,
        Some(SuiteResult::Fail) => SuiteOutcome::Fail,
        None => SuiteOutcome::Inconclusive,
    }
}

/// Both `Fail` and `Inconclusive` gate the ship (fail closed).
fn suite_blocks_ship(outcome: SuiteOutcome) -> bool {
    matches!(outcome, SuiteOutcome::Fail | SuiteOutcome::Inconclusive)
}

/// Acceptance gate (DECISIONS O19). An empty `criteria` means there is no contract at
/// all (`[spec].enabled = false`) — acceptance is not evaluated and the verdict alone
/// gates. Otherwise this is fail closed: a criterion the reviewer never mentioned counts
/// against the ship exactly like a reported `fail`.
pub(crate) fn acceptance_blocks_ship(
    criteria: &[String],
    res: &ReviewResult,
    cfg: &Config,
) -> bool {
    !failing_acceptance_ids(criteria, res, cfg).is_empty()
}

/// The unwaived criteria that keep this review blocking the ship: missing from
/// the report, failed, or unverified when the project requires every criterion
/// verified. One expression behind both the stall detector and the warning, so
/// the warning can name the same criteria that fired the stall.
fn failing_acceptance_ids(criteria: &[String], res: &ReviewResult, cfg: &Config) -> Vec<String> {
    if criteria.is_empty() {
        return Vec::new();
    }
    criteria
        .iter()
        .filter(|id| match res.acceptance.iter().find(|a| &a.id == *id) {
            None => true,
            Some(a) => match a.status {
                AcStatus::Fail => true,
                AcStatus::Unverified => cfg.review.require_all_criteria,
                AcStatus::Pass => false,
            },
        })
        .cloned()
        .collect()
}

/// One failing acceptance criterion: its id plus the reason it blocked, not one
/// joined string. The carry-forward brief renders a bullet per id up front where
/// the bullet clamp cannot reach it, while each reason still clamps to fit.
#[derive(Debug)]
pub(crate) struct AcceptanceReason {
    pub id: String,
    pub reason: String,
}

impl std::fmt::Display for AcceptanceReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.id, self.reason)
    }
}

/// Which criteria blocked and why, one per criterion rather than pre-joined: the
/// carry-forward brief renders these as bullets and truncates on line boundaries,
/// so a single joined line (measured at 4k-10k chars on real reviews) is dropped
/// whole and leaves a heading saying nothing blocked.
pub(crate) fn acceptance_block_reasons(
    criteria: &[String],
    res: &ReviewResult,
    cfg: &Config,
) -> Vec<AcceptanceReason> {
    let mut parts: Vec<AcceptanceReason> = Vec::new();
    for id in criteria {
        match res.acceptance.iter().find(|a| &a.id == id) {
            None => parts.push(AcceptanceReason {
                id: id.clone(),
                reason: "not reported by the reviewer".to_string(),
            }),
            Some(a) => match a.status {
                AcStatus::Fail => {
                    let ev = if a.evidence.is_empty() {
                        "no evidence given".to_string()
                    } else {
                        a.evidence.clone()
                    };
                    parts.push(AcceptanceReason {
                        id: id.clone(),
                        reason: format!("fail: {ev}"),
                    });
                }
                AcStatus::Unverified if cfg.review.require_all_criteria => {
                    parts.push(AcceptanceReason {
                        id: id.clone(),
                        reason: "unverified".to_string(),
                    })
                }
                _ => {}
            },
        }
    }
    parts
}

/// The reviewed-commit gate's decision for one reviewer slot. The panel-dispatch
/// head is the contract: a reviewer that judged an older sha is stale, and one
/// that judged a newer sha is equally not a verdict — the suite ran against the
/// contract head and sibling reviewers judged it, so counting a verdict about a
/// different tree would smuggle an unjudged tree past the panel. Both self-heal:
/// the mismatch feeds `any_request_changes` and the next round re-resolves head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewedCommitDecision {
    /// spar could not resolve the panel head; skip the check loudly, fail open.
    Skipped,
    /// The parsed sha names the panel head.
    Match,
    /// The artifact names no (parsable) commit.
    Absent,
    /// The parsed sha names a different commit.
    Mismatch,
}

/// The recorded verdict a stood-down reviewer carries into the gate: only a seat
/// that never dispatched this round (`stood_down`) and whose last recorded
/// verdict rejects this head. A veto about an older head was superseded by the
/// fix round; a seat that never judged has nothing to carry; a failure stub
/// names no commit, so only a real judgment about this head carries.
fn carried_rejection<'a>(
    slot: &'a SlotState,
    panel_head: Option<&str>,
) -> Option<&'a ReviewResult> {
    slot.stood_down.as_ref()?;
    let head = panel_head?;
    let res = slot.review.as_ref()?;
    if res.verdict != Some(review_result::Verdict::RequestChanges) {
        return None;
    }
    let sha = res.reviewed_commit.as_deref()?;
    review_result::reviewed_commit_matches(head, sha).then_some(res)
}

/// The stood-down seats whose recorded veto rejects the head about to ship:
/// judged, unanswered, and unable to re-judge. The same verdict and acceptance
/// predicates judge them as fresh artifacts. Factored out of the panel so the
/// fail-closed rule stays covered when no run reaches it end to end: a
/// no-progress round cannot reach a panel without moving HEAD, and moving HEAD
/// voids the veto, so the stall detector parks before a panel that would only
/// re-count the same veto.
fn carried_veto_blockers(
    slots: &[SlotState],
    panel_head: Option<&str>,
    criteria: &[String],
    cfg: &Config,
) -> Vec<(String, review_result::ReviewResult)> {
    let mut blockers = Vec::new();
    for s in slots.iter().filter(|s| s.role == SlotRole::Reviewer) {
        if let Some(res) = carried_rejection(s, panel_head) {
            if res.blocks(cfg.review.block_on) || acceptance_blocks_ship(criteria, res, cfg) {
                blockers.push((s.id.clone(), res.clone()));
            }
        }
    }
    blockers
}

/// The panel's lines for the carried vetoes above, in the exact wording the
/// panel pushes. A no-progress round parks before the panel without moving
/// HEAD, and moving HEAD voids the veto, so no end-to-end run reaches the
/// panel with a carried veto; this keeps the fail-closed wording covered at
/// the seam the panel actually calls.
fn carried_veto_blocker_lines(
    slots: &[SlotState],
    panel_head: Option<&str>,
    criteria: &[String],
    cfg: &Config,
) -> Vec<String> {
    carried_veto_blockers(slots, panel_head, criteria, cfg)
        .iter()
        .map(|(id, res)| {
            format!(
                "review `{id}` stood down on quota; its recorded request_changes on {} still blocks this head",
                res.reviewed_commit.as_deref().unwrap_or("unknown")
            )
        })
        .collect()
}

fn decide_reviewed_commit(panel_head: Option<&str>, res: &ReviewResult) -> ReviewedCommitDecision {
    let Some(head) = panel_head else {
        return ReviewedCommitDecision::Skipped;
    };
    let Some(sha) = res.reviewed_commit.as_deref() else {
        return ReviewedCommitDecision::Absent;
    };
    if review_result::reviewed_commit_matches(head, sha) {
        ReviewedCommitDecision::Match
    } else {
        ReviewedCommitDecision::Mismatch
    }
}

/// Fail a reviewer slot closed on a reviewed-commit gate trip: failed status with
/// a greppable error, a slot-attributed event, and a stderr warning. The
/// artifact bytes are preserved as drift evidence, never synthesized over.
fn fail_review_slot_on_commit(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    error: String,
) {
    if let Some(s) = state.slot_mut(slot_id) {
        s.status = SlotStatus::Failed;
        s.error = Some(error.clone());
    }
    eprintln!("warning: review slot `{slot_id}` failed: {error}");
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot_note(slot_id, error),
    );
    let _ = state.save(paths);
}

/// Why the suite was `Inconclusive`, for the bus broadcast and the reviewer prompt.
fn suite_inconclusive_reason(
    slot_ok: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    body: Option<&str>,
) -> String {
    if let Some(sig) = signal {
        return format!("suite runner killed by signal {sig} before a clean report");
    }
    if exit_code.is_none() {
        return "suite runner exited without a status (timed out or killed)".into();
    }
    if !slot_ok {
        return "suite runner did not complete cleanly".into();
    }
    match body {
        None => "no suite.md written".into(),
        Some(_) => "suite.md has no parsable ## Result".into(),
    }
}

/// Added test files that the suite's own command list never names.
///
/// The tester writes its command list itself, so an implementer that adds a new
/// integration-test binary can leave the authoritative gate passing green without ever
/// compiling the code written to prove the change. This warns; it never edits the
/// command list, because a harness that rewrites its own gate is not a gate.
///
/// Silent unless the suite is *selective*. A bare `cargo test` / `pytest` compiles
/// everything, so naming no files there means nothing — warning on it would fire on
/// every run and train the operator to ignore it.
fn unreferenced_test_files(cwd: &Path, base: Option<&str>, suite_body: &str) -> Vec<String> {
    let base = match base {
        Some(b) => b,
        None => return Vec::new(),
    };
    let commands = suite_commands(suite_body);
    // *Every* command must be narrow. One command running the project default covers the
    // added files whatever the others do, so `any` here would warn about files that were
    // in fact compiled.
    if commands.is_empty() || !commands.iter().all(|c| command_is_selective(c)) {
        return Vec::new();
    }
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=A", base, "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(out) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        // Test *targets* only. A `tests/common/mod.rs` helper or a `tests/data/golden.json`
        // fixture is not a target anything can name, so reporting it as uncovered is a
        // false claim in the one place a false claim is most expensive: the reviewers'
        // suite report.
        .filter(|f| !f.is_empty() && is_test_path(f) && is_test_target(f))
        .filter(|f| !commands.iter().any(|c| command_names(c, f)))
        .map(str::to_string)
        .collect()
}

/// Command lines from the report's `## Commands` section (`- \`cmd\` → exit N`).
fn suite_commands(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("##") {
            in_section = t
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case("commands");
            continue;
        }
        if in_section {
            if let Some(rest) = t.strip_prefix('-') {
                let cmd = rest.trim().trim_start_matches('`');
                let cmd = cmd.split('`').next().unwrap_or(cmd).trim();
                if !cmd.is_empty() {
                    out.push(cmd.to_string());
                }
            }
        }
    }
    out
}

/// True when a command narrows to specific *targets* instead of running the project's
/// default set.
///
/// Explicit target flags plus test-file arguments only. Earlier this also counted any
/// token containing `/`, which made `pytest tests/`, `go test ./...` and
/// `./scripts/test.sh` all read as selective — the canonical full-suite command of three
/// ecosystems, each of which does collect every new test file. Warning on those is the
/// false-positive class this check exists to avoid.
///
/// `-p` / `--package` are deliberately absent: `cargo test -p foo` builds every test
/// target in `foo`, including one added this round, so it covers what it does not name.
fn command_is_selective(cmd: &str) -> bool {
    // `a && b` runs both, so the line is narrow only if *every* stage is. Judging one
    // stage is wrong in both directions: `cd repo && cargo test --test foo` would be read
    // as the `cd`, and `cargo test --lib && cargo test --tests` as the `--lib` while
    // `--tests` builds every integration target. Pipeline tails and redirect targets are
    // dropped rather than judged.
    let stages: Vec<&str> = cmd
        .split("&&")
        .map(|s| s.split(['|', '>', ';']).next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        // A `cd repo &&` / `source .venv/bin/activate &&` prefix is setup, not a test
        // invocation, and judging it would read the whole line as broad.
        .filter(|s| !is_shell_setup(s))
        .collect();
    !stages.is_empty() && stages.iter().all(|s| stage_is_selective(s))
}

fn is_shell_setup(stage: &str) -> bool {
    const SETUP: [&str; 6] = ["cd", "export", "source", ".", "set", "unset"];
    stage
        .split_whitespace()
        .next()
        .is_some_and(|head| SETUP.contains(&head))
}

fn stage_is_selective(stage: &str) -> bool {
    // Both spellings: `--test foo` and `--test=foo`.
    const NARROWING: [&str; 6] = ["--test ", "--test=", "--bin ", "--bin=", "--lib", "::"];
    if NARROWING.iter().any(|f| stage.contains(f)) {
        return true;
    }
    // A named test *source file* is selective; a directory, a glob, or a path handed to a
    // flag is not. The first token is the runner, not a target.
    let mut prev_is_flag = false;
    for tok in stage.split_whitespace().skip(1) {
        let is_flag = tok.starts_with('-');
        let candidate = !is_flag && !prev_is_flag && !tok.ends_with('/');
        prev_is_flag = is_flag;
        if candidate && is_test_path(tok) && is_test_source(tok) {
            return true;
        }
    }
    false
}

/// A test file that is its own target, i.e. one a suite command could name.
///
/// Cargo makes an integration target of `tests/*.rs` only at the top level: `tests/
/// common/mod.rs` is a helper compiled into the others and can never be named by
/// `--test`. Everything nested elsewhere is judged on the source-extension check alone.
fn is_test_target(path: &str) -> bool {
    if !is_test_source(path) {
        return false;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if file == "mod.rs" {
        return false;
    }
    match (path.find("tests/"), path.ends_with(".rs")) {
        (Some(i), true) => !path[i + "tests/".len()..].contains('/'),
        _ => true,
    }
}

/// A file a test runner could compile or collect. The extension allowlist is what keeps
/// `pytest -c tests/pytest.ini` and an added `tests/data/golden.json` from reading as
/// test targets.
fn is_test_source(tok: &str) -> bool {
    const SOURCE_EXT: [&str; 11] = [
        "rs", "py", "go", "ts", "tsx", "js", "jsx", "rb", "java", "kt", "swift",
    ];
    let file = tok.rsplit('/').next().unwrap_or(tok);
    file.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && SOURCE_EXT.contains(&ext))
}

fn is_test_path(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|c| c == "tests" || c == "test" || c == "__tests__")
        || file.starts_with("test_")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
}

/// A command covers a file when it names the path or the file stem (cargo's `--test foo`
/// for `tests/foo.rs`).
fn command_names(cmd: &str, path: &str) -> bool {
    if cmd.contains(path) {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    !stem.is_empty()
        && cmd
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|t| t == stem)
}

fn suite_guidance(outcome: SuiteOutcome) -> String {
    let header = "## Suite channel (do not re-run full suites)\n\
         A dedicated channel runs the full suite — spar's own configured commands, or a cheap tester slot; its output is the `## Suite report` section above.\n\n";
    match outcome {
        SuiteOutcome::Pass => format!(
            "{header}\
             - Do **not** kick off full multi-minute/hour test suites.\n\
             - At most: static/diff review, plus optional 1–2 targeted tests on suspect files.\n\
             - Use the suite report above for pass/fail evidence.\n"
        ),
        SuiteOutcome::Fail => format!(
            "{header}\
             - Do **not** kick off full multi-minute/hour test suites.\n\
             - At most: static/diff review, plus optional 1–2 targeted tests on suspect files.\n\
             - Use the suite report above for pass/fail evidence.\n\
             - Orchestrator treats suite **fail** as request_changes even if you approve.\n"
        ),
        SuiteOutcome::Inconclusive => format!(
            "{header}\
             - The suite channel is **inconclusive**: the runner fell over and the suite DID NOT RUN to a clean result. Do **not** cite this as a code or test failure.\n\
             - Do **not** kick off the full multi-minute/hour suite yourself.\n\
             - Instead, run 1–2 targeted tests on the files this change touches for confidence.\n"
        ),
    }
}

fn run_with_task(
    task: String,
    plan_body: Option<String>,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
    _parent_run: Option<String>,
) -> Result<ExitCode> {
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Loop,
        paths.project_root.clone(),
    );
    state.task = Some(task.clone());
    state.key = opts.key.clone();
    state.backend = opts.backend;
    worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
    cfg.save_snapshot(paths, &state.id)?;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    state.big = opts.big;
    state.max_fix_rounds = 3;
    state.max_rounds = opts
        .max_rounds
        .map_or(cfg.rounds.max, |mr| mr.resolve(state.round));
    let (n, roles) = crate::workflow::roles_resolve::reviewer_select_question(cfg);
    let requested = opts.resolve_pool(n, &roles, paths, cfg, &state.id)?;
    let pool_origin = crate::workflow::roles_resolve::pool_origin_for(&opts);
    // A later round resuming this run without `--reload-config` (O27) reads this back
    // via `run_from_approved`'s frozen-pool reuse, so it has to be recorded here too, not
    // just at the plan phase.
    state.pool_origin = pool_origin;
    let health = providers::LiveHealth::for_dry(dry);
    state.providers =
        providers::pick_providers(&requested, n, Some(&requested), dry, pool_origin, &health);
    state.stamp_dispatch();
    // Quota gate is now backup-aware and runs after seat resolution: a paused primary
    // with an eligible backup must not park the run.
    prepare_implement_slots(
        &mut state,
        Some(&requested),
        pool_origin,
        dry,
        cfg,
        paths,
        Reentry {
            from_round_gate: false,
            repoint: false,
            repoint_implementer: false,
        },
    )?;
    // Only the implementer must run next at run start, so only it can park the
    // run here: a paused reviewer or tester seat is reseated onto its backup
    // or stood down at its own dispatch.
    if !dry {
        if let Some(msg) = gate_implementer_seat(&mut state, paths, cfg)? {
            state.error = Some(msg.clone());
            state.set_phase(Phase::Quota);
            paths.ensure_run_dirs(&state.id)?;
            state.save(paths)?;
            if opts.json {
                executor::emit_run_json(&state)?;
            } else {
                eprintln!("error: {msg}");
            }
            return Ok(ExitCode::Quota);
        }
    }
    state.record_unhealthy_pin_warnings();

    paths.ensure_run_dirs(&state.id)?;
    let _ = crate::bus::ensure_bus(paths);
    let _ = crate::bus::join(
        paths,
        Some(&state.id),
        crate::bus::ORCHESTRATOR_SEAT,
        None,
        None,
    );
    if let Some(body) = &plan_body {
        std::fs::write(paths.artifact(&state.id, "plan.md"), body)?;
        if state.big {
            let _ = crate::tasks::seed_from_plan(paths, &state.id, body);
        }
    }
    state.save(paths)?;

    if opts.detach {
        return detach_implement(&state, paths, opts.json);
    }

    let _lock = crate::runlock::RunLock::acquire(paths, &state.id)?;
    execute_loop(&mut state, paths, cfg, opts.accept_contract)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

fn maybe_auto_ship_or_cleanup(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    if state.phase == Phase::AwaitingShipConfirm && cfg.auto_ship() {
        state.gates.ship_confirmed = true;
        // leave at AwaitingShipConfirm with gate set — ship command still does push
        // unless we call ship; for dry-run mark Done
        if state.dry_run {
            state.set_phase(Phase::Done);
            state.save(paths)?;
        }
    }
    if cfg.auto_cleanup && state.phase.is_terminal() && matches!(state.phase, Phase::Done) {
        let _ = crate::worktree::cleanup_run(state, false);
    }
    Ok(())
}

/// True once `spar stop` has dropped the `stopped` marker for this run.
pub fn should_stop(paths: &SparPaths, run_id: &str) -> bool {
    crate::markers::marker_exists(paths, run_id, "stopped") || crate::process::shutdown_requested()
}

/// Halt without dispatching or touching worktrees; the run stays resumable.
fn stop_now(state: &mut RunState, paths: &SparPaths) -> Result<()> {
    state.set_phase(Phase::Stopped);
    state.save(paths)?;
    Ok(())
}

/// The acceptance contract as it stood when the round loop was entered. `body` is the
/// bytes read from disk, never the prompt copy: the overlay note appended to the
/// prompt below must not be readable as a criterion (defect 2, DECISIONS O43).
pub struct FrozenContract {
    pub body: String,
    pub criteria: Vec<String>,
    pub fingerprint: String,
}

impl FrozenContract {
    pub fn freeze(body: &str) -> Self {
        let criteria = review_result::parse_contract_criteria(body);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        Self {
            body: body.to_string(),
            criteria,
            fingerprint: format!("{:x}", hasher.finish()),
        }
    }

    /// Compares against the frozen bytes, not the prompt copy the round mutates.
    pub fn drifted(&self, on_disk: &str) -> bool {
        self.body != on_disk
    }
}

/// The whole reviewer panel stood down, so the panel has no usable seat: any
/// quota stand-down parks the run, an all-readiness stand-down fails. `None`
/// while any reviewer still runs (or the panel is empty). Shared by the
/// dispatch and the `--validate-only --run` projection, so the two stop the
/// same panel the same way.
fn panel_stop(
    reviewer_count: usize,
    stood_down: &[(ParkCause, String)],
) -> Option<(ParkCause, String)> {
    if reviewer_count == 0 || stood_down.len() != reviewer_count {
        return None;
    }
    let cause = if stood_down.iter().any(|(cause, _)| cause.quota_hit()) {
        ParkCause::Quota
    } else {
        ParkCause::Readiness
    };
    let messages = stood_down
        .iter()
        .map(|(_, msg)| msg.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Some((cause, messages))
}

pub fn execute_loop(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    accept_contract: bool,
) -> Result<()> {
    // A resume inherits whatever the last orchestrator left behind; settle its slots
    // before this round stamps its own over them (O49). `Nobody`, asserted: the run lock
    // is held by this process and nothing has been dispatched yet in this call, so any
    // slot still `running` belongs to an orchestrator that is gone. Observing the lock
    // here would find *us* and skip the demotion.
    state.reconcile_and_save(
        paths,
        crate::state::RunOwner::Nobody,
        crate::state::ORPHANED_SLOT,
    )?;

    // Only the implementer gets a branch; reviewers judge from detached checkouts cut
    // at each panel head (O138).
    let impl_ids: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .map(|s| s.id.clone())
        .collect();
    worktree::prepare_isolation(state, paths, &impl_ids)?;

    let plan_body =
        std::fs::read_to_string(paths.artifact(&state.id, "plan.md")).unwrap_or_default();
    let amendment_section = state
        .amendment
        .as_deref()
        .map(|a| {
            format!(
                "## Amendment (this round)\nThe operator supplied a directive for THIS round. It takes precedence over the original task where they conflict. The original task below is context; the amendment is the work.\n\n{a}\n"
            )
        })
        .unwrap_or_default();
    let test_contract_body = {
        let p = paths.artifact(&state.id, "test-contract.md");
        std::fs::read_to_string(&p).unwrap_or_else(|_| {
            "(no pre-written acceptance contract — implement without frozen tests)".into()
        })
    };
    // Frozen once, from the bytes on disk, before the overlay note is appended below so
    // the note can never be read as a criterion. Every round for the life of this
    // `execute_loop` call judges reviewers against this same list; re-entering
    // `implement --run <id>` is what re-freezes against an amended file (DECISIONS O43).
    let frozen = FrozenContract::freeze(&test_contract_body);
    if frozen.criteria.is_empty() && review_result::mentions_criterion_id(&frozen.body) {
        let msg = "test-contract.md mentions AC-n ids but declares no criteria; \
                    the acceptance gate is vacuous for this run until a declaration is added"
            .to_string();
        eprintln!("warning: {msg}");
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    }
    let previous_fingerprint = state.contract_fingerprint.clone();
    let moved = previous_fingerprint
        .as_ref()
        .is_some_and(|prev| *prev != frozen.fingerprint);
    // O43 made re-freezing a *deliberate* human act — stop, edit, re-run — and the round
    // ceiling (O52) turned it into a routine one that spar itself prompts for. So a
    // re-entry may not adopt a contract spar watched move under the slot the contract
    // bounds: the implementer can write to `artifacts/`, and "delete the criterion you
    // cannot pass, then let the ceiling invite the operator to re-freeze it" is a clean
    // path past the acceptance gate. `contract_modified` is exactly the flag that says
    // spar saw it happen, so it is the flag that closes the door.
    if moved && state.contract_modified && !accept_contract {
        bail!(
            "test-contract.md changed while the previous round was running, and this \
             re-entry would adopt it: {} criteria now ({}), was fingerprint {}.\n\
             The implementer can write to that file, so spar will not re-freeze it for \
             you. Diff it against what you approved, then either revert it or re-run \
             with --accept-contract.",
            frozen.criteria.len(),
            frozen.fingerprint,
            previous_fingerprint.as_deref().unwrap_or("(none)")
        );
    }
    state.contract_fingerprint = Some(frozen.fingerprint.clone());
    state.contract_modified = false;
    // The frozen text itself, kept apart from the file the implementer can write to,
    // so `spar amend` starts from what was frozen (O137).
    if paths.artifact(&state.id, "test-contract.md").is_file() {
        let _ = std::fs::write(
            paths.artifact(&state.id, crate::workflow::plan::FROZEN_CONTRACT),
            &frozen.body,
        );
    }
    if moved {
        let msg = format!(
            "contract re-frozen: {} criteria (fingerprint {}, was {}){}",
            frozen.criteria.len(),
            frozen.fingerprint,
            previous_fingerprint.as_deref().unwrap_or("(none)"),
            if accept_contract {
                " — adopted a drifted contract via --accept-contract"
            } else {
                ""
            }
        );
        // Loud, like the adjacent vacuous-contract warning: a re-freeze changes what the
        // ship gate enforces, and one line in events.jsonl is not somewhere anyone looks.
        eprintln!("warning: {msg}");
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    }
    state.save(paths)?;
    let mut test_contract_body = test_contract_body;

    // Bring pre-coding acceptance tests into implementer cwd (fail closed if author ran).
    if let Some(author) = state.test_author_seat().map(|s| s.id.clone()) {
        let impl_cwd = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .and_then(|s| s.cwd.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("implementer cwd missing; cannot apply acceptance tests")
            })?;
        match worktree::apply_spec_tests_to_impl(state, &author, &impl_cwd) {
            Err(e) => {
                return fail(
                    state,
                    paths,
                    anyhow::anyhow!("failed to apply acceptance tests from {author}: {e}"),
                    false,
                );
            }
            // Said out loud on every channel: the overlay carries git-visible files only,
            // so a fixture the author wrote into an ignored path stays behind. Silent, that
            // surfaces as an acceptance test failing at runtime for no visible reason —
            // and the implementer is told it may weaken a test if it documents why.
            Ok(overlay) if !overlay.ignored.is_empty() => {
                let note = format!(
                    "acceptance tests copied WITHOUT {} git-ignored path(s) ({}); \
                     they remain in {}",
                    overlay.ignored.len(),
                    overlay.ignored.join(", "),
                    overlay.author_path.display()
                );
                eprintln!("note: {note}");
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(note.clone()),
                );
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    crate::bus::ORCHESTRATOR_SEAT,
                    note,
                    state.message_budget,
                );
                test_contract_body.push_str(&format!(
                    "\n## Not copied (git-ignored in the test-author worktree)\n\
                     The acceptance tests above were copied from `{}`, but git ignores these \
                     paths so they did **not** come with them:\n{}\n\n\
                     If a test needs one of them, copy it across yourself from that worktree. \
                     Do **not** copy build output or dependency directories (`target/`, \
                     `node_modules/` and the like) — those are ignored on purpose and \
                     rebuilding is cheaper than copying them.\n",
                    overlay.author_path.display(),
                    overlay
                        .ignored
                        .iter()
                        .map(|p| format!("- `{p}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            Ok(_) => {}
        }
    }

    // What the previous round's review panel rejected. Recovered from the artifacts on
    // disk rather than carried in memory: the round after a gate — the one a human just
    // paid for — is a fresh process, and an in-memory list would leave it running blind
    // on the most expensive round of the run. Nothing in the implementer's prompt tells
    // it to go read `review-*.md` on its own.
    let mut blockers: Vec<Blocker> = blockers_from_disk(
        state,
        paths,
        &crate::rules::unwaived(&frozen.criteria, &crate::rules::for_run(paths, &state.id)),
        cfg,
    );

    loop {
        // Set when the implementer is killed at its hard ceiling with work left
        // behind: the leftovers are committed as a partial WIP round and the loop
        // goes on to the suite and the panel instead of failing.
        // Per-round state: a later fix round sets it again or leaves it empty.
        let mut ceiling_partial_note: Option<String> = None;
        // Stop boundary: before the implementer (and every fix-round re-dispatch).
        if should_stop(paths, &state.id) {
            return stop_now(state, paths);
        }
        state.set_phase(Phase::Dispatch);
        // Consumed before the save: a crash between the save and the take
        // would otherwise leave the arm on disk for an entry that never saw
        // the flag.
        let panel_direct = take_panel_rerun(state);
        state.save(paths)?;

        // Round 2+ opens by merging the base forward, after the previous
        // round's leftovers were committed (#229). A conflicted merge stays in
        // progress and is handed to this round's implementer via its prompt.
        // A `--re-review` pass sends the current HEAD straight to the panel
        // unchanged (O180), so it merges nothing: the next implementer round
        // re-merges whatever moved meanwhile.
        let merge_conflict_section = if panel_direct {
            None
        } else {
            match merge_base_at_round_start(state, paths, cfg) {
                Ok(section) => section,
                Err(e) => return fail(state, paths, e, false),
            }
        };

        if let Some(id) = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .map(|s| s.id.clone())
        {
            if let SeatCheck::Park(cause, msg) = recheck_seat(state, paths, cfg, &id, false)? {
                return fail(state, paths, anyhow::anyhow!(msg), cause.quota_hit());
            }
        }

        // Re-resolve implementer each iteration (stable id; provider may have rotated).
        let impl_slot = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no implementer slot"))?;

        if let Some(s) = state.slot_mut(&impl_slot.id) {
            s.status = SlotStatus::Pending;
            s.error = None;
        }

        // `--re-review`: the operator judged the blocking review wrong, so this
        // pass sends the current HEAD straight to the suite and panel below
        // without dispatching the implementer. One-shot: the arm was consumed
        // at the top of the loop, and a still-blocking panel re-arms the
        // normal stall cycle.
        if panel_direct {
            let head = worktree::git_out(&impl_cwd(state, &impl_slot), &["rev-parse", "HEAD"])
                .unwrap_or_default();
            let _ = crate::events::append(
                paths,
                &state.id,
                &crate::events::Event::info(format!(
                    "operator override (--re-review): skipping the implementer dispatch, \
                     sending HEAD `{head}` straight to the suite and panel"
                )),
            );
            state.save(paths)?;
        }
        // Set when the pass below records a ceiling partial for this round, so
        // the normal-finish clear after the dispatch does not erase it.
        let mut partial_recorded = false;
        if !panel_direct {
            let brief = take_carry_forward(paths, &state.id, &impl_slot.id);
            let persisted =
                crate::bus::persisted_for(paths, &state.id, &impl_slot.id).unwrap_or_default();
            let unique = crate::bus::agent_ref(Some(&state.id), &impl_slot.id);
            for m in &persisted {
                for event in [
                    crate::bus::ReceiptEvent::Delivered,
                    crate::bus::ReceiptEvent::Read,
                ] {
                    crate::bus::record_receipt(paths, &m.id, &unique, event, "carry_forward");
                }
            }
            let persisted: Vec<String> = persisted
                .iter()
                .map(|m| format!("from {} (message {}): {}", m.from, m.id, m.body))
                .collect();
            let mut extra = HashMap::new();
            extra.insert("plan_body".into(), plan_body.clone());
            extra.insert("test_contract_body".into(), test_contract_body.clone());
            extra.insert("amendment_section".into(), amendment_section.clone());
            extra.insert(
                "no_progress_warning".into(),
                state.no_progress_warning.clone().unwrap_or_default(),
            );
            extra.insert(
                "merge_conflict_section".into(),
                merge_conflict_section
                    .clone()
                    .map(|(section, _)| section)
                    .unwrap_or_default(),
            );
            extra.insert(
                "carry_forward_section".into(),
                carry_forward_section(
                    state.round,
                    &persisted,
                    &blockers,
                    brief.as_deref(),
                    cfg.rounds.carry_forward_chars,
                ),
            );
            let impl_model = impl_slot.model.clone();
            let impl_job = SlotJob {
                slot_id: impl_slot.id.clone(),
                provider: impl_slot.provider.clone(),
                role: SlotRole::Implementer,
                template: "implementer".into(),
                extra_vars: extra.clone(),
                expected_artifact: Some(format!("summary-{}.md", impl_slot.id)),
                model: impl_model,
                effort: None,
            };
            if let Err(e) = executor::run_slot(state, paths, cfg, &impl_job) {
                // A hard-ceiling kill is reported as a typed cause on the slot, never
                // matched from the error prose. With work left behind the round goes on:
                // the leftovers are committed as WIP below and judged like any round.
                // With no work at all the round fails as before, and a WIP commit the
                // hooks refuse fails the round too: the panel must never judge a HEAD
                // without the round's work.
                // Quota wins over the ceiling: a dispatch that hit both parks with
                // exit 4 instead of going to review on partial work, which could end
                // the run at 2 or 0. Exit codes are a public contract.
                match record_ceiling_partial_unless_quota(
                    state,
                    paths,
                    &impl_job.slot_id,
                    &mut ceiling_partial_note,
                )? {
                    CeilingPartialOutcome::Banked => partial_recorded = true,
                    // A refused WIP leaves HEAD without the round's work: fail
                    // with exit 1 rather than let the panel judge it.
                    CeilingPartialOutcome::Refused(msg) => {
                        return fail(state, paths, anyhow::anyhow!(msg), false);
                    }
                    CeilingPartialOutcome::Absent => {
                        let quota_hit = executor::slot_parks_on_quota(state, &impl_job.slot_id);
                        if quota_hit {
                            if let Some(backup_raw) =
                                crate::backup::backup_for_role(SlotRole::Implementer, 0, cfg)
                            {
                                let store =
                                    crate::quota::QuotaStore::load(paths).unwrap_or_default();
                                let health = providers::LiveHealth::for_dry(state.dry_run);
                                let slot_state = state
                                    .slots
                                    .iter()
                                    .find(|s| s.id == impl_job.slot_id)
                                    .cloned()
                                    .unwrap();
                                let cause = crate::backup::dispatch_stop_cause(
                                    &slot_state,
                                    &store,
                                    state.recorded_route(),
                                    providers::is_provider_present(
                                        &slot_state.provider,
                                        state.dry_run,
                                    ),
                                );
                                if cause.engages_backup()
                                    && !slot_state.source.is_some_and(SeatSource::is_backup)
                                {
                                    let backup_key =
                                        crate::provider_ref::ProviderRef::parse(&backup_raw)
                                            .map(|r| r.storage_key())
                                            .unwrap_or_else(|_| backup_raw.clone());
                                    let cur_key = crate::provider_ref::ProviderRef::parse(
                                        &impl_slot.provider,
                                    )
                                    .map(|r| r.storage_key())
                                    .unwrap_or_else(|_| impl_slot.provider.clone());
                                    if backup_key != cur_key {
                                        if let Some(source) = crate::backup::takeover(
                                            &backup_raw,
                                            SlotRole::Implementer,
                                            cfg,
                                            &store,
                                            paths,
                                            state.dry_run,
                                            state.dry_run,
                                            state.recorded_route(),
                                            &health,
                                        ) {
                                            crate::backup::engage(
                                                state,
                                                paths,
                                                &impl_job.slot_id,
                                                &backup_raw,
                                                source,
                                            )?;
                                            // Retry the implementer with backup in the same round
                                            let retry_impl_slot = state
                                                .slots
                                                .iter()
                                                .find(|s| s.id == impl_job.slot_id)
                                                .cloned()
                                                .unwrap();
                                            let retry_job = SlotJob {
                                                slot_id: retry_impl_slot.id.clone(),
                                                provider: retry_impl_slot.provider.clone(),
                                                role: SlotRole::Implementer,
                                                template: "implementer".into(),
                                                extra_vars: extra.clone(),
                                                expected_artifact: Some(format!(
                                                    "summary-{}.md",
                                                    retry_impl_slot.id
                                                )),
                                                model: retry_impl_slot.model.clone(),
                                                effort: None,
                                            };
                                            if let Err(e2) =
                                                executor::run_slot(state, paths, cfg, &retry_job)
                                            {
                                                if !handle_retry_err(
                                                    state,
                                                    paths,
                                                    &retry_job.slot_id,
                                                    e2,
                                                    &mut ceiling_partial_note,
                                                )? {
                                                    return Ok(());
                                                }
                                                partial_recorded = true;
                                            }
                                            // Retry succeeded, continue to review phase
                                        } else {
                                            return fail(state, paths, e, quota_hit);
                                        }
                                    } else {
                                        return fail(state, paths, e, quota_hit);
                                    }
                                } else {
                                    return fail(state, paths, e, quota_hit);
                                }
                            } else {
                                return fail(state, paths, e, quota_hit);
                            }
                        } else {
                            let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                            let health = providers::LiveHealth::for_dry(state.dry_run);
                            let slot_state = state
                                .slots
                                .iter()
                                .find(|s| s.id == impl_job.slot_id)
                                .cloned()
                                .unwrap();
                            let cause = crate::backup::dispatch_stop_cause(
                                &slot_state,
                                &store,
                                state.recorded_route(),
                                providers::is_provider_present(&slot_state.provider, state.dry_run),
                            );
                            if cause.engages_backup()
                                && !slot_state.source.is_some_and(SeatSource::is_backup)
                            {
                                if let Some(backup_raw) =
                                    crate::backup::backup_for_role(SlotRole::Implementer, 0, cfg)
                                {
                                    if let Some(source) = crate::backup::takeover(
                                        &backup_raw,
                                        SlotRole::Implementer,
                                        cfg,
                                        &store,
                                        paths,
                                        state.dry_run,
                                        state.dry_run,
                                        state.recorded_route(),
                                        &health,
                                    ) {
                                        let backup_key =
                                            crate::provider_ref::ProviderRef::parse(&backup_raw)
                                                .map(|r| r.storage_key())
                                                .unwrap_or_else(|_| backup_raw.clone());
                                        let cur_key = crate::provider_ref::ProviderRef::parse(
                                            &impl_slot.provider,
                                        )
                                        .map(|r| r.storage_key())
                                        .unwrap_or_else(|_| impl_slot.provider.clone());
                                        if backup_key != cur_key {
                                            crate::backup::engage(
                                                state,
                                                paths,
                                                &impl_job.slot_id,
                                                &backup_raw,
                                                source,
                                            )?;
                                            let retry_impl_slot = state
                                                .slots
                                                .iter()
                                                .find(|s| s.id == impl_job.slot_id)
                                                .cloned()
                                                .unwrap();
                                            let retry_job = SlotJob {
                                                slot_id: retry_impl_slot.id.clone(),
                                                provider: retry_impl_slot.provider.clone(),
                                                role: SlotRole::Implementer,
                                                template: "implementer".into(),
                                                extra_vars: extra.clone(),
                                                expected_artifact: Some(format!(
                                                    "summary-{}.md",
                                                    retry_impl_slot.id
                                                )),
                                                model: retry_impl_slot.model.clone(),
                                                effort: None,
                                            };
                                            if let Err(e2) =
                                                executor::run_slot(state, paths, cfg, &retry_job)
                                            {
                                                if !handle_retry_err(
                                                    state,
                                                    paths,
                                                    &retry_job.slot_id,
                                                    e2,
                                                    &mut ceiling_partial_note,
                                                )? {
                                                    return Ok(());
                                                }
                                                partial_recorded = true;
                                            }
                                        } else {
                                            return fail(state, paths, e, quota_hit);
                                        }
                                    } else {
                                        return fail(state, paths, e, quota_hit);
                                    }
                                } else {
                                    return fail(state, paths, e, quota_hit);
                                }
                            } else {
                                return fail(state, paths, e, quota_hit);
                            }
                        }
                    }
                }
            }
        }

        // The implementer finished without a ceiling kill, so this round is not
        // cut short even if an earlier same-round attempt was: drop its mark
        // before review. A just-recorded partial sets the flag below instead. A
        // `--re-review` pass dispatched no implementer, so it finished nothing.
        if !partial_recorded && !panel_direct {
            clear_finished_round_partial_mark(state, paths)?;
        }

        // Refresh implementer cwd after run (worktree may have been set at prepare).
        let impl_slot = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .cloned()
            .unwrap();
        let review_cwd = impl_cwd(state, &impl_slot);

        // The implementer burned a round without moving HEAD or the tree while a
        // still-blocking review against that same commit is open: a blocking
        // verdict, or an approval with a failing acceptance criterion. Do not pay
        // for another full review of that commit: re-dispatch once with a warning at
        // the top of the prompt, and park at the human gate when the next round is
        // empty too. Any round that moves HEAD or dirties the tree resets both the
        // count and the warning and flows to the suite and panel below.
        let criteria =
            crate::rules::unwaived(&frozen.criteria, &crate::rules::for_run(paths, &state.id));
        // A direct-to-panel pass neither counts nor warns: it already sent this
        // HEAD to the panel without dispatching the implementer.
        let stalled = if panel_direct {
            None
        } else {
            detect_no_progress(
                state,
                paths,
                cfg,
                &criteria,
                &review_cwd,
                impl_slot.dispatch_head.as_deref(),
            )
        };
        if let Some(stalled) = stalled {
            state.no_progress_rounds += 1;
            if state.no_progress_rounds >= 2 {
                return park_no_progress(state, paths, &stalled);
            }
            let open: Vec<&str> = stalled.reviews.iter().map(|r| r.slot_id.as_str()).collect();
            let note = format!(
                "no-progress dispatch {}: HEAD `{}` unchanged with {open:?} still blocking; \
                 re-dispatching the implementer with a warning instead of a full review",
                state.round, stalled.head,
            );
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(note));
            state.no_progress_warning = Some(stalled.prompt());
            state.save(paths)?;
            continue;
        }
        if state.no_progress_rounds > 0 || state.no_progress_warning.is_some() {
            state.no_progress_rounds = 0;
            state.no_progress_warning = None;
            state.panel_rerun = false;
            state.save(paths)?;
        }

        // Stop boundary: before the suite job.
        if should_stop(paths, &state.id) {
            return stop_now(state, paths);
        }
        if let Some(note) = commit_before_review(state, paths, cfg, &impl_slot.id) {
            ceiling_partial_note = Some(note);
        }
        // The panel judges a checkout of HEAD, so a round whose base merge is
        // still open, or whose merge was aborted instead of resolved, must fail
        // here rather than go to review. A resolution the implementer committed
        // advances `merged_base` to the pending tip (#229).
        let impl_workdir = impl_cwd(state, &impl_slot);
        let pending_tip = merge_conflict_section.as_ref().map(|(_, tip)| tip.as_str());
        match pre_review_merge_verdict(
            &impl_workdir,
            state.base_ref.as_deref().unwrap_or("branch"),
            pending_tip,
        ) {
            Ok(Some(tip)) => {
                state.merged_base = Some(tip.clone());
                state.save(paths)?;
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(format!(
                        "implementer resolved the base {} merge ({}); merged_base now {tip}",
                        state.base_ref.as_deref().unwrap_or("branch"),
                        tip.chars().take(8).collect::<String>(),
                    )),
                );
            }
            Ok(None) => {}
            Err(msg) => return fail(state, paths, anyhow::anyhow!(msg), false),
        }

        // Suite channel: cheap model runs full suites; reviewers must not re-run them.
        let mut suite_body = String::new();
        let mut suite_outcome = SuiteOutcome::Pass;
        let suite_channel_active = cfg.suite.enabled;
        if cfg.suite.enabled {
            let tester = state
                .slots
                .iter()
                .find(|s| s.role == SlotRole::Tester)
                .cloned();
            // A tester seat with no usable provider parks the run: the suite
            // is a singular gate. `recheck_seat` moves it onto an eligible
            // backup when it has one (O153).
            let tester = match tester {
                Some(t) => match recheck_seat(state, paths, cfg, &t.id, false)? {
                    SeatCheck::Ready(id) => state.slots.iter().find(|s| s.id == id).cloned(),
                    SeatCheck::Park(cause, msg) => {
                        return fail(state, paths, anyhow::anyhow!(msg), cause.quota_hit())
                    }
                },
                None => None,
            };
            if cfg.suite.is_builtin() {
                state.set_phase(Phase::Suite);
                state.save(paths)?;
                let report = if state.dry_run {
                    crate::suite::dry(&cfg.suite.command)
                } else {
                    let run_id = state.id.clone();
                    crate::suite::run(&crate::suite::Options {
                        cwd: &review_cwd,
                        commands: &cfg.suite.command,
                        log_path: &paths.artifact(&state.id, "suite.log"),
                        // The ceiling, not the soft number: `suite.timeout_secs` is the
                        // tester slot's nudge threshold and its kill sits at
                        // `hard_ceiling_multiple` past it (O50/O51). There is nothing to
                        // nudge here, so the knob keeps its meaning by running to the
                        // same wall.
                        budget: executor::hard_ceiling_for_role(cfg, SlotRole::Tester),
                        // These commands compile and run code a model just wrote, so they
                        // get the run's confinement like every other spawn in the tree.
                        isolation: state.isolation,
                        on_pid: &|pid| match pid {
                            Some(pid) => {
                                let _ = crate::markers::write_pid(
                                    paths,
                                    &run_id,
                                    crate::state::BUILTIN_SUITE_PID_ID,
                                    crate::process::PidToken::capture(pid),
                                );
                            }
                            None => crate::markers::clear_pid(
                                paths,
                                &run_id,
                                crate::state::BUILTIN_SUITE_PID_ID,
                            ),
                        },
                        stop: &|| should_stop(paths, &run_id),
                    })
                };
                suite_outcome = report.outcome;
                suite_body = report.body;
                std::fs::write(paths.artifact(&state.id, "suite.md"), &suite_body)?;
                let msg = format!(
                    "suite channel {} (built-in, {} command(s))",
                    match suite_outcome {
                        SuiteOutcome::Pass => "green",
                        SuiteOutcome::Fail => "red",
                        SuiteOutcome::Inconclusive => "inconclusive",
                    },
                    report.runs.len()
                );
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(msg.clone()),
                );
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    crate::bus::ORCHESTRATOR_SEAT,
                    msg,
                    state.message_budget,
                );
            } else if let Some(tester) = tester {
                state.set_phase(Phase::Suite);
                state.save(paths)?;
                if let Some(s) = state.slot_mut(&tester.id) {
                    s.status = SlotStatus::Pending;
                    s.cwd = Some(review_cwd.clone());
                    s.error = None;
                }
                let suite_path = paths.artifact(&state.id, "suite.md");
                let _ = std::fs::remove_file(&suite_path);
                let _ = std::fs::remove_file(
                    paths
                        .markers_dir(&state.id)
                        .join(format!("{}.done", tester.id)),
                );
                let _ = std::fs::remove_file(
                    paths
                        .markers_dir(&state.id)
                        .join(format!("{}.failed", tester.id)),
                );
                let suite_job = SlotJob {
                    slot_id: tester.id.clone(),
                    provider: tester.provider.clone(),
                    role: SlotRole::Tester,
                    template: "tester".into(),
                    extra_vars: HashMap::new(),
                    expected_artifact: Some("suite.md".into()),
                    model: tester.model.clone(),
                    effort: None,
                };
                let suite_ok = executor::run_slot(state, paths, cfg, &suite_job).is_ok();
                if !suite_ok && executor::slot_parks_on_quota(state, &tester.id) {
                    // A rate-limited tester must park the run, not read as an
                    // inconclusive suite that burns a fix round against the same
                    // paused provider.
                    return fail(
                        state,
                        paths,
                        anyhow::anyhow!("tester slot `{}` rate-limited", tester.id),
                        true,
                    );
                }
                // Absence is meaningful: a missing suite.md is Inconclusive, never a synthesized fail.
                let body_opt = std::fs::read_to_string(&suite_path).ok();
                let (exit_code, signal) = state
                    .slots
                    .iter()
                    .find(|s| s.id == tester.id)
                    .map(|s| (s.exit_code, s.signal))
                    .unwrap_or((None, None));
                suite_outcome = derive_suite_outcome(suite_ok, exit_code, body_opt.as_deref());
                suite_body = body_opt.clone().unwrap_or_default();
                let msg = match suite_outcome {
                    SuiteOutcome::Pass => format!("suite channel green (slot {})", tester.id),
                    SuiteOutcome::Fail => format!("suite channel red (slot {})", tester.id),
                    SuiteOutcome::Inconclusive => {
                        let reason = suite_inconclusive_reason(
                            suite_ok,
                            exit_code,
                            signal,
                            body_opt.as_deref(),
                        );
                        format!("suite channel inconclusive (slot {}): {reason}", tester.id)
                    }
                };
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    crate::bus::ORCHESTRATOR_SEAT,
                    msg,
                    state.message_budget,
                );
            } else {
                suite_outcome = SuiteOutcome::Inconclusive;
                suite_body = "## Summary\nsuite.enabled but no tester slot was prepared\n".into();
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    crate::bus::ORCHESTRATOR_SEAT,
                    "suite channel inconclusive: no tester slot prepared".to_string(),
                    state.message_budget,
                );
            }
            state.suite_outcome = Some(suite_outcome);

            let unreferenced = unreferenced_test_files(
                &review_cwd,
                state
                    .merged_base
                    .as_deref()
                    .or(state.base_commit.as_deref()),
                &suite_body,
            );
            if !unreferenced.is_empty() {
                let note = format!(
                    "suite gate does not reference {} added test file(s): {}",
                    unreferenced.len(),
                    unreferenced.join(", ")
                );
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(note.clone()),
                );
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    crate::bus::ORCHESTRATOR_SEAT,
                    note.clone(),
                    state.message_budget,
                );
                // Into the report the reviewers read: the gate itself cannot be trusted to
                // notice a target it never compiled, but a reviewer can. Written to the
                // artifact too, not just the prompt — `suite.md` is what a human opens.
                let warning = format!(
                    "\n## Coverage warning (orchestrator)\nThe suite commands above select \
                     specific targets and name none of these added test files:\n{}\n\nTreat the \
                     suite result as not covering them.\n",
                    unreferenced
                        .iter()
                        .map(|f| format!("- {f}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                suite_body.push_str(&warning);
                let suite_path = paths.artifact(&state.id, "suite.md");
                if suite_path.is_file() {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&suite_path) {
                        let _ = f.write_all(warning.as_bytes());
                    }
                }
            }
        }

        let suite_guidance = if suite_channel_active {
            suite_guidance(suite_outcome)
        } else {
            "## Tests\nYou may run targeted or full suites as needed for confidence. Prefer evidence over claims.\n".to_string()
        };

        state.set_phase(Phase::Review);
        state.save(paths)?;

        let reviewers: Vec<_> = state
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Reviewer)
            .cloned()
            .collect();

        // Detected from disk, against the frozen body, never against `test_contract_body`
        // (which carries the overlay note appended above). Loud, not blocking: the gate
        // still judges the frozen criteria, and spar never rewrites the artifact. A read
        // error (the file removed mid-round) is the loudest possible tamper and must not
        // go silent just because there is nothing to diff against; it is only skipped
        // when the freeze itself had no real criteria (`[spec] enabled = false`), since
        // there is nothing an operator needs to be told about a placeholder contract.
        let mut contract_drift_note = String::new();
        let drift_reason =
            match std::fs::read_to_string(paths.artifact(&state.id, "test-contract.md")) {
                Ok(on_disk) if frozen.drifted(&on_disk) => Some("changed".to_string()),
                Ok(_) => None,
                Err(_) if !frozen.criteria.is_empty() => Some("could not be re-read".to_string()),
                Err(_) => None,
            };
        if let Some(reason) = drift_reason {
            state.contract_modified = true;
            let msg = format!(
                "test-contract.md {reason} after the freeze (write window held by \
                 implementer slot `{}`); the acceptance gate still judges the frozen \
                 contract",
                impl_slot.id
            );
            eprintln!("warning: {msg}");
            let _ =
                crate::events::append(paths, &state.id, &crate::events::Event::info(msg.clone()));
            let _ = crate::bus::broadcast(
                paths,
                Some(&state.id),
                crate::bus::ORCHESTRATOR_SEAT,
                msg.clone(),
                state.message_budget,
            );
            contract_drift_note = format!("\n**Contract drift detected.** {msg}\n");
            state.save(paths)?;
        }

        let mut any_request_changes = suite_channel_active && suite_blocks_ship(suite_outcome);
        blockers.clear();
        if any_request_changes {
            blockers.push(Blocker::Note(format!(
                "suite channel {}: see {}",
                match suite_outcome {
                    SuiteOutcome::Fail => "red",
                    _ => "inconclusive",
                },
                paths.artifact(&state.id, "suite.md").display()
            )));
        }
        // One head per panel, resolved once after the suite ran: the commit the
        // panel was dispatched against is the contract every verdict is judged
        // against. A rotated re-dispatch inside the panel keeps the same head;
        // widened reviewers join a later round's panel with its freshly resolved
        // head. Unresolvable (review_cwd falls back to a possibly non-repo
        // project_root) fails OPEN, loudly: failing closed there would brick such
        // runs permanently with no reviewer fault.
        if let Some(wt) = state.worktrees.iter().find(|w| w.slot_id == impl_slot.id) {
            if let Some(msg) =
                worktree::restore_branch(&wt.path, &wt.branch, state.base_commit.as_deref())
            {
                eprintln!("warning: {msg}");
                let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            }
        }
        let panel_head = worktree::git_out(&review_cwd, &["rev-parse", "HEAD"]);
        if panel_head.is_none() {
            let msg = format!(
                "review panel sha check skipped: could not resolve HEAD in {}",
                review_cwd.display()
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
        }
        let scoped_findings = scoped_review_section(state, cfg, &review_cwd);
        // A ceiling-cut round reaches the panel on a partial WIP commit: the
        // reviewers judge what is here, and the note rides along so a missing
        // surface reads as unfinished work, not a regression.
        let prior_findings = match &ceiling_partial_note {
            Some(note) if scoped_findings.is_empty() => note.clone(),
            Some(note) => format!("{note}\n\n{scoped_findings}"),
            None => scoped_findings.clone(),
        };
        // Read per panel, so a waiver added while the run is live counts from the next
        // gate on. The contract stays frozen (O43); the waiver is the operator's (O121).
        let rulings = crate::rules::for_run(paths, &state.id);
        let criteria = crate::rules::unwaived(&frozen.criteria, &rulings);
        for (id, r) in crate::rules::waived(&rulings) {
            if !frozen.criteria.iter().any(|c| c == id) {
                continue;
            }
            let msg = format!("{id} waived by operator ruling {}: {}", r.id, r.text);
            eprintln!("note: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
        }
        // A reviewer seat with no usable provider is stood down, not parked: the
        // panel keeps running its other seats. Only a panel with no usable
        // seat left parks the run (O153).
        let mut stood_down: Vec<(ParkCause, String)> = Vec::new();
        for rev in &reviewers {
            // Stop boundary: before each reviewer job.
            if should_stop(paths, &state.id) {
                return stop_now(state, paths);
            }
            let rev_id = match recheck_seat(state, paths, cfg, &rev.id, false)? {
                SeatCheck::Ready(id) => id,
                SeatCheck::Park(cause, msg) => {
                    let note = format!("reviewer seat `{}` stood down: {msg}", rev.id);
                    eprintln!("warning: {note}");
                    let _ =
                        crate::events::append(paths, &state.id, &crate::events::Event::info(note));
                    if let Some(s) = state.slot_mut(&rev.id) {
                        s.stood_down = Some(msg.clone());
                    }
                    state.save(paths)?;
                    stood_down.push((cause, msg));
                    continue;
                }
            };
            let mut rev = state
                .slots
                .iter()
                .find(|s| s.id == rev_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("reviewer `{rev_id}` vanished"))?;
            let rev_cwd = reviewer_cwd(
                state,
                paths,
                cfg,
                &rev.id,
                panel_head.as_deref(),
                &review_cwd,
            );
            if let Some(s) = state.slot_mut(&rev.id) {
                s.status = SlotStatus::Pending;
                s.cwd = Some(rev_cwd.clone());
            }
            let mut extra = HashMap::new();
            extra.insert("review_cwd".into(), rev_cwd.display().to_string());
            if !suite_body.is_empty() {
                extra.insert("suite_body".into(), suite_body.clone());
            }
            extra.insert("suite_guidance".into(), suite_guidance.clone());
            extra.insert("plan_body".into(), plan_body.clone());
            extra.insert("test_contract_body".into(), test_contract_body.clone());
            extra.insert("contract_drift_note".into(), contract_drift_note.clone());
            extra.insert("prior_findings".into(), prior_findings.clone());
            let mut job = SlotJob {
                slot_id: rev.id.clone(),
                provider: rev.provider.clone(),
                role: SlotRole::Reviewer,
                template: "reviewer".into(),
                extra_vars: extra,
                expected_artifact: Some(format!("review-{}.md", rev.id)),
                model: None,
                effort: None,
            };
            let mut review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
            if !review_ok {
                // Stop boundary: don't re-dispatch a killed reviewer as a "failure".
                if should_stop(paths, &state.id) {
                    return stop_now(state, paths);
                }
                let infra = state
                    .slots
                    .iter()
                    .find(|s| s.id == rev.id)
                    .and_then(reviewer_infra_fault);
                if infra.is_some() {
                    drop_salvaged_verdict(paths, &state.id, &rev.id);
                }
                // Re-point and re-run once before treating the failure as anything.
                if let Some(id) = try_rotate_reviewer_provider(
                    state,
                    paths,
                    &rev.id,
                    &review_cwd,
                    cfg,
                    infra.is_some(),
                    &providers::LiveHealth::for_dry(state.dry_run),
                )? {
                    let rev_cwd =
                        reviewer_cwd(state, paths, cfg, &id, panel_head.as_deref(), &review_cwd);
                    if let Some(s) = state.slot_mut(&id) {
                        s.status = SlotStatus::Pending;
                        s.error = None;
                        s.cwd = Some(rev_cwd.clone());
                    }
                    job.extra_vars
                        .insert("review_cwd".into(), rev_cwd.display().to_string());
                    rev = state
                        .slots
                        .iter()
                        .find(|s| s.id == id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("reviewer `{id}` vanished"))?;
                    job.slot_id = rev.id.clone();
                    job.provider = rev.provider.clone();
                    job.expected_artifact = Some(format!("review-{}.md", rev.id));
                    review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
                }
            }
            if !review_ok {
                let fault = state
                    .slots
                    .iter()
                    .find(|s| s.id == rev.id)
                    .and_then(reviewer_infra_fault);
                if let Some(reason) = fault {
                    // The panel could not answer, which says nothing about the work: no
                    // verdict is recorded and no fix round is spent (O103).
                    drop_salvaged_verdict(paths, &state.id, &rev.id);
                    let msg = format!(
                        "review slot `{}` could not run on {}: {reason}. the provider could not serve this reviewer; no verdict recorded",
                        rev.id, rev.provider
                    );
                    eprintln!("error: {msg}");
                    let _ = crate::events::append(
                        paths,
                        &state.id,
                        &crate::events::Event::slot_note(&rev.id, msg.clone()),
                    );
                    return fail(state, paths, anyhow::anyhow!(msg), true);
                }
            }

            let review_path = paths.artifact(&state.id, &format!("review-{}.md", rev.id));
            let review_text = std::fs::read_to_string(&review_path).ok();
            let missing_or_empty = review_text
                .as_ref()
                .map(|t| t.trim().is_empty())
                .unwrap_or(true);

            // Fail closed: failed slot or missing review artifact ⇒ treat as request_changes.
            // Timeout salvage may have already written a partial review-*.md.
            if !review_ok || missing_or_empty {
                any_request_changes = true;
                blockers.push(Blocker::Note(format!(
                    "review slot `{}` failed or produced no review",
                    rev.id
                )));
                if missing_or_empty {
                    let acceptance = if frozen.criteria.is_empty() {
                        String::new()
                    } else {
                        let lines: Vec<String> = frozen
                            .criteria
                            .iter()
                            .map(|id| format!("{id}: unverified — reviewer produced no review"))
                            .collect();
                        format!("## Acceptance\n{}\n\n", lines.join("\n"))
                    };
                    let _ = std::fs::write(
                        &review_path,
                        format!(
                            "{}\n## Verdict\nrequest_changes\n\n{acceptance}## Findings\n- severity: major — review slot `{}` failed or produced no artifact\n",
                            review_result::SYNTHETIC_REVIEW_MARKER,
                            rev.id
                        ),
                    );
                }
                // Whatever sits in a failed slot's artifact (an earlier round's stale
                // verdict, a partial review, even a copied marker) is not what the gate
                // counted: it counted spar's own stand-in, so that is what is recorded.
                let why = state
                    .slots
                    .iter()
                    .find(|s| s.id == rev.id)
                    .and_then(|s| s.error.clone())
                    .unwrap_or_else(|| "failed or produced no review".into());
                let res = review_result::ReviewResult {
                    verdict: Some(review_result::Verdict::RequestChanges),
                    findings: vec![review_result::Finding {
                        severity: review_result::Severity::Major,
                        file: None,
                        line: None,
                        text: format!("review slot `{}`: {why}", rev.id),
                        synthetic: true,
                        follow_up: false,
                    }],
                    synthetic: true,
                    ..Default::default()
                };
                review_result::record_review(state, paths, &rev.id, &res);
            } else if let Some(text) = review_text {
                // Fail closed: only an anchored `## Verdict` / approve clears the gate,
                // and every contract criterion must be reported as passing (O19/O20).
                let res = review_result::ReviewResult {
                    scoped: !scoped_findings.is_empty(),
                    ..review_result::parse_review(&text)
                };
                review_result::record_review(state, paths, &rev.id, &res);
                // The semantic layer above the mtime gate: a verdict that names no
                // commit, or one about a commit that is not the panel head, is not
                // a verdict — treated exactly like a missing review.
                match decide_reviewed_commit(panel_head.as_deref(), &res) {
                    ReviewedCommitDecision::Skipped => {}
                    ReviewedCommitDecision::Match => {
                        if let Some(s) = state.slot_mut(&rev.id) {
                            s.reviewed_commit = res.reviewed_commit.clone();
                        }
                        let _ = crate::events::append(
                            paths,
                            &state.id,
                            &crate::events::Event::slot_note(
                                &rev.id,
                                format!(
                                    "judged commit {}",
                                    res.reviewed_commit.as_deref().unwrap_or("?")
                                ),
                            ),
                        );
                    }
                    ReviewedCommitDecision::Absent => {
                        any_request_changes = true;
                        let head = panel_head.as_deref().unwrap_or("unknown");
                        blockers.push(Blocker::Note(format!(
                            "review `{}` names no commit (panel head {head})",
                            rev.id
                        )));
                        fail_review_slot_on_commit(
                            state,
                            paths,
                            &rev.id,
                            format!(
                                "review names no commit (panel head {head}); a verdict that names no commit cannot be counted"
                            ),
                        );
                    }
                    ReviewedCommitDecision::Mismatch => {
                        any_request_changes = true;
                        let judged = res.reviewed_commit.as_deref().unwrap_or("?");
                        let head = panel_head.as_deref().unwrap_or("unknown");
                        let mut blocker = format!(
                            "review `{}` judged {judged}, panel dispatched against {head}",
                            rev.id
                        );
                        // A mid-panel commit visible at a glance: current HEAD as a
                        // third value when it moved past the panel head.
                        if let Some(cur) = worktree::git_out(&review_cwd, &["rev-parse", "HEAD"]) {
                            if cur != head {
                                blocker.push_str(&format!(" [current HEAD {cur}]"));
                            }
                        }
                        blockers.push(Blocker::Note(blocker));
                        fail_review_slot_on_commit(
                            state,
                            paths,
                            &rev.id,
                            format!("review judged {judged}, panel dispatched against {head}"),
                        );
                    }
                }
                if res.blocks(cfg.review.block_on) {
                    any_request_changes = true;
                    blockers.push(Blocker::Verdict(verdict_blocker(
                        state,
                        &rev,
                        &res,
                        &acceptance_block_reasons(&criteria, &res, cfg),
                        &review_path,
                    )));
                } else if !res.approves() {
                    let _ = crate::events::append(
                        paths,
                        &state.id,
                        &crate::events::Event::slot_note(
                            &rev.id,
                            format!(
                                "request_changes counted as approve-with-notes: no finding at or above block_on = {}",
                                cfg.review.block_on.as_str()
                            ),
                        ),
                    );
                }
                if acceptance_blocks_ship(&criteria, &res, cfg) {
                    any_request_changes = true;
                    let reasons = acceptance_block_reasons(&criteria, &res, cfg);
                    // Into the next round's prompt, not only onto the bus: without this
                    // the fix round's implementer prompt is byte-identical to round 1's
                    // and never learns which criterion it failed. A review that already
                    // carries these lines in its own verdict bullet does not repeat them
                    // as separate bullets: one bullet per review keeps a multi-seat panel
                    // inside the carry-forward budget.
                    if !res.blocks(cfg.review.block_on) {
                        for r in &reasons {
                            blockers.push(Blocker::Note(format!(
                                "acceptance (review `{}`): {r}",
                                rev.id
                            )));
                        }
                    }
                    let _ = crate::bus::broadcast(
                        paths,
                        Some(&state.id),
                        crate::bus::ORCHESTRATOR_SEAT,
                        format!(
                            "acceptance gate blocked ship (review {}): {}",
                            rev.id,
                            reasons
                                .iter()
                                .map(|r| r.to_string())
                                .collect::<Vec<_>>()
                                .join("; ")
                        ),
                        state.message_budget,
                    );
                }
            }
        }
        // Every reviewer stood down: the panel has no usable seat. A panel
        // stood down for readiness (no healthy provider left for any
        // reviewer) fails naming the failed probes; a quota stand-down parks
        // the run exactly as a lone reviewer with no backup always has.
        if let Some((cause, messages)) = panel_stop(reviewers.len(), &stood_down) {
            return fail(state, paths, anyhow::anyhow!(messages), cause.quota_hit());
        }
        // No reviewer seats at all: the stood-down check above only fires on a
        // non-empty panel, so an emptied panel would fall through to the ship
        // confirm below. Fail closed: the run can never ship unreviewed.
        if reviewers.is_empty() {
            return fail(
                state,
                paths,
                anyhow::anyhow!(
                    "run {} has no reviewer seats; the run would reach the ship gate unreviewed",
                    state.id
                ),
                false,
            );
        }
        let carried =
            carried_veto_blocker_lines(&state.slots, panel_head.as_deref(), &criteria, cfg);
        if !carried.is_empty() {
            any_request_changes = true;
            blockers.extend(carried.into_iter().map(Blocker::Note));
        }
        // The next round continues from the partial WIP commit, so its
        // implementer prompt carries the cut-short notice as a blocker: without
        // this the fix round would start from partial work with no idea it is.
        if let Some(note) = ceiling_partial_note.clone() {
            blockers.push(Blocker::Note(note));
        }
        if !any_request_changes {
            write_impl_summary(state, paths)?;
            if state.big {
                if let Ok(mut g) = crate::tasks::TaskGraph::load(paths, &state.id) {
                    for t in g
                        .ready_wave()
                        .iter()
                        .map(|t| t.id.clone())
                        .collect::<Vec<_>>()
                    {
                        g.mark_done(&t);
                    }
                    // mark all done for dry/simple path after successful review
                    for t in &mut g.tasks {
                        t.status = crate::tasks::TaskStatus::Done;
                    }
                    let _ = g.save(paths);
                }
            }
            if cfg.auto_ship() && state.dry_run {
                state.gates.ship_confirmed = true;
                state.set_phase(Phase::Done);
            } else {
                state.set_phase(Phase::AwaitingShipConfirm);
            }
            state.save(paths)?;
            return Ok(());
        }

        state.fix_rounds += 1;
        if state.fix_rounds > state.max_fix_rounds {
            // Stuck policy: rotate implementer → widen reviewers → escalate. Resolved
            // *before* the round ceiling, so `stuck` (exit 3, "this cannot be fixed")
            // always beats the ceiling gate (exit 2, "this is costing too much"). The
            // other order let a run whose ladder was already exhausted present as a
            // question for the operator instead of a verdict.
            if !state.rotated_implementer
                && try_rotate_implementer(
                    state,
                    paths,
                    cfg,
                    &providers::LiveHealth::for_dry(state.dry_run),
                )?
            {
                state.rotated_implementer = true;
                state.fix_rounds = 0;
            } else if !state.widened_reviewers
                && try_widen_reviewers(
                    state,
                    paths,
                    &review_cwd,
                    cfg,
                    &providers::LiveHealth::for_dry(state.dry_run),
                )?
            {
                state.widened_reviewers = true;
                state.fix_rounds = 0;
            } else {
                state.set_phase(Phase::Stuck);
                state.error = Some("fix rounds exhausted; escalated".into());
                state.save(paths)?;
                write_stuck(paths, &state.id)?;
                return Ok(());
            }
        }
        // Every path that reaches here is about to buy another cold re-dispatch, the
        // ladder's own retries included, so the ceiling bounds all of them.
        apply_queued_round_extension(state, paths);
        if state.round_ceiling_reached() && escalate_round_ceiling(state, paths)? {
            return Ok(());
        }
        // A fix pass re-dispatches the implementer and the panel, so it is a round of
        // work in its own right (O45) — `fix_rounds` bounds the loop, `round` counts
        // what happened on the run.
        state.begin_round();
        state.set_phase(Phase::Fix);
        state.save(paths)?;
        let run_id = state.id.clone();
        crate::providers::delivery::refresh_run_poll_files(paths, &run_id);
    }
}

/// One still-blocking review against the unchanged HEAD, with the findings the
/// warning and the park reason name: a `request_changes` verdict, or an
/// approval with failing acceptance criteria.
struct NoProgressReview {
    slot_id: String,
    round: u32,
    reviewed_commit: String,
    acceptance: Vec<String>,
    findings: Vec<String>,
}

/// Operator override for the reviewer-is-wrong stall (O180): `--re-review` on
/// re-entry sends the unchanged HEAD back to the panel instead of dispatching
/// the implementer. Consumed at the top of the loop, and the stall counter
/// restarts from zero, so a still-blocking re-review parks on its own two
/// dispatches.
fn take_panel_rerun(state: &mut RunState) -> bool {
    if !state.panel_rerun {
        return false;
    }
    state.panel_rerun = false;
    state.no_progress_rounds = 0;
    state.no_progress_warning = None;
    true
}

/// The implementer ended a dispatch with HEAD and the tree unchanged while these
/// reviews still block that same commit.
struct NoProgress {
    head: String,
    reviews: Vec<NoProgressReview>,
}

impl NoProgress {
    /// Warning rendered at the very top of the re-dispatched implementer's prompt.
    fn prompt(&self) -> String {
        let mut out = format!(
            "## No-progress warning\n\n\
             Your previous dispatch left HEAD `{}` and the worktree \
             unchanged while the review below still blocks that \
             exact commit. This dispatch re-runs you instead of paying for \
             another full review of the same commit. Change the tree now: \
             address the open findings. A second consecutive unchanged dispatch \
             while a review still blocks it parks the run for the operator.\n",
            self.head
        );
        for r in &self.reviews {
            out.push_str(&format!(
                "\nUnaddressed review `{}` (round {}, Reviewed-Commit `{}`)",
                r.slot_id, r.round, r.reviewed_commit
            ));
            if !r.acceptance.is_empty() {
                out.push_str(&format!(": failing {}", r.acceptance.join(", ")));
            }
            out.push(':');
            if r.findings.is_empty() {
                out.push_str(
                    "\n- no findings recorded; the review blocks without naming a \
                     severable finding (acceptance criteria, an untagged item, or a \
                     bare verdict; see the carry-forward blockers).",
                );
            } else {
                for line in r.findings.iter().take(MAX_LISTED_FINDINGS) {
                    out.push_str(&format!("\n- {line}"));
                }
                let extra = r.findings.len().saturating_sub(MAX_LISTED_FINDINGS);
                if extra > 0 {
                    out.push_str(&format!(
                        "\n- (+{extra} more, see the carry-forward blockers)"
                    ));
                }
            }
        }
        out.push('\n');
        out
    }

    /// One-line-per-review summary for the park message and the escalation file.
    fn describe(&self) -> String {
        self.reviews
            .iter()
            .map(|r| {
                format!(
                    "review `{}` (round {}, Reviewed-Commit `{}`): {} open finding(s)",
                    r.slot_id,
                    r.round,
                    r.reviewed_commit,
                    r.findings.len()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// A dispatch that changed nothing while a still-blocking review against that same
/// HEAD is open: a blocking verdict, or an approval with a failing acceptance
/// criterion. Compares HEAD before and after the implementer dispatch and
/// requires a clean tree, so a dispatch that wrote files it could not commit still
/// counts as progress and flows to the panel. (The previous dispatch's
/// `commit_before_review` moved already-judged work into HEAD before this one
/// started.) A dry-run implementer moves no HEAD by design, so it counts as
/// progress unless the `SPAR_DRY_IMPL_NOOP` hook nooped this dispatch (only the
/// hook simulates a stall). The hook budgets dispatches, not rounds, so the
/// check reads the same run-dir counter file the dispatch just wrote.
/// Returns the open reviews driving the re-dispatch, or `None` when the dispatch
/// made progress or nothing blocks it.
fn detect_no_progress(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    criteria: &[String],
    cwd: &Path,
    start_head: Option<&str>,
) -> Option<NoProgress> {
    if state.dry_run && !crate::executor::dry_impl_noop(paths, &state.id) {
        return None;
    }
    let start = start_head?;
    let end = worktree::git_out(cwd, &["rev-parse", "HEAD"])?;
    if end != start {
        return None;
    }
    if !matches!(worktree::porcelain(cwd), Some(s) if s.is_empty()) {
        return None;
    }
    let panel = review_result::panel_before(&state.review_history, state.round);
    let reviews: Vec<NoProgressReview> = panel
        .iter()
        .filter(|e| {
            e.review.blocks(cfg.review.block_on) || acceptance_blocks_ship(criteria, &e.review, cfg)
        })
        .filter(|e| {
            e.review
                .reviewed_commit
                .as_deref()
                .is_some_and(|sha| review_result::reviewed_commit_matches(end.as_str(), sha))
        })
        .map(|e| NoProgressReview {
            slot_id: e.slot_id.clone(),
            round: e.round,
            reviewed_commit: e.review.reviewed_commit.clone().unwrap_or_default(),
            acceptance: failing_acceptance_ids(criteria, &e.review, cfg),
            findings: e
                .review
                .findings
                .iter()
                .filter(|f| !f.synthetic)
                .map(open_finding_line)
                .chain(
                    e.review
                        .untagged
                        .iter()
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| open_untagged_line(t)),
                )
                .collect(),
        })
        .collect();
    if reviews.is_empty() {
        return None;
    }
    Some(NoProgress { head: end, reviews })
}

/// Second consecutive no-progress dispatch: park at the round-extension gate (the
/// existing human gate, exit 2) instead of paying for another full review of a
/// commit the panel already judged. The message names the unaddressed findings so
/// the human knows what the implementer would not touch.
fn park_no_progress(state: &mut RunState, paths: &SparPaths, stalled: &NoProgress) -> Result<()> {
    // The panel never runs on a stalled round, so a reviewer paused after the
    // last panel never gets its seat check. Stand those seats down here so the
    // park records who cannot re-judge the carried veto.
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let paused: Vec<(String, String)> = stalled
        .reviews
        .iter()
        .filter_map(|r| {
            let slot = state.slots.iter().find(|s| s.id == r.slot_id)?;
            if slot.stood_down.is_some() {
                return None;
            }
            seat_quota_blocked(state, &store, slot).map(|key| (r.slot_id.clone(), key))
        })
        .collect();
    for (id, key) in paused {
        if let Some(slot) = state.slot_mut(&id) {
            slot.stood_down = Some(format!(
                "provider {key} is paused or on cooldown; stood down at the no-progress park \
                 without dispatch. resume with `spar provider resume {key}` or reassign the role"
            ));
        }
    }
    // Kept in state so a re-entry renders the warning on its first dispatch
    // instead of parking silently.
    state.no_progress_warning = Some(stalled.prompt());
    // At the ceiling a bare re-entry would hit the ceiling gate before the
    // stall logic, so the message names the flag that re-entry needs; the
    // reviewer-is-wrong exit needs it too.
    let at_ceiling = state.round_ceiling_reached();
    let reentry = if at_ceiling {
        format!(
            "`spar implement --run {} --max-rounds {}`",
            state.id,
            state.max_rounds + 2
        )
    } else {
        format!("`spar implement --run {}`", state.id)
    };
    let re_review = if at_ceiling {
        format!("`--re-review --max-rounds {}`", state.max_rounds + 2)
    } else {
        "`--re-review`".to_string()
    };
    let msg = format!(
        "no progress in two consecutive implementer dispatches: HEAD `{}` and the worktree \
         are unchanged and the blocking {} ({}). The run is parked \
         for a human: take it over by hand (`spar stop {}`) or change the tree and \
         re-enter with {reentry}; re-entering with nothing changed dispatches the \
         implementer once more with the warning and parks again with no panel. \
         If the review is wrong, rule it out with `spar rule add` and re-enter with \
         {re_review} to send the unchanged HEAD back to the panel.",
        stalled.head,
        if stalled.reviews.len() == 1 {
            "review still stands"
        } else {
            "reviews still stand"
        },
        stalled.describe(),
        state.id
    );
    state.set_phase(Phase::AwaitingRoundExtension);
    state.error = Some(msg.clone());
    state.save(paths)?;
    let path = paths.artifact(&state.id, "escalation.md");
    std::fs::write(
        &path,
        format!(
            "# Escalation: no progress\n\n{msg}\n\n{}\n",
            stalled.prompt()
        ),
    )?;
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg.clone()));
    let _ = crate::bus::broadcast(
        paths,
        Some(&state.id),
        "orchestrator",
        msg.clone(),
        state.message_budget,
    );
    eprintln!("{msg}");
    Ok(())
}

/// What committing an implementer worktree's leftovers onto its branch found.
enum LeftoverCommit {
    Committed(String),
    Clean,
    Refused(String),
}

/// Commit what the implementer left uncommitted, overlaid acceptance tests included,
/// on its branch before the suite and the panel run (O138): reviewers judge a detached
/// checkout of `HEAD`, the suite runs on that same commit, and `ship` pushes it. The
/// subject is the PR title, so it reads right as a squash subject. A failure (a hook
/// that refuses) is said out loud and the round goes on: the panel then judges `HEAD`.
/// `None` when there is no worktree to commit on: a dry run, a missing dir, or
/// `.spar` internals. The commit message stays with the caller; the guard set
/// does not drift between the review commit and the ceiling-kill WIP commit.
fn commit_branch_leftovers(
    state: &RunState,
    paths: &SparPaths,
    impl_id: &str,
    subject: &str,
    body: &str,
) -> Option<(String, LeftoverCommit)> {
    let wt = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == impl_id)?
        .clone();
    if state.dry_run || !wt.path.is_dir() || wt.path.starts_with(state.project_root.join(".spar")) {
        return None;
    }
    if let Some(msg) = worktree::restore_branch(&wt.path, &wt.branch, state.base_commit.as_deref())
    {
        eprintln!("warning: {msg}");
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    }
    let outcome = match worktree::commit_leftovers(&wt.path, subject, body) {
        Ok(Some(sha)) => LeftoverCommit::Committed(sha),
        Ok(None) => LeftoverCommit::Clean,
        Err(e) => LeftoverCommit::Refused(e),
    };
    Some((wt.branch, outcome))
}

/// Merge the run's base branch forward at the start of round 2 and later, after
/// the previous round's leftovers were committed (#229). Returns the prompt
/// section handing a conflicted merge to the round's implementer plus the
/// pending upstream tip the pre-review guard verifies a resolution against,
/// `None` when there is nothing to hand over. A clean merge proceeds silently
/// (one event); a fetch the base's remote refuses is not a round failure, so
/// the round works on and ship retries before pushing.
fn merge_base_at_round_start(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<Option<(String, String)>> {
    // A `--dry-run` preview never touches branches; the scenario harness opts back
    // in (`SPAR_MERGE_BASE_DRY_RUN=1`) after swapping a real worktree for the fake one.
    let merge_in_dry_run = crate::util::env_truthy("SPAR_MERGE_BASE_DRY_RUN");
    if (state.dry_run && !merge_in_dry_run) || !cfg.rounds.merge_base || state.round < 2 {
        return Ok(None);
    }
    let Some(impl_slot) = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .cloned()
    else {
        return Ok(None);
    };
    let cwd = impl_cwd(state, &impl_slot);
    if worktree::git_out(&cwd, &["rev-parse", "HEAD"]).is_none() {
        return Ok(None);
    }
    // A merge an earlier round left open is re-handed below, not committed over:
    // committing now would conclude it with the conflict markers as content.
    let resumed = worktree::merge_in_progress(&cwd);
    let dirty_tree =
        worktree::git_out(&cwd, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());
    if !resumed && dirty_tree {
        let sha = worktree::commit_leftovers(
            &cwd,
            "spar: leftover work before base merge",
            &format!("round {}", state.round),
        )
        .map_err(|e| anyhow::anyhow!("could not commit leftover work before merging the base forward: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!("could not commit leftover work before merging the base forward: tree refuses to commit")
        })?;
        let _ = crate::events::append(
            paths,
            &state.id,
            &crate::events::Event::info(format!(
                "committed leftover work as {} before merging the base forward",
                sha.chars().take(8).collect::<String>(),
            )),
        );
    }
    let base_ref = state.base_ref.clone().unwrap_or_default();
    match worktree::merge_base_forward(&cwd, state.base_ref.as_deref())? {
        worktree::BaseMerge::Clean { merged } => {
            state.merged_base = Some(merged.clone());
            state.save(paths)?;
            let _ = crate::events::append(
                paths,
                &state.id,
                &crate::events::Event::info(format!(
                    "merged base {base_ref} forward ({})",
                    merged.chars().take(8).collect::<String>(),
                )),
            );
            Ok(None)
        }
        worktree::BaseMerge::AlreadyCurrent { tip } => {
            state.merged_base = Some(tip.clone());
            state.save(paths)?;
            let _ = crate::events::append(
                paths,
                &state.id,
                &crate::events::Event::info(format!(
                    "base {base_ref} already current ({})",
                    tip.chars().take(8).collect::<String>(),
                )),
            );
            Ok(None)
        }
        worktree::BaseMerge::Skipped(skip) => {
            let _ = crate::events::append(
                paths,
                &state.id,
                &crate::events::Event::info(format!("skipping base forward merge: {skip}")),
            );
            Ok(None)
        }
        worktree::BaseMerge::Conflict { files, tip } => {
            let _ = crate::events::append(
                paths,
                &state.id,
                &crate::events::Event::info(format!(
                    "base {base_ref} merge conflicted ({}); handed to the round {} implementer",
                    files.join(", "),
                    state.round,
                )),
            );
            let _ = crate::bus::broadcast(
                paths,
                Some(&state.id),
                crate::bus::ORCHESTRATOR_SEAT,
                format!(
                    "base {base_ref} merge conflicted ({}); the round {} implementer resolves it first",
                    files.join(", "),
                    state.round,
                ),
                state.message_budget,
            );
            Ok(Some((base_merge_conflict_section(&base_ref, &files), tip)))
        }
    }
}

/// The prompt section heading a round whose base forward merge conflicted: the
/// merge is in progress in the implementer's worktree, so resolving it is the
/// round's first job, before any of the brief below.
fn base_merge_conflict_section(base_ref: &str, files: &[String]) -> String {
    let list = if files.is_empty() {
        "(`git status` names them)".to_string()
    } else {
        files
            .iter()
            .map(|f| format!("- `{f}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "## Base merge conflict — resolve this first\n\n\
         The run's base branch `{base_ref}` moved forward and merging it into your branch \
         conflicted. The merge is in progress in your worktree: resolve it before anything else.\n\n\
         {list}\n\n\
         Resolve each conflict, `git add` the files, and commit the resolution \
         (`git commit --no-edit` keeps the merge message). Do not `git merge --abort` unless \
         the base change genuinely reverts your work: the next round re-merges and the same \
         conflict returns.\n"
    )
}

/// What the implementer did with the base merge handed to its round: committed
/// a resolution containing the pending tip, aborted back to the unmerged
/// branch, or left the merge open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseMergeResolution {
    Resolved,
    Aborted,
    InProgress,
}

fn base_merge_resolution(cwd: &std::path::Path, tip: &str) -> BaseMergeResolution {
    if worktree::merge_in_progress(cwd) {
        BaseMergeResolution::InProgress
    } else if worktree::git_out(cwd, &["merge-base", "--is-ancestor", tip, "HEAD"]).is_some() {
        BaseMergeResolution::Resolved
    } else {
        BaseMergeResolution::Aborted
    }
}

/// The pre-review verdict on the base merge handed to this round: the pending
/// tip to record when the implementer committed a resolution, `None` when
/// there is nothing to verify, or the message the round fails with when the
/// merge is still open or was aborted instead (#229).
fn pre_review_merge_verdict(
    cwd: &std::path::Path,
    base: &str,
    pending: Option<&str>,
) -> Result<Option<String>, String> {
    let unresolved = || {
        let files = worktree::conflicted_files(cwd).join(", ");
        format!(
            "base {base} merge unresolved: the implementer did not commit a resolution \
             (conflicting files: {files}); resolve it in the implementer worktree and resume"
        )
    };
    match pending {
        Some(tip) => match base_merge_resolution(cwd, tip) {
            BaseMergeResolution::Resolved => Ok(Some(tip.to_string())),
            BaseMergeResolution::Aborted => Err(format!(
                "base {base} merge aborted: the implementer did not commit a resolution \
                 (the base is unmerged again); resolve it in the implementer worktree and resume"
            )),
            BaseMergeResolution::InProgress => Err(unresolved()),
        },
        None if worktree::merge_in_progress(cwd) => Err(unresolved()),
        None => Ok(None),
    }
}

/// The refreshed cut-short notice when the commit below banks a ceiling-cut
/// round's leftovers: the reviewers and the next round read this instead of
/// the kill-time note, so a Refused round whose work did land never claims it
/// is uncommitted. `None` when no notice needs refreshing.
fn commit_before_review(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    impl_id: &str,
) -> Option<String> {
    let subject = crate::ship::pr_title(state.task.as_deref(), cfg.ship.title_max);
    let body = format!(
        "spar: uncommitted implementer work before review round {}",
        state.round
    );
    let round = state.round;
    let (branch, outcome) = commit_branch_leftovers(state, paths, impl_id, &subject, &body)?;
    match outcome {
        LeftoverCommit::Committed(sha) => {
            let note = format!(
                "committed uncommitted implementer work on {branch} as {} before review round {round}",
                sha.chars().take(8).collect::<String>(),
            );
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(note));
            // A ceiling-cut round whose WIP commit the hooks refused banks here
            // instead: HEAD now carries the work, so the round reads as WIP
            // rather than Refused. Rounds without a partial mark leave none behind.
            if let Some(entry) = state
                .ceiling_partial_rounds
                .iter_mut()
                .find(|e| e.round == round)
            {
                entry.kind = CeilingPartialKind::Wip;
                entry.commit = Some(sha);
                let _ = state.save(paths);
                return Some(ceiling_partial_notice(round));
            }
            None
        }
        LeftoverCommit::Clean => None,
        LeftoverCommit::Refused(e) => {
            let msg = format!(
                "could not commit uncommitted implementer work before review round {round}: {e}; \
                 the panel judges HEAD without it"
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            None
        }
    }
}

/// What the reviewers and the next round are told when the implementer is cut
/// short at its hard ceiling: the commit under review is partial work, saved so
/// the round can be judged and continued rather than thrown away.
fn ceiling_partial_notice(round: u32) -> String {
    format!(
        "NOTE: the implementer pass for round {round} was cut short at its hard time \
         ceiling before it could finish, so the commit under review is partial work \
         (saved as a WIP commit). Judge what is here and continue it; unmet task scope \
         or acceptance criteria are still request_changes reasons, not an approved round."
    )
}

/// What the reviewers and the next round are told when the implementer is cut
/// short at its hard ceiling after committing its work: the commit under review
/// is partial work, but the tree was clean so no WIP commit was written.
fn ceiling_partial_clean_notice(round: u32) -> String {
    format!(
        "NOTE: the implementer pass for round {round} was cut short at its hard time \
         ceiling after committing its work; the tree was clean, so no WIP commit \
         was written. Judge what is here and continue it; unmet task scope \
         or acceptance criteria are still request_changes reasons, not an approved round."
    )
}

/// Why the round failed when the previous implementer was cut short at its hard
/// ceiling and its leftovers could not be committed: the panel judges a
/// checkout of HEAD, so the failure must never send it a HEAD without the
/// round's work. The live kill names the commit error; the failure tells the
/// operator what to do next.
fn ceiling_partial_refused_notice(round: u32, detail: Option<&str>) -> String {
    let cause = match detail {
        Some(e) => format!(" ({e})"),
        None => String::new(),
    };
    format!(
        "the implementer pass for round {round} was cut short at its hard time \
         ceiling and its partial work could not be committed{cause}; failing the round \
         instead of asking review to judge the HEAD commit without the round's work. \
         Fix the commit hook (or commit the leftover work by hand) and resume."
    )
}

/// What the next round's implementer is told after a Refused bank: no WIP
/// commit exists, so the blocker must not claim one, and the round's leftovers
/// are still uncommitted in the tree the next pass reuses.
fn ceiling_partial_refused_blocker(round: u32) -> String {
    format!(
        "NOTE: round {round}'s implementer was cut short at its hard ceiling and the \
         WIP commit was refused, so the round's partial work is still uncommitted in \
         the tree; start the next pass from that work, not from HEAD."
    )
}

/// Commit an implementer's leftovers after a hard-ceiling kill as a WIP round and
/// report the notice for the reviewers and the next round. `None` when the kill
/// was not at the ceiling, left no work at all, or the round's banked commit no
/// longer sits at HEAD: those rounds fail as before.
///
/// A resume that idles into a second ceiling kill returns the carried bank's
/// notice (a WIP or Clean one) instead of failing: the tree is clean and HEAD
/// is still the banked commit, so the banked partial is still there and the
/// run goes on like the first hit. This covers only a round that never reached
/// the gate (a quota park before review, a stop): when the bank's round was
/// rejected at the gate, an idle fix round fails instead of sending the same
/// HEAD back through the suite and the panel for another pass.
///
/// The message names the ceiling kill, so the WIP commit reads as an interrupted
/// round on the branch rather than a normal one. Like `commit_before_review`, a
/// commit the hooks refuse is said out loud. Unlike that path the round
/// cannot go on: without the WIP commit there is nothing for review to judge,
/// so the record site banks the Refused mark and fails the round instead.
fn try_commit_ceiling_partial(
    state: &mut RunState,
    paths: &SparPaths,
    impl_id: &str,
) -> Option<(String, CeilingPartialKind, Option<String>)> {
    if executor::slot_timeout_cause(state, impl_id) != Some(executor::TimeoutCause::HardCeiling) {
        return None;
    }
    let wt_path = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == impl_id)
        .map(|w| w.path.clone())?;
    // The round's start HEAD and tree, never the run base: from round 2 on the base
    // predates earlier rounds' commits, so the base always reads as work. The tree
    // snapshot matters too: round 1 starts dirty (the test author's files land
    // uncommitted) and a Refused round leaves leftovers, so a dirty tree alone
    // cannot mean this round did anything. Without a recorded start only a dirty
    // tree can be judged.
    let (since, since_tree) = state
        .slots
        .iter()
        .find(|s| s.id == impl_id)
        .map(|s| (s.dispatch_head.clone(), s.dispatch_tree.clone()))
        .unwrap_or_default();
    let round = state.round;
    // The latest banked partial at or before this round. A same-round resume
    // carries its own round's bank; a round opened by a resume after a park
    // carries the previous round's bank when the new round adds nothing
    // before its own ceiling kill. Whether the bank's round reached the gate
    // decides (checked below): any review_history entry for that round means
    // the gate saw it, so an idle fix round fails instead of sending an
    // already-rejected HEAD back through the suite and the panel. Only a
    // round that never reached review (a quota park before review, a stop)
    // carries its bank forward.
    let carried: Option<(u32, CeilingPartialKind, Option<String>)> =
        state.ceiling_bank_at_or_below(round);
    let banked: Option<String> = carried.as_ref().and_then(|(_, _, commit)| commit.clone());
    if !executor::dispatch_left_work(
        &wt_path,
        since.as_deref(),
        since_tree.as_deref(),
        banked.as_deref(),
    ) {
        // A resume that idles into a second ceiling kill: the tree is clean and
        // HEAD is still the carried banked commit, so there is no new work, but
        // the banked partial is still there. Go on like the first hit instead
        // of failing the round. The notice names the bank's round, since that
        // is the round whose work is under review. The HEAD check keeps the
        // notice honest: an agent that reset HEAD away from the bank before the
        // second kill no longer carries that commit, so the bank says nothing.
        // The bank's round reached the gate when review_history holds any
        // entry for it: a verdict, a red suite, a failed sha or acceptance
        // check, or a stood-down seat all open a fix round, so an idle fix
        // round fails instead of sending an already-rejected HEAD back for
        // another pass. No entry means the round never reached review (a
        // quota park, a stop): the bank was never judged, so the run goes on
        // with it. Treating a synthetic RequestChanges (a failed slot, no
        // artifact) as a gate rejection is conservative: no panel judged the
        // commit, but failing closed beats looping an unjudged HEAD.
        let (bank_round, bank_kind, commit) = carried.as_ref()?;
        let gate_saw_round = state.review_history.iter().any(|e| e.round == *bank_round);
        if gate_saw_round {
            return None;
        }
        let commit = commit.clone()?;
        if worktree::git_out(&wt_path, &["rev-parse", "HEAD"]).as_deref() != Some(commit.as_str()) {
            return None;
        }
        let notice = match bank_kind {
            CeilingPartialKind::Wip => ceiling_partial_notice(*bank_round),
            CeilingPartialKind::Clean => ceiling_partial_clean_notice(*bank_round),
            CeilingPartialKind::Refused => return None,
        };
        return Some((notice, *bank_kind, Some(commit)));
    }
    let subject =
        format!("chore(spar): partial work, implementer cut short at hard ceiling (round {round})");
    let body = format!(
        "spar: the round {round} implementer was killed at its hard time ceiling before it \
         could finish. Its leftovers are committed here so the suite and the review panel \
         judge what is on the branch, and the next round continues from this commit."
    );
    let (branch, outcome) = commit_branch_leftovers(state, paths, impl_id, &subject, &body)?;
    let (note, notice, kind, commit) = match outcome {
        LeftoverCommit::Committed(sha) => (
            format!(
                "committed partial round-{round} implementer work on {branch} as {} (ceiling kill WIP)",
                sha.chars().take(8).collect::<String>(),
            ),
            ceiling_partial_notice(round),
            CeilingPartialKind::Wip,
            Some(sha),
        ),
        LeftoverCommit::Clean => {
            let head = worktree::git_out(&wt_path, &["rev-parse", "HEAD"]);
            if banked
                .as_deref()
                .is_some_and(|sha| head.as_deref() == Some(sha))
            {
                return None;
            }
            (
                format!(
                    "ceiling kill left committed work on {branch} with a clean tree; round {round} goes on"
                ),
                ceiling_partial_clean_notice(round),
                CeilingPartialKind::Clean,
                head,
            )
        }
        LeftoverCommit::Refused(e) => {
            // No commit, so no review: the record site banks this as a Refused
            // mark for the next round's blockers and fails the round instead.
            // The hook's own reason rides in the notice, said out loud.
            return Some((
                ceiling_partial_refused_notice(round, Some(&e)),
                CeilingPartialKind::Refused,
                None,
            ));
        }
    };
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(note));
    Some((notice, kind, commit))
}

/// What a failed ceiling-cut dispatch decided: banked (review the recorded
/// partial), refused (the WIP commit never landed, so fail the round rather
/// than review a HEAD without the round's work), or absent (no ceiling
/// partial; the caller falls through to quota routing as before).
#[derive(Debug)]
enum CeilingPartialOutcome {
    Banked,
    Refused(String),
    Absent,
}

/// Bank a ceiling-cut partial for review unless the slot also hit quota, in
/// which case the run parks with exit 4 instead: a review on partial work
/// could end the run at 2 or 0, and exit codes are a public contract. A
/// refused WIP commit banks its Refused mark for the next round's blockers
/// and fails the round with exit 1: an approve must never ship a HEAD the
/// round's work never reached.
fn record_ceiling_partial_unless_quota(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    note_out: &mut Option<String>,
) -> Result<CeilingPartialOutcome> {
    if executor::slot_parks_on_quota(state, slot_id) {
        return Ok(CeilingPartialOutcome::Absent);
    }
    if let Some((note, kind, commit)) = try_commit_ceiling_partial(state, paths, slot_id) {
        record_ceiling_partial(
            state,
            paths,
            slot_id,
            CeilingPartialRecord {
                note: note.clone(),
                kind,
                commit,
            },
            note_out,
        )?;
        if kind == CeilingPartialKind::Refused {
            return Ok(CeilingPartialOutcome::Refused(note));
        }
        Ok(CeilingPartialOutcome::Banked)
    } else {
        Ok(CeilingPartialOutcome::Absent)
    }
}

/// A backup-retry implementer dispatch failed. A ceiling-cut partial round goes on
/// like the primary dispatch's, except a refused WIP commit, which fails the
/// round; anything else fails the run with its quota routing.
/// Returns whether the loop falls through to the review phase: a recorded ceiling
/// partial continues, while a park or hard failure already ended the loop inside
/// `fail` and the caller must return from the loop function.
fn handle_retry_err(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    err: anyhow::Error,
    note_out: &mut Option<String>,
) -> Result<bool> {
    match record_ceiling_partial_unless_quota(state, paths, slot_id, note_out)? {
        CeilingPartialOutcome::Banked => Ok(true),
        CeilingPartialOutcome::Refused(msg) => {
            fail(state, paths, anyhow::anyhow!(msg), false)?;
            Ok(false)
        }
        CeilingPartialOutcome::Absent => {
            let quota_hit = executor::slot_parks_on_quota(state, slot_id);
            fail(state, paths, err, quota_hit)?;
            Ok(false)
        }
    }
}

/// Drop this round's ceiling-cut mark when its implementer finishes without a
/// ceiling kill: the round is no longer cut short, so ship judges the real
/// summary and the next round gets no stale notice. Keyed on the dispatch, not
/// the verdict (a finished round the panel rejects still finished). Older
/// rounds keep their marks for the PR's Partial work section.
fn clear_finished_round_partial_mark(state: &mut RunState, paths: &SparPaths) -> Result<()> {
    let before = state.ceiling_partial_rounds.len();
    state
        .ceiling_partial_rounds
        .retain(|e| e.round != state.round);
    if state.ceiling_partial_rounds.len() != before {
        state.save(paths)?;
    }
    Ok(())
}

/// What a ceiling kill banks for the round: the notice and how the work was
/// left. Whether the bank announces itself derives from the kind at the record
/// site: a refused WIP fails the round instead of going on, so it banks
/// silently (no done verdict, no panel broadcast) rather than carrying a flag.
struct CeilingPartialRecord {
    note: String,
    kind: CeilingPartialKind,
    commit: Option<String>,
}

/// Record a ceiling-cut partial round: the killed slot reads done (the kill stays
/// in the event log, not as a failure string on a done slot) and the round is
/// remembered on the run so a fresh process rebuilds the cut-short notice from
/// disk for the next round's implementer. The kind records how the work was left
/// so the rebuilt notice words the WIP commit honestly.
fn record_ceiling_partial(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    record: CeilingPartialRecord,
    out: &mut Option<String>,
) -> Result<()> {
    let CeilingPartialRecord { note, kind, commit } = record;
    eprintln!("note: {note}");
    // Only a bank the round goes on announces itself: the slot reads done now,
    // so the executor's failed dispatch verdict for it is stale (verdict markers
    // only steer running slots, and nothing else reading them must see a failed
    // implementer for a round that went on). A refused WIP fails the round
    // instead of going on, so it banks the mark silently and the slot keeps its
    // failed verdict: no marker clear, no done verdict, no panel broadcast; the
    // failure message says it out loud.
    if kind != CeilingPartialKind::Refused {
        crate::markers::clear_slot(paths, &state.id, slot_id);
        if let Some(s) = state.slot_mut(slot_id) {
            s.status = SlotStatus::Done;
            s.error = None;
        }
        let _ = crate::events::append(
            paths,
            &state.id,
            &crate::events::Event::slot(slot_id, SlotStatus::Done),
        );
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot_note(slot_id, note.clone()),
    );
    if kind != CeilingPartialKind::Refused {
        let _ = crate::bus::broadcast(
            paths,
            Some(&state.id),
            "orchestrator",
            note.clone(),
            state.message_budget,
        );
    }
    if let Some(entry) = state
        .ceiling_partial_rounds
        .iter_mut()
        .find(|e| e.round == state.round)
    {
        entry.kind = kind;
        entry.commit = commit;
    } else {
        state.ceiling_partial_rounds.push(CeilingPartialRound {
            round: state.round,
            kind,
            commit,
        });
    }
    state.save(paths)?;
    *out = Some(note);
    Ok(())
}

/// The tree the implementer works in: its slot directory when set, else its
/// worktree record, else the project root. Shared by the round loop's start and
/// end so the no-progress check compares the same tree the slot just wrote.
fn impl_cwd(state: &RunState, impl_slot: &SlotState) -> PathBuf {
    impl_slot
        .cwd
        .clone()
        .or_else(|| {
            state
                .worktrees
                .iter()
                .find(|w| w.slot_id == impl_slot.id)
                .map(|w| w.path.clone())
        })
        .unwrap_or_else(|| state.project_root.clone())
}

/// The tree reviewer `seat` judges from: its own detached checkout at the panel head
/// (O138), so a reviewer that checks out or commits anything moves nothing the
/// implementer or `ship` reads (#158). The implementer's tree when there is no real
/// worktree to detach from (dry run, `isolation = "none"`, a head that did not resolve),
/// and, loudly, when the checkout cannot be made: `restore_branch` stays the net there.
fn reviewer_cwd(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    seat: &str,
    panel_head: Option<&str>,
    review_cwd: &Path,
) -> PathBuf {
    let Some(head) = panel_head else {
        return review_cwd.to_path_buf();
    };
    let real_worktree = state
        .worktrees
        .iter()
        .any(|w| w.path == review_cwd && !w.path.starts_with(state.project_root.join(".spar")));
    if state.dry_run || !real_worktree {
        return review_cwd.to_path_buf();
    }
    match worktree::reviewer_checkout(state, seat, head, &cfg.worktree.copy) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!(
                "reviewer `{seat}` judges from the implementer's worktree: its own checkout \
                 failed ({e:#})"
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            review_cwd.to_path_buf()
        }
    }
}

/// The panel position a reviewer seat id encodes (`review-1-cli-codex` is 1).
/// `None` for ids that spell none (`review-cli-grok-wide`). This parses what
/// the id spells, not membership: a pinned widen id (`review-3-wide`) still
/// parses to its number, so gate panel paths on [`is_panel_reviewer`] (and
/// index the config with live positions, not this number) first.
pub(crate) fn reviewer_ordinal(slot_id: &str) -> Option<usize> {
    slot_id
        .strip_prefix("review-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// A reviewer seat's position in the live panel in slot order: the index its
/// backup holds in `backups.reviewer`. Id-encoded ordinals go stale when a drop
/// retires a lower seat, so only the live position counts. Widened extras sit
/// past the panel and hold no backup-list position, so they report none.
fn panel_position(state: &RunState, slot_id: &str) -> Option<usize> {
    state
        .slots
        .iter()
        .filter(|s| is_panel_reviewer(s))
        .position(|s| s.id == slot_id)
}

/// The Nth live reviewer panel seat in slot order (never a widened extra).
/// Panel positions shift when a drop removes a lower seat: seats keep their ids
/// for the life of the run, so the ordinal baked into an id goes stale and only
/// the live position counts.
fn panel_seat_with_ordinal(state: &RunState, ordinal: usize) -> Option<String> {
    state
        .slots
        .iter()
        .filter(|s| is_panel_reviewer(s))
        .nth(ordinal)
        .map(|s| s.id.clone())
}

/// What a reviewer drop removes, computed without touching anything. Both the
/// dispatch (which applies it under the run lock) and `--validate-only` (which
/// folds it into the displayed fleet) plan from it. Survivors keep their ids:
/// nothing renumbers, and dropped ids retire so they are never reminted.
pub struct ReviewerDropPlan {
    pub removed: Vec<String>,
}

pub(crate) fn plan_reviewer_drop(
    state: &RunState,
    ordinals: &[usize],
) -> Result<Option<ReviewerDropPlan>> {
    let mut gone: Vec<usize> = ordinals.to_vec();
    gone.sort_unstable();
    gone.dedup();
    if gone.is_empty() {
        return Ok(None);
    }
    // Resolve each ordinal against the current panel. A missing ordinal means no
    // slot was ever seated for it (e.g. a plan continuation), so there is
    // nothing to reap.
    let mut removed: Vec<String> = Vec::new();
    for ordinal in &gone {
        if let Some(slot_id) = panel_seat_with_ordinal(state, *ordinal) {
            removed.push(slot_id);
        }
    }
    if removed.is_empty() {
        return Ok(None);
    }
    Ok(Some(ReviewerDropPlan { removed }))
}

/// Retire reviewer seats by panel position (`--without reviewer[N]`) in state
/// only: prune their review history so the ship gate and review history stop
/// counting that seat's open findings, drop their slots, and retire their ids
/// so no later seat ever reuses them. Survivors keep their ids. Returns the
/// removed slot ids. File deletion is separate (`delete_dropped_reviewer_files`)
/// so a dispatch can save before deleting: a failed save must never leave
/// slots dropped with the config still naming them, and a failed deletion must
/// never leave the config naming seats whose checkouts are gone.
pub(crate) fn retire_reviewer_seats(
    state: &mut RunState,
    ordinals: &[usize],
) -> Result<Vec<String>> {
    let Some(plan) = plan_reviewer_drop(state, ordinals)? else {
        return Ok(Vec::new());
    };
    let ReviewerDropPlan { removed } = plan;
    for id in &removed {
        retire_panel_seat(state, id);
    }
    Ok(removed)
}

/// Delete the checkouts, pid markers and verdict artifacts of retired reviewer
/// seats. Runs after the state saves, so a deletion failure leaves orphaned
/// files behind dropped seats, never seats behind a config that names them.
pub(crate) fn delete_dropped_reviewer_files(
    state: &RunState,
    paths: &SparPaths,
    removed: &[String],
) -> Result<()> {
    for id in removed {
        crate::worktree::remove_reviewer_checkout(state, id);
        crate::markers::clear_pid(paths, &state.id, id);
        for ext in ["md", "json"] {
            let path = paths.artifact(&state.id, &format!("review-{id}.{ext}"));
            if path.is_file() {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// Persist an amended snapshot and its state fail-safe: the snapshot stages to
/// a temp file first and renames into place only after the state save lands,
/// so a failed state save never leaves the frozen config narrowed while the
/// slots it narrowed are still live. Either failure removes the staged temp.
fn save_amended_pair(
    paths: &SparPaths,
    run_id: &str,
    cfg: &Config,
    state: &RunState,
) -> Result<()> {
    let staged = cfg.stage_snapshot(paths, run_id)?;
    state.save(paths).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    crate::config::Config::commit_snapshot(paths, run_id, &staged).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    Ok(())
}

/// Retire one live panel seat like a drop: its history is pruned now, its id
/// retires so it is never reminted, its files go after the save. Returns the id.
fn retire_panel_seat(state: &mut RunState, id: &str) -> String {
    state.review_history.retain(|e| e.slot_id != id);
    state.retired_seat_ids.insert(id.to_string());
    state.slots.retain(|s| s.id != id);
    id.to_string()
}

/// Whether a `--role reviewer=` pin reconciles live seats on a bound run. The
/// re-seat only runs where seats already exist to reconcile: a first dispatch
/// mints the panel once from the amended config, so running the re-seat there
/// too would mint every pinned seat twice under the same id.
fn bound_panel_resync_applies(state: &RunState, role: &[String], pin_len: usize) -> bool {
    crate::config::role_pins_reviewer(role)
        && pin_len > 0
        && state
            .slots
            .iter()
            .any(|s| s.role == SlotRole::Implementer || is_panel_reviewer(s))
}

/// Parse the frozen reviewer panel into pins. A malformed entry refuses the
/// amend rather than seating half a panel.
fn parse_reviewer_pin(panel: &[String]) -> Result<Vec<crate::provider_ref::Pin>> {
    panel
        .iter()
        .map(|raw| crate::provider_ref::Pin::parse(raw.as_str()))
        .collect()
}

/// Re-seat the live reviewer panel to a `--role reviewer=` pin, so the live
/// panel and the frozen config panel stay one fact. A seat already carrying
/// the pinned provider keeps its seat (and its history); a pinned provider
/// seated elsewhere in the panel swaps into position; anything else retires
/// like a drop and mints fresh at an id neither live nor retired. Live seats
/// past the pin's length retire the same way. Returns the retired ids, whose
/// files the caller deletes after its save lands.
fn resync_reviewer_panel_to_pin(
    state: &mut RunState,
    backups: &mut Vec<String>,
    pin: &[crate::provider_ref::Pin],
) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    // Seats already placed by an earlier pin entry: a repeated provider in
    // the pin mints a second seat instead of seating one seat twice.
    let mut placed: BTreeSet<String> = BTreeSet::new();
    for (position, want) in pin.iter().enumerate() {
        let panel: Vec<String> = state
            .slots
            .iter()
            .filter(|s| is_panel_reviewer(s))
            .map(|s| s.id.clone())
            .collect();
        let provider_of = |id: &str| {
            state
                .slots
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.provider.clone())
        };
        if let Some(id) = panel.get(position) {
            if provider_of(id).as_deref() == Some(want.provider.as_str())
                && placed.insert(id.clone())
            {
                if let Some(s) = state.slot_mut(id) {
                    s.reviewer_kind = Some(ReviewerSeatKind::Panel);
                }
                blank_spent_backup_entry(backups, position, &want.provider);
                continue;
            }
        }
        if let Some(other) = panel
            .iter()
            .find(|id| {
                provider_of(id).as_deref() == Some(want.provider.as_str()) && !placed.contains(*id)
            })
            .cloned()
        {
            let here = panel
                .get(position)
                .and_then(|id| state.slots.iter().position(|s| &s.id == id));
            let there = state.slots.iter().position(|s| s.id == other);
            if let (Some(a), Some(b)) = (here, there) {
                state.slots.swap(a, b);
                if let Some(s) = state.slot_mut(&other) {
                    s.reviewer_kind = Some(ReviewerSeatKind::Panel);
                }
                placed.insert(other);
                blank_spent_backup_entry(backups, position, &want.provider);
                continue;
            }
        }
        // The pinned provider is nowhere in the panel, or the only seat
        // carrying it is already placed: the seat here retires like a drop
        // and a fresh seat mints at an id neither live nor retired, so a
        // move can never land on a live or retired id.
        let at = panel
            .get(position)
            .and_then(|id| state.slots.iter().position(|s| &s.id == id));
        if let Some(id) = panel.get(position).cloned() {
            removed.push(retire_panel_seat(state, &id));
        }
        let mut ordinal = position;
        while {
            let id = format!(
                "review-{ordinal}-{}",
                crate::util::sanitize_slot(&want.provider)
            );
            state.slots.iter().any(|s| s.id == id) || state.retired_seat_ids.contains(&id)
        } {
            ordinal += 1;
        }
        let seat_id = format!(
            "review-{ordinal}-{}",
            crate::util::sanitize_slot(&want.provider)
        );
        let mut slot = executor::init_slot_model(
            &seat_id,
            want.display(),
            SlotRole::Reviewer,
            want.model.clone(),
        );
        slot.source = Some(SeatSource::CliRole);
        slot.reviewer_kind = Some(ReviewerSeatKind::Panel);
        match at {
            Some(i) if i <= state.slots.len() => state.slots.insert(i, slot),
            _ => state.slots.push(slot),
        }
        placed.insert(seat_id);
        blank_spent_backup_entry(backups, position, &want.provider);
    }
    while backups.last().is_some_and(|s| s.is_empty()) {
        backups.pop();
    }
    // Entries past the pin length sit under positions no seat can hold: the
    // live panel is exactly the pin's length after this function, so they
    // can never be read back.
    backups.truncate(pin.len());
    // Live seats past the pin's length retire like drops: their history is
    // pruned now, their ids retire, their files go after the save.
    let live_ids: Vec<String> = state
        .slots
        .iter()
        .filter(|s| is_panel_reviewer(s))
        .map(|s| s.id.clone())
        .collect();
    for id in live_ids.into_iter().skip(pin.len()) {
        removed.push(retire_panel_seat(state, &id));
    }
    Ok(removed)
}

/// Blank a same-position backup entry that now names the seat's own provider:
/// a seat serving its engaged backup is not governed by the config, the same
/// spent-backup rule drops apply.
fn blank_spent_backup_entry(backups: &mut [String], position: usize, provider: &str) {
    if let Some(entry) = backups.get_mut(position) {
        if !entry.is_empty() && backup_key(entry) == provider {
            entry.clear();
        }
    }
}

#[cfg(test)]
mod amended_pair_tests {
    use super::*;

    fn pair_run(id: &str) -> RunState {
        let mut st = RunState::new(
            id,
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        st.dry_run = true;
        st
    }

    fn staged_files(paths: &SparPaths, run_id: &str) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(paths.run_dir(run_id))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config.json.tmp."))
            })
            .collect()
    }

    #[test]
    fn pair_persists_both_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = pair_run("r-pair-ok");
        st.slots.push(executor::init_slot(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        save_amended_pair(&paths, "r-pair-ok", &Config::default(), &st).unwrap();
        assert!(paths.run_config_file("r-pair-ok").exists());
        assert!(staged_files(&paths, "r-pair-ok").is_empty());
    }

    #[test]
    fn pair_failure_leaves_no_staged_config_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(paths.run_dir("r-pair-bad")).unwrap();
        std::fs::create_dir(paths.state_file("r-pair-bad")).unwrap();
        let st = pair_run("r-pair-bad");
        save_amended_pair(&paths, "r-pair-bad", &Config::default(), &st)
            .expect_err("saving state into a directory must fail");
        assert!(!paths.run_config_file("r-pair-bad").exists());
        assert!(staged_files(&paths, "r-pair-bad").is_empty());
    }
}

#[cfg(test)]
mod resync_panel_tests {
    use super::*;

    fn resync_state(ids: &[(&str, &str)]) -> RunState {
        let mut state = RunState::new(
            "r-resync",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        for &(id, provider) in ids {
            state
                .slots
                .push(executor::init_slot(id, provider, SlotRole::Reviewer));
        }
        state
    }

    fn panel_ids(state: &RunState) -> Vec<String> {
        state
            .slots
            .iter()
            .filter(|s| is_panel_reviewer(s))
            .map(|s| s.id.clone())
            .collect()
    }

    #[test]
    fn swapped_pin_swaps_live_seats_into_position() {
        let mut state = resync_state(&[
            ("review-0-cli-claude", "cli:claude"),
            ("review-1-cli-grok", "cli:grok"),
        ]);
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:grok".to_string(), "cli:claude".to_string()];
        let removed = resync_reviewer_panel_to_pin(
            &mut state,
            &mut cfg.backups.reviewer,
            &parse_reviewer_pin(&cfg.roles.reviewer).unwrap(),
        )
        .unwrap();
        assert!(removed.is_empty());
        assert!(state.retired_seat_ids.is_empty());
        assert_eq!(
            panel_ids(&state),
            vec![
                "review-1-cli-grok".to_string(),
                "review-0-cli-claude".to_string()
            ]
        );
    }

    #[test]
    fn replaced_provider_retires_and_mints_fresh_at_position() {
        let mut state = resync_state(&[("review-0-cli-claude", "cli:claude")]);
        state
            .retired_seat_ids
            .insert("review-0-cli-codex".to_string());
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".to_string()];
        let removed = resync_reviewer_panel_to_pin(
            &mut state,
            &mut cfg.backups.reviewer,
            &parse_reviewer_pin(&cfg.roles.reviewer).unwrap(),
        )
        .unwrap();
        assert_eq!(removed, vec!["review-0-cli-claude".to_string()]);
        assert_eq!(panel_ids(&state), vec!["review-1-cli-codex".to_string()]);
        assert!(state.retired_seat_ids.contains("review-0-cli-claude"));
    }

    /// The re-seat runs wherever live seats exist to reconcile, and nowhere
    /// else: a first dispatch (no implementer, no panel) mints the panel once
    /// from the amended config, so the re-seat must not run there too.
    #[test]
    fn resync_applies_only_where_seats_exist() {
        let role = vec!["reviewer=cli:codex".to_string()];
        let bare: Vec<String> = Vec::new();
        let empty = resync_state(&[]);
        assert!(!bound_panel_resync_applies(&empty, &role, 1));
        assert!(!bound_panel_resync_applies(&empty, &bare, 0));
        let mut dispatched = resync_state(&[("review-0-cli-claude", "cli:claude")]);
        assert!(bound_panel_resync_applies(&dispatched, &role, 1));
        assert!(!bound_panel_resync_applies(&dispatched, &role, 0));
        assert!(!bound_panel_resync_applies(&dispatched, &bare, 1));
        dispatched.slots.push(executor::init_slot(
            "impl",
            "cli:claude",
            SlotRole::Implementer,
        ));
        assert!(bound_panel_resync_applies(&dispatched, &role, 1));
        let mut impl_only = resync_state(&[]);
        impl_only.slots.push(executor::init_slot(
            "impl",
            "cli:claude",
            SlotRole::Implementer,
        ));
        assert!(bound_panel_resync_applies(&impl_only, &role, 1));
    }

    #[test]
    fn repeated_pin_provider_mints_a_second_seat() {
        let mut state = resync_state(&[("review-0-cli-claude", "cli:claude")]);
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:claude".to_string(), "cli:claude".to_string()];
        let removed = resync_reviewer_panel_to_pin(
            &mut state,
            &mut cfg.backups.reviewer,
            &parse_reviewer_pin(&cfg.roles.reviewer).unwrap(),
        )
        .unwrap();
        assert!(removed.is_empty());
        assert_eq!(panel_ids(&state).len(), 2);
        let mut ids = panel_ids(&state);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 2);
    }
}

#[cfg(test)]
mod widen_id_tests {
    use super::*;

    /// A pinned widen after a drop must not remint a live `-wide` id: drops
    /// shrink `slots.len()`, so `review-{len}-wide` can already exist.
    #[test]
    fn pinned_widen_never_mints_an_id_a_live_seat_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r-wide",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.slots.push(executor::init_slot(
            "impl",
            "cli:grok",
            SlotRole::Implementer,
        ));
        state.slots.push(executor::init_slot(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        state.slots.push(executor::init_slot(
            "review-3-wide",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:claude".to_string()];
        cfg.cli_role_keys.insert(SlotRole::Reviewer);
        assert!(try_widen_reviewers(
            &mut state,
            &paths,
            tmp.path(),
            &cfg,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap());
        let mut ids: Vec<&str> = state.slots.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            total,
            "widen minted a duplicate seat id: {ids:?}"
        );
    }

    /// A retired `-wide` id is never reminted either, even when no live slot
    /// holds it: widening skips it the same way it skips live ids.
    #[test]
    fn pinned_widen_never_mints_a_retired_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = RunState::new(
            "r-wide",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.slots.push(executor::init_slot(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        state.retired_seat_ids.insert("review-1-wide".to_string());
        assert_eq!(next_wide_id(&state), "review-2-wide");
    }

    /// The unpinned mint enforces the same rule: a retired
    /// `review-{provider}-wide` id is not reminted, so widening passes rather
    /// than resurrecting a moved seat's old id.
    #[test]
    fn unpinned_widen_never_mints_a_retired_id() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r-wide",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.slots.push(executor::init_slot(
            "impl",
            "cli:muse",
            SlotRole::Implementer,
        ));
        state.slots.push(executor::init_slot(
            "review-0-cli-claude",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        state.providers = vec!["cli:claude".into(), "cli:codex".into()];
        state.pool_origin = PoolOrigin::CliProviders;
        state
            .retired_seat_ids
            .insert("review-cli-codex-wide".to_string());
        let cfg = Config::default();
        assert!(
            !try_widen_reviewers(
                &mut state,
                &paths,
                tmp.path(),
                &cfg,
                &crate::providers::StubHealth::allow_all(),
            )
            .unwrap(),
            "the retired wide id must not come back"
        );
        assert!(
            state.slots.iter().all(|s| s.id != "review-cli-codex-wide"),
            "no slot resurrects the retired id: {:?}",
            state.slots.iter().map(|s| &s.id).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod reviewer_drop_history_tests {
    use super::*;

    fn finding_body(text: &str) -> String {
        format!("## Verdict\nrequest_changes\n\n## Findings\n- severity: major - {text}\n")
    }

    fn seat_with_finding(
        state: &mut RunState,
        paths: &SparPaths,
        id: &str,
        provider: &str,
        finding: &str,
    ) {
        state
            .slots
            .push(executor::init_slot(id, provider, SlotRole::Reviewer));
        let review = review_result::parse_review(&finding_body(finding));
        review_result::record_review(state, paths, id, &review);
    }

    /// Sequential drops never renumber survivors: a seat keeps its id for the life
    /// of the run, the dropped id retires, and the survivor's findings stay
    /// pinned to its own id instead of vanishing with a later drop.
    #[test]
    fn sequential_drops_keep_stable_ids_and_the_survivors_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r-drops",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        seat_with_finding(
            &mut state,
            &paths,
            "review-0-cli-claude",
            "cli:claude",
            "claude finding",
        );
        seat_with_finding(
            &mut state,
            &paths,
            "review-1-cli-codex",
            "cli:codex",
            "codex one finding",
        );
        seat_with_finding(
            &mut state,
            &paths,
            "review-2-cli-codex",
            "cli:codex",
            "codex two finding",
        );

        let retired = retire_reviewer_seats(&mut state, &[0]).unwrap();
        delete_dropped_reviewer_files(&state, &paths, &retired).unwrap();
        let ids: Vec<&str> = state.slots.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["review-1-cli-codex", "review-2-cli-codex"]);
        assert!(
            state.retired_seat_ids.contains("review-0-cli-claude"),
            "the dropped id retires: {:?}",
            state.retired_seat_ids
        );
        let retired = retire_reviewer_seats(&mut state, &[1]).unwrap();
        delete_dropped_reviewer_files(&state, &paths, &retired).unwrap();

        let ids: Vec<&str> = state.slots.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["review-1-cli-codex"]);
        assert!(
            state.retired_seat_ids.contains("review-2-cli-codex"),
            "the second dropped id retires too: {:?}",
            state.retired_seat_ids
        );
        assert_eq!(state.review_history.len(), 1, "{:?}", state.review_history);
        let open = review_result::open_findings(&state, tmp.path());
        assert_eq!(open.len(), 1, "{open:?}");
        assert!(
            open[0].finding.text.contains("codex one finding"),
            "{open:?}"
        );
    }

    /// Dropping the middle seat prunes only its own entries: writer ids are
    /// never shared across seats, so the survivors keep both their ids and
    /// their findings.
    #[test]
    fn middle_drop_prunes_only_the_doomed_seats_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r-middle-drop",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        for (id, provider, finding) in [
            ("review-0-cli-claude", "cli:claude", "claude finding"),
            ("review-1-cli-codex", "cli:codex", "codex one finding"),
            ("review-2-cli-codex", "cli:codex", "codex two finding"),
        ] {
            seat_with_finding(&mut state, &paths, id, provider, finding);
        }

        let retired = retire_reviewer_seats(&mut state, &[1]).unwrap();
        delete_dropped_reviewer_files(&state, &paths, &retired).unwrap();

        let ids: Vec<&str> = state.slots.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["review-0-cli-claude", "review-2-cli-codex"]);
        let kept: Vec<&str> = state
            .review_history
            .iter()
            .map(|e| e.slot_id.as_str())
            .collect();
        assert_eq!(kept, ["review-0-cli-claude", "review-2-cli-codex"]);
        let open = review_result::open_findings(&state, tmp.path());
        assert_eq!(open.len(), 2, "{open:?}");
        assert!(
            open.iter().all(|o| !o.finding.text.contains("codex one")),
            "{open:?}"
        );
    }
}

/// Blank the same-position backup entries a surviving reviewer seat already serves:
/// the seat rotated to its backup, so the fallback is spent and keeping it would
/// fail backup-against-primary validation on reload. Runs on the post-drop live
/// panel positions. Entries are blanked, never removed: the list is positional
/// and deleting one would hand every higher seat its neighbour's backup. The
/// comparison is the validation's own storage-key equality, and trailing blanks
/// trim since an empty list is unset.
/// Returns whether anything was blanked.
pub fn disengage_spent_reviewer_backups(state: &RunState, backups: &mut Vec<String>) -> bool {
    let mut blanked = false;
    let panel: Vec<&SlotState> = state
        .slots
        .iter()
        .filter(|s| is_panel_reviewer(s))
        .collect();
    for (position, s) in panel.iter().enumerate() {
        if s.source != Some(SeatSource::Backup) {
            continue;
        }
        let Some(entry) = backups.get_mut(position).filter(|b| !b.is_empty()) else {
            continue;
        };
        if backup_key(entry) == backup_key(&s.provider) {
            entry.clear();
            blanked = true;
        }
    }
    while backups.last().is_some_and(|s| s.is_empty()) {
        backups.pop();
    }
    blanked
}

fn backup_key(raw: &str) -> String {
    crate::provider_ref::ProviderRef::parse(raw)
        .map(|r| r.storage_key())
        .unwrap_or_else(|_| raw.to_string())
}

/// Put another provider in a seat. A reviewer seat is renamed to name its new vendor
/// (O85); the implementer keeps its id because its worktree and branch are keyed on it.
/// Returns the seat's id after the move. The caller saves.
fn move_seat(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    pin: &crate::provider_ref::Pin,
    source: SeatSource,
) -> Result<String> {
    let new_id = stage_move(state, slot_id, pin, source)?;
    if new_id != slot_id {
        state.rename_slot(paths, slot_id, &new_id);
    }
    Ok(new_id)
}

/// The disk-free half of a seat move: guards, re-point and rename, the same
/// outcome `move_seat` leaves in memory. The amend preview runs this on its
/// throwaway clone so `--validate-only` reads the post-move panel the dispatch
/// re-seats, and a `--reassign` onto a live or retired id fails the preview
/// instead of the dispatch. A move onto a retired id fails closed: the id
/// would come back with the dropped seat's history pruned and its verdict
/// files deleted.
fn stage_move(
    state: &mut RunState,
    slot_id: &str,
    pin: &crate::provider_ref::Pin,
    source: SeatSource,
) -> Result<String> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.id == slot_id)
        .ok_or_else(|| anyhow::anyhow!("unknown slot {slot_id}"))?;
    let new_id = moved_seat_id(slot.role, reviewer_seat_kind(slot), slot_id, &pin.provider);
    if new_id != slot_id && state.slots.iter().any(|s| s.id == new_id) {
        anyhow::bail!("seat {new_id} is already live; refusing to move {slot_id} onto it");
    }
    if new_id != slot_id && state.retired_seat_ids.contains(&new_id) {
        anyhow::bail!("seat {new_id} is retired; refusing to remint it");
    }
    if let Some(s) = state.slot_mut(slot_id) {
        s.point_at(&pin.display());
        // A level the old CLI took may not exist on the new one; the dispatch then
        // resolves the role's level from config.
        if executor::check_slot_effort(&pin.display(), s.effort).is_err() {
            s.effort = None;
        }
        s.source = Some(source);
    }
    if new_id != slot_id {
        state.rename_slot_in_memory(slot_id, &new_id);
    }
    Ok(new_id)
}

/// The post-move state an amend preview judges: every resolved `--reassign`
/// applied in order to a clone, the way the dispatch applies them after the
/// preview passes, so ordinals, pins and the re-seat simulation all read the
/// seats the dispatch will actually amend.
pub(crate) fn simulate_moves(
    state: &RunState,
    moves: &[(String, crate::provider_ref::Pin)],
) -> Result<RunState> {
    let mut moved = state.clone();
    for (seat, pin) in moves {
        stage_move(&mut moved, seat, pin, SeatSource::Reassigned)?;
    }
    Ok(moved)
}

/// The id a seat takes when `provider` fills it: reviewer and tester ids name their
/// provider (O80), the implementer keeps `impl`. A widened extra keeps its
/// wideness: it holds no panel position, so no ordinal survives the move.
fn moved_seat_id(
    role: SlotRole,
    kind: Option<ReviewerSeatKind>,
    slot_id: &str,
    provider: &str,
) -> String {
    if kind == Some(ReviewerSeatKind::Wide) {
        return format!("review-{}-wide", sanitize_slot(provider));
    }
    match (role, reviewer_ordinal(slot_id)) {
        (SlotRole::Reviewer, Some(ordinal)) => {
            format!("review-{ordinal}-{}", sanitize_slot(provider))
        }
        (SlotRole::Tester, _) => format!("suite-{}", sanitize_slot(provider)),
        _ => slot_id.to_string(),
    }
}

fn move_seat_to_backup(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    backup_raw: &str,
    source: SeatSource,
) -> Result<String> {
    let pin = crate::provider_ref::Pin::parse(backup_raw)?;
    let id = move_seat(state, paths, slot_id, &pin, source)?;
    state.warn_unhealthy_pin(&pin.display(), Some(source));
    state.save(paths)?;
    Ok(id)
}

/// Why a failed reviewer dispatch is an infrastructure failure rather than a review
/// (#155, O103): the provider could not serve it (quota, auth), the CLI never started,
/// or it exited having run no tools, which on a native CLI means it never read the
/// diff. `None` for a reviewer that genuinely ran and still failed: that keeps the
/// fail-closed `request_changes` it always had. A kill (signal) is not infra either:
/// a hung CLI ran into its ceiling, and `stop` is handled before this is asked.
fn reviewer_infra_fault(slot: &SlotState) -> Option<String> {
    if slot.status != SlotStatus::Failed {
        return None;
    }
    if slot.quota_hit {
        return Some("rate-limited".into());
    }
    let err = slot.error.as_deref().unwrap_or("failed");
    if slot.auth_fault {
        return Some(format!("auth: {err}"));
    }
    if slot.exec_backend == Some(crate::provider_ref::ExecBackend::ApiSdk) || slot.signal.is_some()
    {
        return None;
    }
    match &slot.usage {
        None if slot.exit_code.is_none() => Some(format!("could not start: {err}")),
        Some(u) if u.tools == 0 => Some(format!("ran no tools (tools == 0): {err}")),
        _ => None,
    }
}

/// Remove the `request_changes` stub `salvage_expected_artifact` writes for a failed
/// reviewer, when the failure was the provider's: that stub is a verdict nobody gave.
/// Only the stub is removed; a review the slot wrote itself is left alone.
fn drop_salvaged_verdict(paths: &SparPaths, run_id: &str, slot_id: &str) {
    let path = paths.artifact(run_id, &format!("review-{slot_id}.md"));
    let is_stub =
        std::fs::read_to_string(&path).is_ok_and(|t| t.contains(executor::REVIEW_SALVAGE_FINDING));
    if is_stub {
        let _ = std::fs::remove_file(&path);
    }
}

enum SeatCheck {
    Ready(String),
    Park(ParkCause, String),
}

/// Why a seat stood down: a quota-benched seat parks the run (exit 4),
/// a seat with no healthy provider fails readiness (exit 1). Carried on
/// the variant from where the re-check decides, never re-probed after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParkCause {
    Quota,
    Readiness,
}

impl ParkCause {
    fn quota_hit(self) -> bool {
        matches!(self, ParkCause::Quota)
    }
}

/// Run-start quota gate: only the implementer must run next at run start, so
/// only it can park the run here. `recheck_seat` moves it onto an eligible
/// backup when it has one; a seat with no usable provider parks the run, and
/// the returned message names it (O153).
fn gate_implementer_seat(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<Option<String>> {
    let impl_id: Option<String> = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .map(|s| s.id.clone());
    match impl_id {
        None => Ok(None),
        Some(id) => match recheck_seat(state, paths, cfg, &id, true)? {
            SeatCheck::Ready(id) => {
                // Quota-clean but the CLI may still be missing: without a
                // backup to take over, the seat stays put.
                let has_backup = cfg.backups.implementer.is_some() || cfg.backups.tester.is_some();
                if !has_backup {
                    return Ok(None);
                }
                let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                let route = state.recorded_route();
                let missing = state
                    .slots
                    .iter()
                    .find(|s| s.id == id)
                    .filter(|s| {
                        !crate::backup::is_provider_eligible(
                            &s.provider_ref().display(),
                            &store,
                            route,
                        ) || !crate::providers::is_provider_present(&s.provider, state.dry_run)
                    })
                    .map(|s| s.provider.clone());
                match missing {
                    Some(provider) => Ok(Some(format!(
                        "provider for `{id}` not available ({provider}). \
                         install the CLI or set `--select`/`--providers` to a usable one"
                    ))),
                    None => Ok(None),
                }
            }
            SeatCheck::Park(_, msg) => Ok(Some(msg)),
        },
    }
}

/// Whether a seat's quota bucket holds it right now, without moving the seat.
/// The live branch of the re-check gate (O106): a live run trusts only pauses it
/// can trust, manual and strong, plus a weak pause written before this dispatch
/// started. The line is the dispatch start, not the run creation: one id threads
/// plan, implement, and every round (O45), so `created_at` long predates any
/// later round. Returns the quota key when blocked. `None` in dry runs, which
/// skip pause gating, as the run-start gate always has.
fn seat_quota_blocked(
    state: &RunState,
    store: &crate::quota::QuotaStore,
    slot: &SlotState,
) -> Option<String> {
    if state.dry_run {
        return None;
    }
    let seat = slot.provider_ref();
    let key = seat.quota_key(state.recorded_route());
    let horizon = state.round_started_at.unwrap_or(state.created_at);
    seat_dispatch_blocked(store, &key, horizon, false).then_some(key)
}

/// Re-read pause state right before a dispatch (#175, O106). Pauses used to be read
/// only when a run started or resumed, so `provider pause grok --until 2027-01-01` did
/// not stop a live run from dispatching its next grok reviewer. A seat whose provider
/// is no longer eligible moves to its declared backup, or the run parks.
/// What the next dispatch would do with one stored seat. `recheck_seat`
/// performs it; `--validate-only` shows it.
enum SlotNext {
    Keep,
    MoveTo { raw: String, source: SeatSource },
    Park(ParkCause, String),
    Fail(String),
}

// A start-of-run gate parks on any pause (O106: a weak pause still steers the
// next run's seat choice); a live re-check trusts only manual and strong pauses.
fn seat_dispatch_blocked(
    store: &crate::quota::QuotaStore,
    key: &str,
    horizon: chrono::DateTime<chrono::Utc>,
    strict: bool,
) -> bool {
    if strict {
        !store.is_usable(key)
    } else if store.blocks_live_dispatch(key) {
        true
    } else {
        !store.is_usable(key) && store.get(key).updated_at.is_none_or(|at| at < horizon)
    }
}

/// The quota-gated half of a re-check, without the move: a paused seat yields
/// to its configured backup, and a seat with no eligible backup parks.
#[allow(clippy::too_many_arguments)]
fn resolve_blocked_slot_next(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    slot: &SlotState,
    store: &crate::quota::QuotaStore,
    health: &dyn crate::providers::HealthSource,
    route: crate::providers::RecordedRoute<'_>,
    probe_dry: bool,
    presence_dry: bool,
) -> Option<(String, SeatSource)> {
    let seat = slot.provider_ref();
    let sub = seat.storage_key();
    let ordinal = match slot.role {
        SlotRole::Reviewer => panel_position(state, &slot.id),
        _ => Some(0),
    };
    ordinal
        .and_then(|o| crate::backup::backup_for_role(slot.role, o, cfg))
        .filter(|b| crate::quota::sub_key(b) != sub)
        .and_then(|b| {
            crate::backup::takeover(
                &b,
                slot.role,
                cfg,
                store,
                paths,
                probe_dry,
                presence_dry,
                route,
                health,
            )
            .map(|source| (b, source))
        })
}

/// The health-gated half of a re-check, without the move: a seat whose
/// provider went unhealthy between dispatches re-resolves through the same
/// single point as launch (O171). Pins only warn; a seat with nothing healthy
/// left fails readiness (exit 1), never parks (exit 4).
#[allow(clippy::too_many_arguments)]
fn resolve_healthy_slot_next(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    slot: &SlotState,
    store: &crate::quota::QuotaStore,
    health: &dyn crate::providers::HealthSource,
    route: crate::providers::RecordedRoute<'_>,
    probe_dry: bool,
    presence_dry: bool,
    used: &[String],
) -> SlotNext {
    let (role, pool_idx, role_idx): (SlotRole, usize, usize) = match slot.role {
        SlotRole::Implementer => (SlotRole::Implementer, 0, 0),
        SlotRole::Reviewer => match panel_position(state, &slot.id) {
            Some(r) => (SlotRole::Reviewer, r + 1, r),
            None => return SlotNext::Keep,
        },
        SlotRole::Tester => (SlotRole::Tester, 0, 0),
        _ => return SlotNext::Keep,
    };
    if matches!(slot.status, SlotStatus::Running)
        || matches!(slot.source, Some(SeatSource::Reassigned))
        || slot.source.is_some_and(|s| s.is_explicit_pin())
    {
        return SlotNext::Keep;
    }
    // A seat whose provider is still healthy keeps it: re-resolution is
    // positional, so re-resolving a healthy seat would reshuffle a continuing
    // run onto different providers every round. Only a provider that went
    // unhealthy between dispatches goes back through the single point.
    if health.check(&slot.provider).usable {
        return SlotNext::Keep;
    }
    // Every role re-resolves through the same point as launch: a file seat on
    // a provider that went unhealthy yields to its backup, then the order rung.
    let fresh = crate::backup::resolve_with_backup(
        role,
        pool_idx,
        role_idx,
        &state.providers,
        state.pool_origin,
        cfg,
        slot.model.as_deref(),
        store,
        paths,
        probe_dry,
        presence_dry,
        route,
        health,
        used,
    );
    let Some((fresh_raw, fresh_src)) = fresh else {
        let skips = crate::workflow::roles_resolve::health_skips_for_seat(
            role,
            pool_idx,
            &state.providers,
            state.pool_origin,
            cfg,
            health,
        );
        // A reviewer with nothing healthy left stands down and the panel keeps
        // running, mirroring the launch narrowing. The probe detail travels
        // with the message so a panel with no usable seat left fails readiness
        // naming the failed probes, never a quota park.
        if role == SlotRole::Reviewer {
            let detail = match crate::backup::readiness_error(&skips) {
                Some(err) => format!("{err:#}"),
                None => format!("no provider resolved for `{}`", slot.id),
            };
            return SlotNext::Park(ParkCause::Readiness, detail);
        }
        return match crate::backup::readiness_error(&skips) {
            Some(err) => SlotNext::Fail(format!("{err:#}")),
            None => SlotNext::Fail(format!("no provider resolved for `{}`", slot.id)),
        };
    };
    if fresh_raw == slot.provider {
        return SlotNext::Keep;
    }
    SlotNext::MoveTo {
        raw: fresh_raw,
        source: fresh_src,
    }
}

fn recheck_seat(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    slot_id: &str,
    strict: bool,
) -> Result<SeatCheck> {
    let Some(slot) = state.slots.iter().find(|s| s.id == slot_id).cloned() else {
        return Ok(SeatCheck::Ready(slot_id.to_string()));
    };
    // Dry runs skip pause gating, as the run-start gate always has.
    if state.dry_run {
        return Ok(SeatCheck::Ready(slot_id.to_string()));
    }
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let key = slot.provider_ref().quota_key(state.recorded_route());
    // A live run trusts only pauses it can trust: manual and strong (O106). A
    // weak heuristic pause ("429" in "429 passed") written by this very dispatch
    // must not park it or reseat it, but a weak pause written before this
    // dispatch started steers the seat onto its backup, so the first dispatch
    // reads it strict while later dispatches stay trust-only. The line is the
    // dispatch start, not the run creation: one id threads plan, implement, and
    // every round (O45), so `created_at` long predates any later round.
    let horizon = state.round_started_at.unwrap_or(state.created_at);
    if !seat_dispatch_blocked(&store, &key, horizon, strict) {
        let health = providers::LiveHealth::for_dry(state.dry_run);
        return recheck_seat_health(state, paths, cfg, &slot, &health);
    }
    let health = providers::LiveHealth::for_dry(state.dry_run);
    let Some((backup_raw, source)) = resolve_blocked_slot_next(
        state,
        paths,
        cfg,
        &slot,
        &store,
        &health,
        state.recorded_route(),
        state.dry_run,
        state.dry_run,
    ) else {
        return Ok(SeatCheck::Park(
            ParkCause::Quota,
            no_backup_message(&key, slot_id),
        ));
    };
    let new_id = move_seat_to_backup(state, paths, slot_id, &backup_raw, source)?;
    let note = format!("{key} is paused; `{new_id}` moved to its backup {backup_raw}");
    eprintln!("note: {note}");
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot_note(&new_id, note),
    );
    Ok(SeatCheck::Ready(new_id))
}

fn recheck_seat_health(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    slot: &SlotState,
    health: &dyn crate::providers::HealthSource,
) -> anyhow::Result<SeatCheck> {
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    // Sibling seats hold their providers: a re-seated singleton must not land
    // on one a sibling still holds while a distinct usable one stands idle.
    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.id != slot.id)
        .map(|s| s.provider.clone())
        .collect();
    match resolve_healthy_slot_next(
        state,
        paths,
        cfg,
        slot,
        &store,
        health,
        state.recorded_route(),
        state.dry_run,
        state.dry_run,
        &used,
    ) {
        SlotNext::Keep => Ok(SeatCheck::Ready(slot.id.clone())),
        SlotNext::MoveTo { raw, source } => {
            let new_id = move_seat_to_backup(state, paths, &slot.id, &raw, source)?;
            Ok(SeatCheck::Ready(new_id))
        }
        SlotNext::Park(cause, detail) => Ok(SeatCheck::Park(cause, detail)),
        SlotNext::Fail(msg) => anyhow::bail!("{msg}"),
    }
}

fn no_backup_message(key: &str, slot_id: &str) -> String {
    format!(
        "provider {key} is paused or on cooldown and `{slot_id}` has no eligible backup. \
         resume with `spar provider resume {key}` or reassign the role"
    )
}

/// Where `--validate-only --run` stops when the projection fails: the cause
/// the dispatch would park or fail with, and the message it would print.
pub(crate) struct ProjectionStop {
    cause: ParkCause,
    message: String,
}

impl ProjectionStop {
    pub(crate) fn quota(&self) -> bool {
        self.cause.quota_hit()
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) struct ContinuedProjection {
    pub(crate) fleet: Vec<FleetSeat>,
    pub(crate) stop: Option<ProjectionStop>,
}

/// What the next dispatch would resolve for a continued run's stored seats,
/// without moving anything: the read-only mirror of the continue entry gate
/// plus the per-seat re-check. The dispatch stamps a fresh horizon and
/// re-stamps the ambient codex route before it re-checks anything, so the
/// projection resolves from those too — never the stale stored ones — and
/// never re-probes quota: a projection that wrote the store would steer the
/// dispatch it claims to preview.
pub(crate) fn project_continued_fleet(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    roles: &[SlotRole],
) -> ContinuedProjection {
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let health = providers::LiveHealth::for_dry(state.dry_run);
    let stored_fleet: Vec<FleetSeat> = state
        .slots
        .iter()
        .map(|slot| FleetSeat {
            seat: slot.id.clone(),
            role: slot.role,
            provider: slot.provider.clone(),
            model: slot.model.clone(),
            source: slot.source.unwrap_or(SeatSource::Unknown),
            projected: false,
        })
        .collect();
    if state.dry_run {
        return ContinuedProjection {
            fleet: stored_fleet,
            stop: None,
        };
    }
    let route_owned = crate::providers::codex::dispatch_route();
    let route = route_owned.as_recorded();
    let horizon = chrono::Utc::now();
    // Projected provider per seat, in dispatch order: the entry gate
    // re-checks the implementer first, so it picks before the reviewers do.
    let mut held: HashMap<String, String> = state
        .slots
        .iter()
        .map(|s| (s.id.clone(), s.provider.clone()))
        .collect();
    let mut fleet = Vec::with_capacity(state.slots.len());
    let mut stop: Option<ProjectionStop> = None;
    let mut stood_down: Vec<(ParkCause, String)> = Vec::new();
    let mut ordered: Vec<&SlotState> = state.slots.iter().collect();
    ordered.sort_by_key(|s| !matches!(s.role, SlotRole::Implementer));
    for slot in ordered {
        // Seats the next dispatch does not re-resolve (the other phase's
        // history) show stored: the dispatch leaves them untouched.
        if !roles.contains(&slot.role) {
            if let Some(stored) = stored_fleet.iter().find(|f| f.seat == slot.id) {
                fleet.push(stored.clone());
            }
            continue;
        }
        // The dispatch stops at the first parked or failed seat: later seats
        // never resolve, so they stay stored and unprojected.
        if stop.is_some() {
            if let Some(stored) = stored_fleet.iter().find(|f| f.seat == slot.id) {
                fleet.push(stored.clone());
            }
            continue;
        }
        let stored = FleetSeat {
            seat: slot.id.clone(),
            role: slot.role,
            provider: slot.provider.clone(),
            model: slot.model.clone(),
            source: slot.source.unwrap_or(SeatSource::Unknown),
            projected: true,
        };
        let used: Vec<String> = held
            .iter()
            .filter(|(id, _)| *id != &slot.id)
            .map(|(_, provider)| provider.clone())
            .collect();
        let key = slot.provider_ref().quota_key(route);
        // The entry gate re-checks only the implementer, and strict: a weak
        // pause predating this dispatch steers it onto its backup or parks it.
        let strict = matches!(slot.role, SlotRole::Implementer);
        if !seat_dispatch_blocked(&store, &key, horizon, strict) {
            match resolve_healthy_slot_next(
                state,
                paths,
                cfg,
                slot,
                &store,
                &health,
                route,
                true,
                state.dry_run,
                &used,
            ) {
                SlotNext::Keep => fleet.push(stored),
                SlotNext::MoveTo { raw, source } => {
                    if let Ok(raw_ref) = crate::provider_ref::ProviderRef::parse(&raw) {
                        held.insert(slot.id.clone(), raw_ref.display());
                    }
                    fleet.push(FleetSeat {
                        seat: moved_seat_id(slot.role, reviewer_seat_kind(slot), &slot.id, &raw),
                        provider: raw,
                        source,
                        projected: true,
                        ..stored
                    });
                }
                SlotNext::Park(cause, detail) => {
                    fleet.push(stored);
                    if matches!(slot.role, SlotRole::Reviewer) {
                        stood_down.push((cause, detail));
                    } else {
                        stop = Some(ProjectionStop {
                            cause,
                            message: detail,
                        });
                    }
                }
                SlotNext::Fail(message) => {
                    fleet.push(stored);
                    stop = Some(ProjectionStop {
                        cause: ParkCause::Readiness,
                        message,
                    });
                }
            }
            continue;
        }
        match resolve_blocked_slot_next(
            state,
            paths,
            cfg,
            slot,
            &store,
            &health,
            route,
            true,
            state.dry_run,
        ) {
            Some((raw, source)) => {
                if let Ok(raw_ref) = crate::provider_ref::ProviderRef::parse(&raw) {
                    held.insert(slot.id.clone(), raw_ref.display());
                }
                fleet.push(FleetSeat {
                    seat: moved_seat_id(slot.role, reviewer_seat_kind(slot), &slot.id, &raw),
                    provider: raw,
                    source,
                    projected: true,
                    ..stored
                });
            }
            None => {
                fleet.push(stored);
                let message = no_backup_message(&key, &slot.id);
                if matches!(slot.role, SlotRole::Reviewer) {
                    stood_down.push((ParkCause::Quota, message));
                } else {
                    stop = Some(ProjectionStop {
                        cause: ParkCause::Quota,
                        message,
                    });
                }
            }
        }
    }
    if stop.is_none() {
        let reviewers = state
            .slots
            .iter()
            .filter(|s| matches!(s.role, SlotRole::Reviewer))
            .count();
        // A panel with no usable seat left fails readiness naming the
        // failed probes, never a quota park (O171).
        stop = panel_stop(reviewers, &stood_down)
            .map(|(cause, message)| ProjectionStop { cause, message });
    }
    ContinuedProjection { fleet, stop }
}

/// Change implementer **provider** only; keep stable slot id and worktree.
fn try_rotate_implementer(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
) -> Result<bool> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .cloned();
    let Some(slot_state) = slot else {
        return Ok(false);
    };
    let cur = slot_state.provider.clone();
    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .map(|s| s.provider.clone())
        .collect();
    {
        let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        let has_backup = cfg.backups.implementer.is_some();
        let cause = crate::backup::dispatch_stop_cause(
            &slot_state,
            &store,
            state.recorded_route(),
            providers::is_provider_present(&slot_state.provider, state.dry_run),
        );
        if has_backup {
            if cause.engages_backup() {
                if let Some(backup_raw) =
                    crate::backup::backup_for_role(SlotRole::Implementer, 0, cfg)
                {
                    let backup_key = crate::provider_ref::ProviderRef::parse(&backup_raw)
                        .map(|r| r.storage_key())
                        .unwrap_or_else(|_| backup_raw.clone());
                    let used_contains_backup = used.iter().any(|u| {
                        crate::provider_ref::ProviderRef::parse(u)
                            .map(|r| r.storage_key())
                            .unwrap_or_else(|_| u.clone())
                            == backup_key
                    });
                    if !used_contains_backup
                        && !slot_state.source.is_some_and(SeatSource::is_backup)
                    {
                        if let Some(source) = crate::backup::takeover(
                            &backup_raw,
                            SlotRole::Implementer,
                            cfg,
                            &store,
                            paths,
                            state.dry_run,
                            state.dry_run,
                            state.recorded_route(),
                            health,
                        ) {
                            let impl_id = slot_state.id.clone();
                            crate::backup::engage(state, paths, &impl_id, &backup_raw, source)?;
                            return Ok(true);
                        }
                    }
                }
                return Ok(false);
            } else {
                return Ok(false);
            }
        }
    }
    // Candidate order: [roles].implementer, then [providers].order, then the live fleet.
    // Rotation must report the rung it actually drew from (`SlotState::point_at` only
    // touches provider/model), not leave the slot claiming its original source.
    let implementer_role_source = if cfg.cli_role_keys.contains(&SlotRole::Implementer) {
        SeatSource::CliRole
    } else {
        SeatSource::RolesFile
    };
    // The live-fleet leg resolves each pool position honestly through `resolve_seat`
    // instead of stamping every position with `state.pool_origin.as_seat_source()`: a
    // `Synth` pool mixes `[roles]` and `[providers].order` seats, and the blanket guess
    // mislabels whichever positions did not actually take that rung.
    let pool_sources = crate::workflow::roles_resolve::resolve_seat_sources(
        SlotRole::Implementer,
        state.providers.len(),
        &state.providers,
        state.pool_origin,
        cfg,
        health,
    );
    let next = cfg
        .roles
        .implementer
        .iter()
        .map(|s| (s.as_str(), implementer_role_source))
        .chain(
            cfg.providers
                .order
                .iter()
                .map(|s| (s.as_str(), SeatSource::ProvidersOrder)),
        )
        .chain(
            state
                .providers
                .iter()
                .map(|s| s.as_str())
                .zip(pool_sources.iter().copied()),
        )
        .find(|(p, src)| {
            *p != cur.as_str()
                && !used.iter().any(|u| u == p)
                && (src.is_explicit_pin() || health.check(p).usable)
        })
        .map(|(p, src)| (p.to_string(), src));
    let Some((next, source)) = next else {
        return Ok(false);
    };
    let impl_id = slot_state.id.clone();
    if let Some(s) = state.slot_mut(&impl_id) {
        s.point_at(&next);
        s.source = Some(source);
    }
    state.save(paths)?;
    Ok(true)
}

/// Add an extra adversarial reviewer from a provider not already reviewing.
/// The next free `review-{n}-wide` id. Drops shrink `slots.len()`, so the
/// length alone can remint an extra seat's live id.
fn next_wide_id(state: &RunState) -> String {
    let mut n = state.slots.len();
    loop {
        let id = format!("review-{n}-wide");
        // A retired id is never reminted, even when no live slot holds it.
        if state.slots.iter().all(|s| s.id != id) && !state.retired_seat_ids.contains(&id) {
            return id;
        }
        n += 1;
    }
}

fn try_widen_reviewers(
    state: &mut RunState,
    paths: &SparPaths,
    review_cwd: &std::path::Path,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
) -> Result<bool> {
    // A pinned panel (CLI `--role reviewer=…` or file `[roles].reviewer`) is an
    // exclusion list, not a floor: widening it must duplicate a pin the run actually
    // dispatched, never import a provider from `[providers].order` the operator
    // deliberately left out (Bug A). Compare by storage key so a `cli:codex@terra` pin
    // recognises the slot it created (`provider=cli:codex, model=terra`) as itself.
    if crate::workflow::roles_resolve::reviewer_panel_pinned(cfg) {
        let pins: Vec<String> = cfg
            .roles
            .reviewer
            .iter()
            .map(|p| {
                crate::provider_ref::ProviderRef::parse(p)
                    .map(|r| r.storage_key())
                    .unwrap_or_else(|_| p.clone())
            })
            .collect();
        let dispatched = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Reviewer && pins.iter().any(|p| p == &s.provider))
            .or_else(|| state.slots.iter().find(|s| s.role == SlotRole::Reviewer));
        let Some(src) = dispatched else {
            return Ok(false);
        };
        let (provider, model, source) = (src.provider.clone(), src.model.clone(), src.source);
        let id = next_wide_id(state);
        let mut slot = executor::init_slot_model(&id, &provider, SlotRole::Reviewer, model);
        slot.cwd = Some(review_cwd.to_path_buf());
        slot.source = source;
        slot.reviewer_kind = Some(ReviewerSeatKind::Wide);
        state.slots.push(slot);
        state.save(paths)?;
        return Ok(true);
    }
    let existing: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Reviewer)
        .map(|s| s.provider.clone())
        .collect();
    // Unpinned: draw the next reviewer from the run's own pool first — an explicit
    // --providers/--select still outranks [providers].order (O56) even on the widen
    // path — then fall to [providers].order once the pool is exhausted. Each pool
    // position resolves its honest source through `resolve_seat` rather than a blanket
    // `pool_origin.as_seat_source()` guess, since a `Synth` pool mixes `[roles]` and
    // `[providers].order` seats. Compare by storage key: a pool entry may still carry an
    // `@model` suffix while `existing`/`slot.provider` are always model-free, so a raw
    // string comparison would miss a provider that is already reviewing and add a third
    // seat on it instead of moving on.
    let normalize = |p: &str| {
        crate::provider_ref::ProviderRef::parse(p)
            .map(|r| r.storage_key())
            .unwrap_or_else(|_| p.to_string())
    };
    let pool_sources = crate::workflow::roles_resolve::resolve_seat_sources(
        SlotRole::Reviewer,
        state.providers.len(),
        &state.providers,
        state.pool_origin,
        cfg,
        health,
    );
    let candidate = state
        .providers
        .iter()
        .cloned()
        .zip(pool_sources)
        .chain(
            cfg.providers
                .order
                .iter()
                .cloned()
                .map(|p| (p, SeatSource::ProvidersOrder)),
        )
        .find(|(p, src)| {
            !existing.contains(&normalize(p)) && (src.is_explicit_pin() || health.check(p).usable)
        });
    let Some((prov, source)) = candidate else {
        // still widen with a synthetic extra reviewer on a repeated provider
        let dispatched = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Reviewer && existing.first() == Some(&s.provider));
        let prov = existing
            .first()
            .cloned()
            .unwrap_or_else(|| "cli:claude".into());
        let source = dispatched.and_then(|s| s.source);
        let id = next_wide_id(state);
        let mut slot = executor::init_slot(&id, &prov, SlotRole::Reviewer);
        slot.cwd = Some(review_cwd.to_path_buf());
        slot.source = source;
        slot.reviewer_kind = Some(ReviewerSeatKind::Wide);
        state.slots.push(slot);
        state.save(paths)?;
        return Ok(true);
    };
    let id = format!("review-{}-wide", sanitize_slot(&prov));
    // A retired id is never reminted, even when no live slot holds it.
    if state.slots.iter().any(|s| s.id == id) || state.retired_seat_ids.contains(&id) {
        return Ok(false);
    }
    let mut slot = executor::init_slot(&id, &prov, SlotRole::Reviewer);
    slot.cwd = Some(review_cwd.to_path_buf());
    slot.source = Some(source);
    slot.reviewer_kind = Some(ReviewerSeatKind::Wide);
    state.slots.push(slot);
    state.save(paths)?;
    Ok(true)
}

/// Re-point a failed reviewer seat for one more dispatch. Returns the seat's id after
/// the move (a backup renames it, O85), or `None` when there is nowhere to go.
///
/// `infra` is set when the failure was the provider's, not the review's (O103): then
/// the declared `--backup` for this panel position is tried first, the way every other
/// role's environmental stop already does. Every candidate must be eligible right now:
/// rotating onto a paused provider only buys the same failure again.
fn try_rotate_reviewer_provider(
    state: &mut RunState,
    paths: &SparPaths,
    rev_id: &str,
    review_cwd: &std::path::Path,
    cfg: &Config,
    infra: bool,
    health: &dyn crate::providers::HealthSource,
) -> Result<Option<String>> {
    let Some(slot) = state.slots.iter().find(|s| s.id == rev_id).cloned() else {
        return Ok(None);
    };
    let cur = slot.provider.clone();
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    if infra && !slot.source.is_some_and(SeatSource::is_backup) {
        // The seat's own backup: its live panel position, never the ordinal
        // baked into its id, which goes stale when a drop retires a lower seat.
        let backup = panel_position(state, rev_id)
            .and_then(|o| crate::backup::backup_for_role(SlotRole::Reviewer, o, cfg))
            .filter(|b| crate::quota::sub_key(b) != crate::quota::sub_key(&cur))
            .and_then(|b| {
                crate::backup::takeover(
                    &b,
                    SlotRole::Reviewer,
                    cfg,
                    &store,
                    paths,
                    state.dry_run,
                    state.dry_run,
                    state.recorded_route(),
                    health,
                )
                .map(|source| (b, source))
            });
        if let Some((raw, source)) = backup {
            let id = move_seat_to_backup(state, paths, rev_id, &raw, source)?;
            if let Some(s) = state.slot_mut(&id) {
                s.cwd = Some(review_cwd.to_path_buf());
            }
            state.save(paths)?;
            return Ok(Some(id));
        }
    }
    let eligible = |p: &String| -> bool {
        *p != cur && crate::backup::is_provider_eligible(p, &store, state.recorded_route())
    };
    let healthy = |p: &String| -> bool { eligible(p) && health.check(p).usable };
    let reviewer_role_source = if cfg.cli_role_keys.contains(&SlotRole::Reviewer) {
        SeatSource::CliRole
    } else {
        SeatSource::RolesFile
    };
    // A pinned panel is an exclusion list (Bug A): rotation must stay inside
    // `[roles].reviewer` and never reach into the run's pool or `[providers].order` for a
    // replacement, even when the pin list has only one entry and offers none. Only a real
    // pin list counts here (`reviewer_panel_has_pins`, not `reviewer_panel_pinned`): a
    // `--fleet small` narrowing with no underlying pins has nothing to rotate within and
    // must fall through like an unpinned panel. Bound the pin candidates to `panel_size`
    // too, so rotating a `small`-truncated two-pin panel duplicates the dispatched pin
    // rather than restoring the pin the preset dropped.
    let role_pinned = reviewer_role_source.is_explicit_pin();
    let next = if crate::workflow::roles_resolve::reviewer_panel_has_pins(cfg) {
        cfg.roles
            .reviewer
            .iter()
            .take(crate::workflow::roles_resolve::panel_size(cfg))
            .find(|p| eligible(p) && (role_pinned || health.check(p).usable))
            .cloned()
            .map(|p| (p, reviewer_role_source))
    } else {
        cfg.roles
            .reviewer
            .iter()
            .find(|p| eligible(p))
            .cloned()
            .map(|p| (p, reviewer_role_source))
            .or_else(|| {
                let pool_pinned = matches!(state.pool_origin, PoolOrigin::CliProviders);
                state
                    .providers
                    .iter()
                    .find(|p| if pool_pinned { eligible(p) } else { healthy(p) })
                    .cloned()
                    .map(|p| (p, state.pool_origin.as_seat_source()))
            })
            .or_else(|| {
                cfg.providers
                    .order
                    .iter()
                    .find(|p| healthy(p))
                    .cloned()
                    .map(|p| (p, SeatSource::ProvidersOrder))
            })
    };
    let Some((next, source)) = next else {
        return Ok(None);
    };
    if let Some(s) = state.slot_mut(rev_id) {
        s.point_at(&next);
        s.source = Some(source);
        s.cwd = Some(review_cwd.to_path_buf());
    }
    state.save(paths)?;
    Ok(Some(rev_id.to_string()))
}

#[cfg(test)]
mod moved_seat_tests {
    use super::*;

    #[test]
    fn moved_seat_ids_name_the_new_provider() {
        assert_eq!(
            moved_seat_id(
                SlotRole::Reviewer,
                Some(ReviewerSeatKind::Panel),
                "review-1-cli-codex",
                "cli:claude"
            ),
            "review-1-cli-claude"
        );
        assert_eq!(
            moved_seat_id(
                SlotRole::Reviewer,
                Some(ReviewerSeatKind::Wide),
                "review-cli-grok-wide",
                "cli:claude"
            ),
            "review-cli-claude-wide"
        );
        assert_eq!(
            moved_seat_id(
                SlotRole::Reviewer,
                Some(ReviewerSeatKind::Wide),
                "review-3-wide",
                "cli:claude"
            ),
            "review-cli-claude-wide"
        );
        assert_eq!(
            moved_seat_id(SlotRole::Tester, None, "suite-cli-codex", "cli:pi"),
            "suite-cli-pi"
        );
        assert_eq!(
            moved_seat_id(SlotRole::Implementer, None, "impl", "cli:pi"),
            "impl"
        );
    }

    #[test]
    fn a_rename_carries_the_artifact_and_its_history() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        let mut slot = executor::init_slot("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        slot.artifact = Some("review-review-0-cli-codex.md".into());
        state.review_history.push(crate::state::ReviewHistoryEntry {
            round: 1,
            slot_id: "review-0-cli-codex".to_string(),
            review: review_result::parse_review("## Verdict\nrequest_changes\n"),
        });
        let refused: crate::effort::EffortLevel = crate::effort::EffortLevel::all()
            .iter()
            .map(|r| r.parse().unwrap())
            .find(|l| executor::check_slot_effort("cli:claude", Some(*l)).is_err())
            .expect("claude refuses some level");
        slot.effort = Some(refused);
        state.slots.push(slot);
        let pin = crate::provider_ref::Pin::parse("cli:claude").unwrap();
        let id = move_seat(
            &mut state,
            &paths,
            "review-0-cli-codex",
            &pin,
            SeatSource::Reassigned,
        )
        .unwrap();
        let s = state.slots.iter().find(|s| s.id == id).unwrap();
        assert_eq!(id, "review-0-cli-claude");
        assert_eq!(s.artifact.as_deref(), Some("review-review-0-cli-claude.md"));
        assert_eq!(
            state
                .review_history
                .iter()
                .map(|e| e.slot_id.as_str())
                .collect::<Vec<_>>(),
            ["review-0-cli-claude"],
            "the seat's own entries move with the rename"
        );
        assert_eq!(s.effort, None, "a level the new CLI refuses is dropped");
    }

    /// A rename onto a retired id fails closed instead of resurrecting a seat
    /// whose history was pruned at its drop.
    #[test]
    fn a_rename_onto_a_retired_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r-retired",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.slots.push(executor::init_slot(
            "review-0-cli-codex",
            "cli:codex",
            SlotRole::Reviewer,
        ));
        state
            .retired_seat_ids
            .insert("review-0-cli-claude".to_string());
        let pin = crate::provider_ref::Pin::parse("cli:claude").unwrap();
        let err = move_seat(
            &mut state,
            &paths,
            "review-0-cli-codex",
            &pin,
            SeatSource::Reassigned,
        )
        .expect_err("reminting a retired id must fail");
        assert!(
            err.to_string().contains("retired"),
            "the refusal names the retired id: {err}"
        );
        let seat = state
            .slots
            .iter()
            .find(|s| s.id == "review-0-cli-codex")
            .expect("the seat is still there");
        assert_eq!(
            (seat.provider.as_str(), seat.source),
            ("cli:codex", None),
            "the refusal lands before the seat is touched: {seat:?}"
        );
    }
}

#[cfg(test)]
mod recheck_seat_tests {
    use super::*;
    use tempfile::tempdir;

    fn state_with(slot_id: &str, provider: &str, role: SlotRole) -> RunState {
        let mut st = RunState::new(
            "r-recheck",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        st.slots.push(executor::init_slot(slot_id, provider, role));
        st
    }

    fn pause(paths: &SparPaths, key: &str) {
        let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        store.pause_manual(key, None);
        store.save(paths).unwrap();
    }

    #[test]
    fn an_eligible_seat_dispatches_as_it_is() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let got = recheck_seat(
            &mut st,
            &paths,
            &Config::default(),
            "review-0-cli-grok",
            false,
        )
        .unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-grok"));
    }

    /// The 2026-10-22 shape: grok paused after the run started. The next grok reviewer
    /// must move to its declared backup, renamed to name its new vendor (O85).
    /// The backup comes from the seat's live panel position, not the ordinal in
    /// its id: this seat is still named `review-1` but it is the only seat left,
    /// so position 0 governs.
    #[test]
    fn a_paused_seat_moves_to_the_backup_at_its_live_panel_position() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        pause(&paths, "cli:grok");
        let mut st = state_with("review-1-cli-grok", "cli:grok", SlotRole::Reviewer);
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex@gpt-5.6-terra".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-1-cli-grok", false).unwrap();
        let SeatCheck::Ready(id) = got else {
            panic!("expected a move to the backup");
        };
        assert_eq!(id, "review-1-cli-codex");
        let slot = st.slots.iter().find(|s| s.id == id).unwrap();
        assert_eq!(slot.provider, "cli:codex");
        assert_eq!(slot.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(slot.source, Some(SeatSource::Backup));
    }

    #[test]
    fn a_paused_seat_with_no_eligible_backup_parks() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        pause(&paths, "cli:claude");
        let mut st = state_with("impl", "cli:claude", SlotRole::Implementer);
        let got = recheck_seat(&mut st, &paths, &Config::default(), "impl", true).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(ParkCause::Quota, ref m) if m.contains("cli:claude"))
        );

        pause(&paths, "cli:codex");
        let mut cfg = Config::default();
        cfg.backups.implementer = Some("cli:codex".into());
        let got = recheck_seat(&mut st, &paths, &cfg, "impl", true).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(ParkCause::Quota, _)),
            "a paused backup is no way out"
        );
        assert_eq!(st.slots[0].provider, "cli:claude");
    }

    /// One id threads plan, implement, and every round (O45), so `created_at`
    /// long predates any later round: the pre-existing/this-dispatch line is the
    /// dispatch start. A weak pause written after the run was created but before
    /// this dispatch steers onto the backup; one written after the dispatch
    /// started stays trust-only.
    #[test]
    fn a_weak_pause_between_creation_and_dispatch_steers_onto_the_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        st.created_at -= chrono::Duration::minutes(10);
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.providers.get_mut("cli:grok").unwrap().updated_at =
            Some(st.created_at + chrono::Duration::seconds(60));
        store.save(&paths).unwrap();
        st.stamp_dispatch();
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-grok", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-codex"),
            "a weak pause predating the dispatch steers onto the backup"
        );
    }

    #[test]
    fn a_weak_pause_written_after_dispatch_start_stays_trust_only() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        st.stamp_dispatch();
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.save(&paths).unwrap();
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-grok", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-grok"),
            "a weak pause from this dispatch must not move the seat"
        );
    }

    /// A weak pause written before the run started steers the next run onto its
    /// backup (O106): only pauses this run wrote stay trust-only.
    #[test]
    fn a_weak_pause_older_than_the_run_moves_a_reviewer_to_its_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.providers.get_mut("cli:grok").unwrap().updated_at =
            Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        store.save(&paths).unwrap();
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-grok", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-codex"),
            "a pre-existing weak pause steers onto the backup"
        );
    }

    /// A continued run re-resolves an auto-selected implementer through the same
    /// point as launch: the sick provider is re-seated onto the healthy order
    /// entry, never redispatched.
    #[test]
    fn a_continued_run_reseats_a_sick_auto_implementer() {
        use crate::providers::StubHealth;
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("impl", "cli:codex", SlotRole::Implementer);
        st.providers = vec!["cli:codex".into(), "cli:claude".into()];
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:codex".into(), "cli:claude".into()];
        let health = StubHealth::refuse("cli:codex", "probe `codex login status` failed");
        let slot = st.slots[0].clone();
        let got = recheck_seat_health(&mut st, &paths, &cfg, &slot, &health).unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "impl"));
        let slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(slot.provider, "cli:claude");
        assert_eq!(slot.source, Some(SeatSource::ProvidersOrder));
    }

    /// Nothing healthy left on continuation is a readiness error naming the
    /// probe, never a quota park.
    #[test]
    fn a_continued_run_with_no_healthy_provider_fails_readiness() {
        use crate::providers::StubHealth;
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("impl", "cli:codex", SlotRole::Implementer);
        st.providers = vec!["cli:codex".into()];
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:codex".into()];
        let health = StubHealth::refuse("cli:codex", "probe `codex login status` failed");
        let slot = st.slots[0].clone();
        let err = recheck_seat_health(&mut st, &paths, &cfg, &slot, &health)
            .err()
            .expect("readiness error, not a seat");
        assert!(err.to_string().contains("no healthy provider"), "{err}");
        assert!(err.to_string().contains("cli:codex"), "{err}");
    }

    /// A sick pin keeps its seat on continuation, exactly as at launch.
    #[test]
    fn a_continued_run_keeps_a_sick_pin_on_its_seat() {
        use crate::providers::StubHealth;
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("impl", "cli:codex", SlotRole::Implementer);
        st.providers = vec!["cli:codex".into(), "cli:claude".into()];
        st.slots[0].source = Some(SeatSource::CliProviders);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:codex".into(), "cli:claude".into()];
        let health = StubHealth::refuse("cli:codex", "probe `codex login status` failed");
        let slot = st.slots[0].clone();
        let got = recheck_seat_health(&mut st, &paths, &cfg, &slot, &health).unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "impl"));
        assert_eq!(st.slots[0].provider, "cli:codex");
    }

    /// A continued reviewer file seat on a provider that went unhealthy yields
    /// to its backup, exactly as the launch resolution would seat it.
    #[test]
    fn a_continued_run_moves_a_sick_auto_reviewer_onto_its_backup() {
        use crate::providers::StubHealth;
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        st.providers = vec!["cli:codex".into()];
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        cfg.providers.order = vec!["cli:codex".into()];
        cfg.backups.reviewer = vec!["cli:claude".into()];
        let health = StubHealth::refuse("cli:codex", "probe `codex login status` failed");
        let slot = st.slots[0].clone();
        let got = recheck_seat_health(&mut st, &paths, &cfg, &slot, &health).unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-claude"));
        let slot = st
            .slots
            .iter()
            .find(|s| s.id == "review-0-cli-claude")
            .unwrap();
        assert_eq!(slot.provider, "cli:claude");
        assert_eq!(slot.source, Some(SeatSource::Backup));
    }

    /// A continued tester file seat on a provider that went unhealthy also
    /// yields to its backup instead of dispatching the sick provider.
    #[test]
    fn a_continued_run_moves_a_sick_auto_tester_onto_its_backup() {
        use crate::providers::StubHealth;
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("test", "cli:codex", SlotRole::Tester);
        st.providers = vec!["cli:codex".into()];
        let mut cfg = Config::default();
        cfg.roles.tester = Some("cli:codex".into());
        cfg.providers.order = vec!["cli:codex".into()];
        cfg.backups.tester = Some("cli:claude".into());
        let health = StubHealth::refuse("cli:codex", "probe `codex login status` failed");
        let slot = st.slots[0].clone();
        let got = recheck_seat_health(&mut st, &paths, &cfg, &slot, &health).unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "suite-cli-claude"));
        let slot = st
            .slots
            .iter()
            .find(|s| s.id == "suite-cli-claude")
            .unwrap();
        assert_eq!(slot.provider, "cli:claude");
        assert_eq!(slot.source, Some(SeatSource::Backup));
    }

    /// A continued reviewer with nothing healthy left stands down and the panel
    /// keeps running, mirroring the launch narrowing: never a readiness error
    /// for a reviewer, and never a quota park.
    #[test]
    fn a_continued_reviewer_with_nothing_healthy_stands_down() {
        use crate::providers::StubHealth;
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        st.providers = vec!["cli:codex".into()];
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        cfg.providers.order = vec!["cli:codex".into()];
        let health = StubHealth::refuse("cli:codex", "probe `codex login status` failed");
        let slot = st.slots[0].clone();
        let got = recheck_seat_health(&mut st, &paths, &cfg, &slot, &health).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(ParkCause::Readiness, _)),
            "a reviewer with no healthy seat stands down"
        );
    }

    #[test]
    fn a_weak_pause_older_than_the_run_parks_a_tester_with_no_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:codex", "possible quota signal: 429");
        store.providers.get_mut("cli:codex").unwrap().updated_at =
            Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        store.save(&paths).unwrap();
        let mut st = state_with("test", "cli:codex", SlotRole::Tester);
        let got = recheck_seat(&mut st, &paths, &Config::default(), "test", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(ParkCause::Quota, _)),
            "a pre-existing weak pause parks a tester with no backup"
        );
    }

    /// A pre-existing weak pause benches a reviewer with no backup too: the
    /// re-check parks, and the review panel loop stands the seat down instead
    /// of parking the run.
    #[test]
    fn a_weak_pause_older_than_the_run_parks_a_reviewer_with_no_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.providers.get_mut("cli:grok").unwrap().updated_at =
            Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        store.save(&paths).unwrap();
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let got = recheck_seat(
            &mut st,
            &paths,
            &Config::default(),
            "review-0-cli-grok",
            false,
        )
        .unwrap();
        assert!(
            matches!(got, SeatCheck::Park(ParkCause::Quota, _)),
            "a pre-existing weak pause parks a reviewer with no backup"
        );
    }

    /// The broad scrape paused codex because a test log said "429 passed". That pause is
    /// weak: it must not park this run or move its seat.
    #[test]
    fn a_weak_heuristic_pause_never_moves_or_parks_a_live_seat() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:codex", "possible quota signal: 429");
        store.save(&paths).unwrap();
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-codex", false).unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-codex"));
        assert_eq!(st.slots[0].provider, "cli:codex");

        let mut store = crate::quota::QuotaStore::load(&paths).unwrap();
        store.mark_strong("cli:codex");
        store.save(&paths).unwrap();
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-codex", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-grok"),
            "a strong pause still moves the seat"
        );
    }

    /// The start-of-run gate is strict where a live re-check is lenient (O106): a
    /// weak pause still steers the next run's seat choice, so with no backup the
    /// implementer parks instead of dispatching into it.
    #[test]
    fn a_weak_pause_parks_the_start_gate_with_no_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:claude", "possible quota signal: quota");
        store.save(&paths).unwrap();
        let mut st = state_with("impl", "cli:claude", SlotRole::Implementer);
        let got = recheck_seat(&mut st, &paths, &Config::default(), "impl", true).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(ParkCause::Quota, ref m) if m.contains("cli:claude"))
        );
    }

    #[test]
    fn reviewer_ordinal_reads_the_panel_position() {
        assert_eq!(reviewer_ordinal("review-0-cli-claude"), Some(0));
        assert_eq!(reviewer_ordinal("review-12-cli-codex"), Some(12));
        assert_eq!(reviewer_ordinal("review-cli-grok-wide"), None);
        assert_eq!(reviewer_ordinal("impl"), None);
    }

    #[test]
    fn panel_by_ordinal_excludes_widened_extras() {
        let mut st = RunState::new(
            "r-wide",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        for (id, provider) in [
            ("review-0-cli-codex", "cli:codex"),
            ("review-1-cli-grok", "cli:grok"),
            ("review-3-wide", "cli:agy"),
            ("review-cli-pi-wide", "cli:pi"),
        ] {
            st.slots
                .push(executor::init_slot(id, provider, SlotRole::Reviewer));
        }
        assert_eq!(
            reviewer_panel_by_ordinal(&st),
            vec!["cli:codex".to_string(), "cli:grok".to_string()],
            "neither the pinned nor the unpinned widened extra is a panel seat"
        );
        assert_eq!(panel_position(&st, "review-3-wide"), None);
        assert_eq!(panel_position(&st, "review-cli-pi-wide"), None);
        assert_eq!(panel_position(&st, "review-1-cli-grok"), Some(1));
    }

    #[test]
    fn park_cause_maps_to_the_exit_code() {
        assert!(
            ParkCause::Quota.quota_hit(),
            "a quota stand-down parks the run (exit 4)"
        );
        assert!(
            !ParkCause::Readiness.quota_hit(),
            "a readiness stand-down fails (exit 1)"
        );
    }

    #[test]
    fn stood_down_panel_parks_on_quota_and_fails_on_readiness() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mixed = vec![
            (ParkCause::Quota, "provider cli:grok is paused".to_string()),
            (
                ParkCause::Readiness,
                "provider cli:claude probe failed".to_string(),
            ),
        ];
        let (cause, messages) =
            panel_stop(2, &mixed).expect("a fully stood-down panel stops the run");
        assert_eq!(
            cause,
            ParkCause::Quota,
            "any quota stand-down parks (exit 4)"
        );
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let res = fail(
            &mut st,
            &paths,
            anyhow::anyhow!(messages),
            cause.quota_hit(),
        );
        assert!(res.is_ok(), "a quota park returns Ok, not Err");
        assert_eq!(st.phase, Phase::Quota);
        assert_eq!(st.exit_code(), crate::exit_codes::ExitCode::Quota);

        let unready = vec![(ParkCause::Readiness, "probe failed".to_string())];
        let (cause, messages) =
            panel_stop(1, &unready).expect("a fully stood-down panel stops the run");
        assert_eq!(cause, ParkCause::Readiness, "all-readiness fails (exit 1)");
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let res = fail(
            &mut st,
            &paths,
            anyhow::anyhow!(messages),
            cause.quota_hit(),
        );
        assert!(res.is_err(), "a readiness failure propagates Err");
        assert_eq!(st.phase, Phase::Failed);
        assert_eq!(st.exit_code(), crate::exit_codes::ExitCode::Failure);

        assert!(
            panel_stop(2, &unready).is_none(),
            "a panel with a reviewer still running does not stop"
        );
        assert!(panel_stop(0, &[]).is_none(), "an empty panel does not stop");
    }
}

#[cfg(test)]
mod carried_rejection_tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn stood_down(body: &str) -> SlotState {
        let mut s = executor::init_slot("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        s.stood_down = Some("provider cli:grok is paused".into());
        s.review = Some(crate::workflow::review_result::parse_review(body));
        s
    }

    fn verdict_on(verdict: &str, sha: &str) -> String {
        format!("## Verdict\n{verdict}\n\nReviewed-Commit: {sha}\n\n## Findings\n- severity: major - x\n")
    }

    #[test]
    fn request_changes_on_this_head_carries() {
        let s = stood_down(&verdict_on("request_changes", HEAD));
        assert!(carried_rejection(&s, Some(HEAD)).is_some());
    }

    #[test]
    fn request_changes_on_an_older_head_does_not_carry() {
        let s = stood_down(&verdict_on("request_changes", OTHER));
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn approve_on_this_head_does_not_carry() {
        let s = stood_down(&verdict_on("approve", HEAD));
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_dispatched_seat_carries_nothing() {
        let mut s = stood_down(&verdict_on("request_changes", HEAD));
        s.stood_down = None;
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_seat_that_never_judged_carries_nothing() {
        let mut s = executor::init_slot("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        s.stood_down = Some("provider cli:grok is paused".into());
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_failure_stub_names_no_commit_so_it_carries_nothing() {
        let s = stood_down("## Verdict\nrequest_changes\n\n## Findings\n- severity: major - x\n");
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_stood_down_veto_on_this_head_counts_as_a_panel_blocker() {
        let s = stood_down(&verdict_on("request_changes", HEAD));
        let out = carried_veto_blocker_lines(&[s], Some(HEAD), &[], &Config::default());
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("review-0-cli-grok") && out[0].contains(HEAD),
            "the panel line names the seat and the head its veto still blocks: {}",
            out[0]
        );
    }

    #[test]
    fn a_stale_veto_blocks_nothing() {
        let s = stood_down(&verdict_on("request_changes", OTHER));
        let out = carried_veto_blockers(&[s], Some(HEAD), &[], &Config::default());
        assert!(out.is_empty());
    }

    #[test]
    fn a_stood_down_approve_blocks_nothing() {
        let s = stood_down(&verdict_on("approve", HEAD));
        let out = carried_veto_blockers(&[s], Some(HEAD), &[], &Config::default());
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod reviewer_infra_tests {
    use super::*;
    use tempfile::tempdir;

    fn failed(tools: Option<u32>, exit: Option<i32>, error: &str) -> SlotState {
        let mut s = executor::init_slot("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        s.status = SlotStatus::Failed;
        s.exit_code = exit;
        s.error = Some(error.into());
        s.exec_backend = Some(crate::provider_ref::ExecBackend::NativeCli);
        s.usage = tools.map(|t| {
            serde_json::from_value(serde_json::json!({
                "slot_id": "review-0-cli-codex",
                "provider": "cli:codex",
                "input_tokens": 0,
                "output_tokens": 0,
                "tools": t,
            }))
            .unwrap()
        });
        s
    }

    /// The incident shapes from #155: codex unable to spawn its host binary, a logged
    /// out CLI, a revoked token. None of them is a review.
    #[test]
    fn provider_failures_are_infrastructure_not_verdicts() {
        let spawn = failed(
            None,
            None,
            "spawn failed: No such file or directory (os error 2)",
        );
        assert!(reviewer_infra_fault(&spawn)
            .unwrap()
            .starts_with("could not start"));

        let zero = failed(
            Some(0),
            Some(1),
            "Error: failed to spawn codex-code-mode-host: No such file or directory",
        );
        assert!(reviewer_infra_fault(&zero).unwrap().contains("tools == 0"));

        let mut quota = failed(Some(3), Some(1), "exit 1");
        quota.quota_hit = true;
        assert_eq!(
            reviewer_infra_fault(&quota).as_deref(),
            Some("rate-limited")
        );

        let mut auth = failed(Some(4), Some(1), "auth: cli:codex rejected the credential");
        auth.auth_fault = true;
        assert!(reviewer_infra_fault(&auth).unwrap().starts_with("auth:"));
    }

    /// A reviewer that read the diff and then failed to write its artifact keeps the
    /// fail-closed `request_changes` (O89); so does a hung one spar killed.
    #[test]
    fn a_reviewer_that_ran_keeps_todays_fail_closed_path() {
        let ran = failed(
            Some(7),
            Some(1),
            "missing expected artifact: review-review-0-cli-codex.md",
        );
        assert!(reviewer_infra_fault(&ran).is_none());

        let mut killed = failed(Some(0), None, "hard ceiling: slot timed out");
        killed.signal = Some(9);
        assert!(reviewer_infra_fault(&killed).is_none());

        let mut api = failed(Some(0), Some(1), "exit 1");
        api.exec_backend = Some(crate::provider_ref::ExecBackend::ApiSdk);
        assert!(
            reviewer_infra_fault(&api).is_none(),
            "api slots always report tools 0"
        );

        let mut done = failed(Some(0), Some(0), "");
        done.status = SlotStatus::Done;
        assert!(reviewer_infra_fault(&done).is_none());
    }

    #[test]
    fn an_infra_failure_rotates_to_the_declared_backup_first() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = RunState::new(
            "r-infra",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        st.slots.push(failed(Some(0), Some(1), "exit 1"));
        st.providers = vec!["cli:claude".into(), "cli:codex".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into()];

        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0-cli-codex",
            tmp.path(),
            &cfg,
            true,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap();
        assert_eq!(id.as_deref(), Some("review-0-cli-grok"));
        assert_eq!(st.slots[0].provider, "cli:grok");
        assert_eq!(st.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn a_work_failure_never_takes_the_backup_and_rotation_skips_paused_providers() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_manual("cli:claude", None);
        store.save(&paths).unwrap();
        let mut st = RunState::new(
            "r-infra",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        st.slots
            .push(failed(Some(5), Some(1), "missing expected artifact"));
        st.providers = vec!["cli:claude".into(), "cli:codex".into(), "cli:agy".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into()];

        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0-cli-codex",
            tmp.path(),
            &cfg,
            false,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap();
        assert_eq!(id.as_deref(), Some("review-0-cli-codex"));
        assert_eq!(
            st.slots[0].provider, "cli:agy",
            "paused claude is skipped, the backup is not used for a work failure"
        );
    }

    #[test]
    fn only_spars_own_salvage_stub_is_dropped() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let art = paths.artifact("r1", "review-review-0-cli-codex.md");
        std::fs::create_dir_all(art.parent().unwrap()).unwrap();
        std::fs::write(
            &art,
            format!(
                "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — {} (exit 1)\n",
                executor::REVIEW_SALVAGE_FINDING
            ),
        )
        .unwrap();
        drop_salvaged_verdict(&paths, "r1", "review-0-cli-codex");
        assert!(!art.exists());

        std::fs::write(
            &art,
            "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — real bug\n",
        )
        .unwrap();
        drop_salvaged_verdict(&paths, "r1", "review-0-cli-codex");
        assert!(
            art.exists(),
            "a review the slot wrote is evidence and stays"
        );
    }
}

#[cfg(test)]
mod rotate_reviewer_tests {
    use super::*;
    use tempfile::tempdir;

    fn state_with_reviewer(provider: &str) -> RunState {
        let mut st = RunState::new(
            "r-rotate",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        let mut slot = executor::init_slot("review-0", provider, SlotRole::Reviewer);
        slot.source = Some(SeatSource::RolesFile);
        st.slots.push(slot);
        st
    }

    /// A one-entry pinned panel offers no alternate provider: rotation must refuse
    /// rather than reach into `state.providers`/`[providers].order` for a substitute
    /// nobody pinned (Bug A, also reachable through this escalation path).
    #[test]
    fn single_pin_panel_never_falls_through_to_pool_or_order() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:codex");
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        let changed = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0",
            tmp.path(),
            &cfg,
            false,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .is_some();
        assert!(!changed, "single-pin panel has no alternate to rotate to");
        assert_eq!(st.slots[0].provider, "cli:codex");
    }

    /// A two-entry pinned panel rotates within the pins, never past them.
    #[test]
    fn two_pin_panel_rotates_to_the_other_pin_only() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:codex");
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into(), "api:openai".into()];
        let changed = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0",
            tmp.path(),
            &cfg,
            false,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .is_some();
        assert!(changed);
        assert_eq!(st.slots[0].provider, "api:openai");
    }

    /// A drop retires the lower seat while survivors keep their ids, so the
    /// ordinal baked into an id is stale: after dropping reviewer[0] from a
    /// 3-seat panel with backups [b0,b1,b2], `review-1-*` sits at live
    /// position 0 and its infra failure must take b1, never the id's b2.
    #[test]
    fn infra_failover_after_a_drop_takes_the_seats_own_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = RunState::new(
            "r-drop-failover",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        let mut first = executor::init_slot("review-1-cli-codex", "cli:codex", SlotRole::Reviewer);
        first.status = SlotStatus::Failed;
        first.error = Some("could not start: spawn failed".into());
        first.exec_backend = Some(crate::provider_ref::ExecBackend::NativeCli);
        st.slots.push(first);
        st.slots.push(executor::init_slot(
            "review-2-cli-agy",
            "cli:agy",
            SlotRole::Reviewer,
        ));
        st.providers = vec!["cli:claude".into(), "cli:codex".into(), "cli:agy".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into(), "cli:pi".into()];

        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-1-cli-codex",
            tmp.path(),
            &cfg,
            true,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .expect("seat 1 keeps its own backup after the drop");
        assert_eq!(id, "review-1-cli-grok");
        assert_eq!(st.slots[0].provider, "cli:grok");
        assert_eq!(st.slots[0].source, Some(SeatSource::Backup));

        // The second survivor keeps its own backup too: seat 1 took the
        // position-0 backup, so seat 2 must take the position-1 backup, never
        // find none because seat 1 consumed the wrong entry.
        let second = st
            .slots
            .iter_mut()
            .find(|s| s.id == "review-2-cli-agy")
            .expect("seat 2 survives the first rotation");
        second.status = SlotStatus::Failed;
        second.error = Some("could not start: spawn failed".into());
        second.exec_backend = Some(crate::provider_ref::ExecBackend::NativeCli);
        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-2-cli-agy",
            tmp.path(),
            &cfg,
            true,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .expect("seat 2 keeps its own backup after seat 1 failed over");
        assert_eq!(id, "review-2-cli-pi");
        let second = st
            .slots
            .iter()
            .find(|s| s.id == "review-2-cli-pi")
            .expect("seat 2 moved to its backup");
        assert_eq!(second.provider, "cli:pi");
        assert_eq!(second.source, Some(SeatSource::Backup));
    }

    /// Run 52588b2c's seat: a claude dispatch hit its weekly limit, then the seat was
    /// rotated to codex. The record must not name codex while still carrying claude's
    /// verdict, or `reviewer_infra_fault` reports codex as rate-limited (O149).
    #[test]
    fn rotation_drops_the_previous_providers_verdict() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:claude");
        {
            let s = &mut st.slots[0];
            s.status = SlotStatus::Failed;
            s.quota_hit = true;
            s.auth_fault = true;
            s.exit_code = Some(1);
            s.pid = Some(42);
            s.error = Some("exit 1".into());
            s.usage = Some(
                serde_json::from_value(serde_json::json!({
                    "slot_id": "review-0",
                    "provider": "cli:claude",
                    "input_tokens": 0,
                    "output_tokens": 15,
                }))
                .unwrap(),
            );
        }
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:claude".into(), "cli:codex@gpt-5.6-terra".into()];
        try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0",
            tmp.path(),
            &cfg,
            false,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .expect("rotates to the other pin");
        let s = &st.slots[0];
        assert_eq!(s.provider, "cli:codex");
        assert_eq!(s.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(s.status, SlotStatus::Pending);
        assert!(!s.quota_hit && !s.auth_fault);
        assert!(s.usage.is_none() && s.exit_code.is_none());
        assert_eq!(
            s.pid,
            Some(42),
            "a live process stays visible to stop --abandoned"
        );
        assert_eq!(reviewer_infra_fault(s), None);
    }

    /// `--fleet small` (a preset-only panel narrowing, no `[roles].reviewer` pin) must not
    /// disable failure-recovery rotation: with nothing actually pinned, rotation falls
    /// through to the run's own pool the same way an ordinary unpinned panel does
    /// (review-0-cli-claude, major finding 1).
    #[test]
    fn preset_only_narrowing_still_rotates_through_the_pool() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:grok");
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let cfg = Config {
            fleet_reviewer_override: Some(1),
            ..Config::default()
        };
        let changed = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0",
            tmp.path(),
            &cfg,
            false,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .is_some();
        assert!(changed, "narrowed-but-unpinned panel must still retry");
        assert_eq!(st.slots[0].provider, "cli:claude");
    }

    /// Rotating a `--fleet small`-truncated two-pin panel must not restore the pin
    /// `small` dropped: candidates are bound to `panel_size`, so the truncated panel has
    /// no alternate to rotate to and refuses, exactly like a genuine one-pin panel
    /// (review-0-cli-claude, minor finding 2).
    #[test]
    fn small_truncated_two_pin_panel_does_not_restore_the_dropped_pin() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:codex");
        let cfg = Config {
            fleet_reviewer_override: Some(1),
            roles: crate::config::RolesConfig {
                reviewer: vec!["cli:codex".into(), "api:openai".into()],
                ..Default::default()
            },
            ..Config::default()
        };
        let changed = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0",
            tmp.path(),
            &cfg,
            false,
            &crate::providers::StubHealth::allow_all(),
        )
        .unwrap()
        .is_some();
        assert!(!changed, "truncated panel has no alternate within its size");
        assert_eq!(st.slots[0].provider, "cli:codex");
    }

    /// A `[roles].reviewer` entry is a non-pin seat: rotation passes over an entry
    /// whose login probe fails instead of seating it.
    #[test]
    fn file_pinned_panel_skips_an_unhealthy_entry() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:claude");
        st.providers = vec![];
        st.pool_origin = PoolOrigin::Synth;
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into(), "cli:claude".into(), "cli:grok".into()];
        let health =
            crate::providers::StubHealth::refuse("cli:codex", "probe `codex login` failed");
        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0",
            tmp.path(),
            &cfg,
            false,
            &health,
        )
        .unwrap();
        assert_eq!(id.as_deref(), Some("review-0"));
        assert_eq!(st.slots[0].provider, "cli:grok");
    }
}

#[cfg(test)]
mod try_rotate_implementer_tests {
    use super::*;
    use crate::state::{SeatSource, SlotStatus};
    use tempfile::tempdir;

    fn state_with_impl(provider: &str, source: Option<SeatSource>, quota_hit: bool) -> RunState {
        let mut st = RunState::new(
            "r-rotate-impl",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        let mut slot = executor::init_slot("impl", provider, SlotRole::Implementer);
        slot.source = source;
        slot.status = SlotStatus::Failed;
        slot.quota_hit = quota_hit;
        slot.error = Some("simulated failure".into());
        st.slots.push(slot);
        st.providers = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        st
    }

    #[test]
    fn environmental_activates_backup_over_distinctive_pool_provider() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        // Primary environmental (quota_hit) -> backup, distinctive next is codex but backup is grok@backup
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st.slots[0].error = Some("rate limit seven_day rejected".into());
        // Make primary ineligible via store
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = Some("cli:grok@backup".into());
        // ensure backup provider is available (detect_all would list claude,grok,codex as available if binaries exist? But we use store only, and available None because we didn't set detect. is_provider_eligible will check store and available; with available None, only store matters. backup grok is not paused, so eligible)
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(changed, "environmental should rotate to backup");
        assert_eq!(st.slots[0].provider, "cli:grok");
        assert_eq!(st.slots[0].model.as_deref(), Some("backup"));
        assert_eq!(st.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn work_failure_with_backup_declared_does_not_rotate() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), false);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(
            !changed,
            "work failure with backup declared must not rotate at all (no backup, no pool)"
        );
        assert_eq!(st.slots[0].provider, "cli:claude");
        assert_ne!(st.slots[0].source, Some(SeatSource::Backup));
        // Neutralisation: if work branch were changed to activate backup or pool, this would fail
        let mut st_env = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st_env.slots[0].error = Some("rate limit exceeded".into());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let changed_env = try_rotate_implementer(&mut st_env, &paths, &cfg, &health).unwrap();
        assert!(
            changed_env,
            "environmental with same backup must activate, proving branch not neutralised"
        );
    }

    #[test]
    fn work_failure_without_backup_still_rotates_to_pool() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), false);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = None;
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(
            changed,
            "work failure without backup should rotate via pool"
        );
        assert_eq!(st.slots[0].provider, "cli:codex");
        assert_ne!(st.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn work_failure_with_declared_backup_neutralisation_fails() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), false);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(
            !changed,
            "neutralisation check: work with backup must not activate backup"
        );
        assert_ne!(st.slots[0].source, Some(SeatSource::Backup));
        assert_eq!(st.slots[0].provider, "cli:claude");
        // Environmental should still activate backup – proves branch not neutralised to always false
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let mut st2 = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st2.slots[0].error = Some("rate limit exceeded".into());
        let changed2 = try_rotate_implementer(&mut st2, &paths, &cfg, &health).unwrap();
        assert!(changed2, "environmental with same backup must activate");
        assert_eq!(st2.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn consumed_backup_is_not_reselected() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        // Slot already from backup: simulate environmental failure after backup already activated
        let mut st = state_with_impl("cli:grok", Some(SeatSource::Backup), true);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = Some("cli:grok@backup".into());
        st.providers = vec!["cli:grok".into(), "cli:codex".into()];
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(!changed, "consumed backup must not be reselected");
        assert_eq!(st.slots[0].provider, "cli:grok");
    }

    #[test]
    fn backup_not_eligible_does_not_activate() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        // Also pause backup
        let mut store = crate::quota::QuotaStore::load(&paths).unwrap();
        store.pause_quota("cli:grok", "test");
        store.save(&paths).unwrap();
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st.slots[0].error = Some("rate limit exceeded".into());
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(!changed, "ineligible backup must not be selected");
    }

    #[test]
    fn neutralization_would_fail_backup_branch() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st.slots[0].error = Some("rate limit exceeded".into());
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert(SlotRole::Implementer);
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let health = crate::providers::StubHealth::allow_all();
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(changed);
        assert_eq!(st.slots[0].provider, "cli:grok");
        // If Environmental branch were deleted, rotation would fall to codex
        // This test documents that deleting the if cause == Environmental block changes behavior
    }

    /// The `[providers].order` rung of stuck-ladder rotation passes over an entry
    /// whose login probe fails instead of seating it.
    #[test]
    fn rotation_passes_over_an_unhealthy_order_entry() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::RolesFile), false);
        st.providers = vec![];
        st.pool_origin = PoolOrigin::Synth;
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        let health =
            crate::providers::StubHealth::refuse("cli:codex", "probe `codex login` failed");
        let changed = try_rotate_implementer(&mut st, &paths, &cfg, &health).unwrap();
        assert!(changed, "rotation must pass over the unhealthy order entry");
        assert_eq!(st.slots[0].provider, "cli:grok");
        assert_eq!(st.slots[0].source, Some(SeatSource::ProvidersOrder));
    }
}

#[cfg(test)]
mod try_widen_reviewers_tests {
    use super::*;
    use tempfile::tempdir;

    /// Widening an unpinned panel never seats a provider whose login probe fails:
    /// the `[providers].order` fallback passes over it.
    #[test]
    fn widening_never_adds_an_unhealthy_order_entry() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = RunState::new(
            "r-widen",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        let mut slot = executor::init_slot("review-0", "cli:claude", SlotRole::Reviewer);
        slot.source = Some(SeatSource::ProvidersOrder);
        st.slots.push(slot);
        st.providers = vec![];
        st.pool_origin = PoolOrigin::Synth;
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:codex".into(), "cli:grok".into()];
        let health =
            crate::providers::StubHealth::refuse("cli:codex", "probe `codex login` failed");
        let widened = try_widen_reviewers(&mut st, &paths, tmp.path(), &cfg, &health).unwrap();
        assert!(widened);
        let added = st.slots.iter().find(|s| s.id != "review-0").unwrap();
        assert_eq!(added.provider, "cli:grok");
    }
}

/// `quota_hit` is the discriminator: only a dispatch whose own log matched the quota
/// scrape (`executor::detect_and_pause_quota`) parks here instead of failing outright,
/// so a genuine defect never gets misread as "wait and retry".
///
/// A quota park returns `Ok`, not `Err`: `execute_loop`'s callers propagate an `Err`
/// straight past `finish_out`/`state.exit_code()` to `main`'s catch-all, which always
/// exits `1` — the same shortcut the pre-dispatch quota gate in `run_from_approved`
/// avoids by returning `Ok(ExitCode::Quota)` directly. Ending the round loop with `Ok`
/// here lets the normal tail read the just-set `Phase::Quota` and exit `4`.
fn fail(state: &mut RunState, paths: &SparPaths, e: anyhow::Error, quota_hit: bool) -> Result<()> {
    state.set_phase(if quota_hit {
        Phase::Quota
    } else {
        Phase::Failed
    });
    state.error = Some(e.to_string());
    state.save(paths)?;
    if quota_hit {
        Ok(())
    } else {
        Err(e)
    }
}

#[cfg(test)]
mod fail_quota_routing_tests {
    use super::*;
    use crate::exit_codes::ExitCode as ProcessExitCode;
    use tempfile::tempdir;

    fn state() -> RunState {
        RunState::new(
            "r-quota",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        )
    }

    /// A quota-detected dispatch failure must park at `Phase::Quota` and exit `4`, and
    /// must not propagate `Err` — that would skip `finish_out`/`state.exit_code()` and
    /// fall through to `main`'s catch-all, which always exits `1`.
    #[test]
    fn quota_hit_parks_at_quota_and_does_not_propagate_err() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state();
        let res = fail(&mut st, &paths, anyhow::anyhow!("rate limited"), true);
        assert!(res.is_ok(), "quota park must return Ok, not Err");
        assert_eq!(st.phase, Phase::Quota);
        assert_eq!(st.exit_code(), ProcessExitCode::Quota);
    }

    /// An ordinary dispatch failure (no quota signal in the log) must still fail the
    /// run outright at exit `1` — the discriminator must never swallow a real defect.
    #[test]
    fn ordinary_failure_stays_failed_and_propagates_err() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state();
        let res = fail(
            &mut st,
            &paths,
            anyhow::anyhow!("panic: index out of bounds"),
            false,
        );
        assert!(res.is_err(), "an ordinary failure must still propagate Err");
        assert_eq!(st.phase, Phase::Failed);
        assert_eq!(st.exit_code(), ProcessExitCode::Failure);
    }
}

fn write_impl_summary(state: &RunState, paths: &SparPaths) -> Result<()> {
    let suite_line = match state.suite_outcome {
        Some(SuiteOutcome::Pass) => "Suite: pass\n",
        Some(SuiteOutcome::Fail) => "Suite: fail\n",
        Some(SuiteOutcome::Inconclusive) => {
            "Suite: inconclusive (runner fell over; tests did not run)\n"
        }
        None => "",
    };
    let mut body = format!(
        "# Implementation summary\n\nRun: {}\nTask: {}\nFix rounds: {}\n{suite_line}\n",
        state.id,
        state.task.as_deref().unwrap_or(""),
        state.fix_rounds
    );
    for s in &state.slots {
        body.push_str(&format!("- {} ({}) {:?}\n", s.id, s.provider, s.status));
    }
    if let Some(p) = state.partial_round_phrase() {
        body.push_str(&format!(
            "\nPartial work: {} {} cut short at the hard ceiling; the branch carries partial work.\n",
            p.noun, p.verb
        ));
    }
    body.push_str("\nShip when ready: `spar ship ");
    body.push_str(&state.id);
    body.push_str("` (requires confirm).\n");
    std::fs::write(paths.artifact(&state.id, "summary.md"), body)?;
    Ok(())
}

/// Park the run at the round-ceiling gate instead of buying another cold re-dispatch.
///
const QUEUED_MAX_ROUNDS: &str = "max-rounds.queued";

/// A ceiling at or below the round the run is on parks it again the moment it re-enters,
/// which reads as the flag having been ignored. `reusing` is a round opened but never
/// dispatched: re-entry runs that round again, so it may be the ceiling itself.
/// A `--reassign` the run's own seats refuse (no such seat, ambiguous role): found under
/// the lock, after the launch pre-check, and still a refusal of the launch (O126).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReassignRefused(String);

#[derive(Debug, thiserror::Error)]
#[error(
    "--max-rounds {target} is at or below run {run_id}'s current round {round}, so it would \
     park at the ceiling again immediately; pass at least {floor}, or +N for N more rounds"
)]
pub struct RoundExtensionRefused {
    run_id: String,
    target: u32,
    round: u32,
    floor: u32,
}

/// Re-entry ceiling update: on any re-entry `--max-rounds` only ever raises. A
/// stall park reuses the extension gate below the ceiling, and resolving `+N`
/// there must not drop the ceiling it never reached, or install one on an
/// uncapped run; the same holds for a stopped or failed run re-entered below
/// its ceiling. A fresh launch sets its own ceiling, so the guard is scoped to
/// entries that already minted implementer slots: the first implement entry on
/// a planned run still has only planner slots, and the flag sets the ceiling.
fn gate_max_rounds(current: u32, target: u32, reentry: bool) -> u32 {
    if reentry && (current == 0 || target <= current) {
        current
    } else {
        target
    }
}

pub fn check_round_extension(state: &RunState, target: u32, reusing: bool) -> Result<()> {
    let floor = if reusing && state.round > 0 {
        state.round
    } else {
        state.round + 1
    };
    if target >= floor {
        return Ok(());
    }
    Err(RoundExtensionRefused {
        run_id: state.id.clone(),
        target,
        round: state.round,
        floor,
    }
    .into())
}

/// How long an extension waits for a run's lock when its orchestrator is parking or
/// finishing up (the gate is saved before the lock is released, and reclaim can follow).
const EXTENSION_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// `implement --run <id> --max-rounds …` against a run an orchestrator is still driving.
/// In flight, the extension is left as a marker the loop reads before its next ceiling
/// check and once more after saving a gate. Already at a waitable stop, nothing will read
/// it, so `None` sends the caller to wait for the lock instead; the same when the run
/// reached one between our first read and the marker landing (whichever of us looks
/// second sees the other's write).
fn queue_round_extension(
    paths: &SparPaths,
    state: &RunState,
    target: u32,
    owner_pid: u32,
    json: bool,
) -> Result<Option<ExitCode>> {
    if state.phase.is_waitable_stop() {
        return Ok(None);
    }
    crate::markers::write_marker(paths, &state.id, QUEUED_MAX_ROUNDS, &format!("{target}\n"))?;
    if RunState::load(paths, &state.id)?.phase.is_waitable_stop() {
        return Ok(None);
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::info(format!(
            "round ceiling {target} queued for the live orchestrator (pid {owner_pid})"
        )),
    );
    report_queued(&state.id, target, owner_pid, state.round, json)?;
    Ok(Some(ExitCode::Success))
}

fn report_queued(run_id: &str, target: u32, owner_pid: u32, round: u32, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "run_id": run_id,
                "queued_max_rounds": target,
                "owner_pid": owner_pid,
                "round": round,
            }))?
        );
    } else {
        println!(
            "run {run_id} is being driven by pid {owner_pid}; round ceiling {target} queued for its next ceiling check"
        );
    }
    Ok(())
}

/// Poll for the lock of a run whose orchestrator is parking. `Err(code)` when that
/// orchestrator consumed a marker we left and carried on (the extension took effect);
/// `OrchestratorBusy` when the lock never frees.
fn acquire_for_extension(
    paths: &SparPaths,
    run_id: &str,
    target: u32,
    owner_pid: u32,
    json: bool,
) -> Result<std::result::Result<crate::runlock::RunLock, ExitCode>> {
    let deadline = std::time::Instant::now() + EXTENSION_LOCK_WAIT;
    loop {
        let err = match crate::runlock::RunLock::acquire(paths, run_id) {
            Ok(lock) => return Ok(Ok(lock)),
            Err(e) if e.is::<crate::runlock::OrchestratorBusy>() => e,
            Err(e) => return Err(e),
        };
        if !paths.marker(run_id, QUEUED_MAX_ROUNDS).exists() {
            let st = RunState::load(paths, run_id)?;
            if !st.phase.is_waitable_stop() && st.max_rounds >= target {
                report_queued(run_id, target, owner_pid, st.round, json)?;
                return Ok(Err(ExitCode::Success));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(err);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Consume a queued `--max-rounds`. Only ever raises: a queued value the run already
/// passed (a later flag won) is dropped rather than lowering the ceiling under it.
fn apply_queued_round_extension(state: &mut RunState, paths: &SparPaths) {
    let path = paths.marker(&state.id, QUEUED_MAX_ROUNDS);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let Ok(target) = body.trim().parse::<u32>() else {
        return;
    };
    if state.max_rounds == 0 || target <= state.max_rounds {
        return;
    }
    state.max_rounds = target;
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::info(format!("round ceiling raised to {target} (queued)")),
    );
}

/// Same idiom as the fix-round exhaustion path below it — set an error, write
/// `escalation.md`, stop — with one deliberate difference: this is a **gate** (exit 2),
/// not `Stuck` (exit 3). Nothing is broken; the run has simply spent the re-dispatch
/// budget it was allowed to spend on its own, and only a human can authorise more.
///
/// `Ok(false)` when an extension queued while the gate was being saved lifted the
/// ceiling after all: the caller carries on instead of parking (the lock is still ours).
fn escalate_round_ceiling(state: &mut RunState, paths: &SparPaths) -> Result<bool> {
    // Deliberately does not offer `ship`: `confirm_ship` accepts only the ship and
    // winner gates, so from here it exits 1.
    let msg = format!(
        "round ceiling reached: round {} of a maximum {}. Each further round is a cold \
         re-dispatch. Buy more with `spar implement --run {} --max-rounds {}`, or \
         `spar stop {}` and take it over by hand.",
        state.round,
        state.max_rounds,
        state.id,
        state.max_rounds + 2,
        state.id
    );
    state.set_phase(Phase::AwaitingRoundExtension);
    state.error = Some(msg.clone());
    state.save(paths)?;
    // An extension may have been queued between the last read and this save; a queuer
    // that read the run before the save cannot see the gate, so this read must see it.
    apply_queued_round_extension(state, paths);
    if !state.round_ceiling_reached() {
        state.error = None;
        return Ok(false);
    }
    // `print_run_human` shows the phase but not `error`, so without this an operator
    // running implement interactively sees a phase name and no way out of it.
    eprintln!("{msg}");
    // Findings describe the commit their reviewer read; one whose file moved since may
    // already be fixed, and whoever takes the run over should check it at HEAD (O120).
    let open = review_result::open_findings(state, &review_result::findings_cwd(state));
    let mut findings = String::new();
    if !open.is_empty() {
        findings.push_str("\n## Open findings\n\n");
        for f in &open {
            findings.push_str(&f.line());
            findings.push('\n');
        }
        eprint!("{findings}");
    }
    std::fs::write(
        paths.artifact(&state.id, "escalation.md"),
        format!(
            "# Escalation: round ceiling\n\n{msg}\n\nFix rounds this leg: {}\nSuite: {:?}\n{findings}",
            state.fix_rounds, state.suite_outcome
        ),
    )?;
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg.clone()));
    let _ = crate::bus::broadcast(
        paths,
        Some(&state.id),
        crate::bus::ORCHESTRATOR_SEAT,
        msg,
        state.message_budget,
    );
    Ok(true)
}

/// `{{prior_findings}}` for this round's panel (O119): from `[review]
/// scoped_from_round` on, the previous panel's findings and the diff since the commit it
/// judged, so reviewers check those and look for regressions instead of re-reviewing
/// from scratch. Empty, meaning a full review, when the previous panel cannot anchor
/// one: none recorded, a review spar wrote itself, or reviewers that judged different
/// commits.
fn scoped_review_section(state: &RunState, cfg: &Config, review_cwd: &Path) -> String {
    let from = cfg.review.scoped_from_round;
    if from == 0 || state.round < from {
        return String::new();
    }
    let panel = review_result::panel_before(&state.review_history, state.round);
    if panel.is_empty() || panel.iter().any(|e| e.review.synthetic) {
        return String::new();
    }
    let commit = panel[0].review.reviewed_commit.clone();
    if panel.iter().any(|e| {
        !review_result::same_reviewed_commit(e.review.reviewed_commit.as_deref(), commit.as_deref())
    }) {
        return String::new();
    }
    // A critical filed under Follow-up blocked its round, so it is re-judged too.
    let prior: Vec<(&str, &review_result::Finding)> = review_result::panel_findings(&panel)
        .into_iter()
        .filter(|(_, f)| !f.follow_up || f.severity == review_result::Severity::Critical)
        .map(|(e, f)| (e.slot_id.as_str(), f))
        .collect();
    let stat = commit
        .as_deref()
        .and_then(|c| worktree::git_out(review_cwd, &["diff", "--stat", &format!("{c}..HEAD")]));
    render_scoped_review(
        state.round,
        &prior,
        commit.as_deref(),
        stat.as_deref(),
        review_cwd,
    )
}

fn render_scoped_review(
    round: u32,
    prior: &[(&str, &review_result::Finding)],
    commit: Option<&str>,
    stat: Option<&str>,
    review_cwd: &Path,
) -> String {
    let mut out = format!(
        "## Scoped re-review (round {round})\n\n\
         This is a re-review. Earlier rounds already reviewed this change in full. Judge \
         the previous panel's findings below at HEAD and look for regressions in what \
         changed since; do not re-review the whole change from scratch.\n\n"
    );
    if let Some(c) = commit {
        out.push_str(&format!(
            "The previous panel judged `{c}`. Full diff since: `git -C {} diff {c}..HEAD`.\n",
            review_cwd.display()
        ));
        match stat.map(str::trim).filter(|s| !s.is_empty()) {
            Some(stat) => out.push_str(&format!("\n```\n{stat}\n```\n")),
            None => out.push_str("Nothing changed since, or the diff could not be read.\n"),
        }
        out.push('\n');
    }
    out.push_str("### Prior findings\n");
    if prior.is_empty() {
        out.push_str("- (none recorded: the previous panel blocked on acceptance criteria only)\n");
    }
    for (slot, f) in prior {
        out.push_str(&review_result::finding_line(slot, f));
        out.push('\n');
    }
    out.push_str(
        "\nThis round:\n\
         - Under `## Findings`, list each prior finding still open at HEAD, at its severity, \
         and any regression the change since introduced. Leave resolved ones out and name \
         them under `## Resolved`.\n\
         - Any other new finding that is not critical goes under `## Follow-up`, with a \
         severity (`- severity: major — ...`). Follow-up never blocks the ship; it goes into \
         the PR body as open work. A new critical finding still goes under `## Findings`.\n\
         - `request_changes` means a prior finding is still open, the change since regressed \
         something, or an acceptance criterion fails.\n",
    );
    out
}

fn ceiling_partial_blocker(kind: CeilingPartialKind, round: u32) -> String {
    match kind {
        CeilingPartialKind::Clean => ceiling_partial_clean_notice(round),
        CeilingPartialKind::Refused => ceiling_partial_refused_blocker(round),
        CeilingPartialKind::Wip => ceiling_partial_notice(round),
    }
}

/// Rebuild the previous round's blocker list from the review artifacts on disk.
///
/// Seeds the round loop on entry so a re-dispatch after *any* pause — the round-ceiling
/// gate, a stop, a quota bounce, a crashed orchestrator — starts knowing which `AC-n`
/// failed. Silent when the reviews approved: a stale approval is not a blocker, and the
/// live round overwrites this list at its own review step anyway.
fn blockers_from_disk(
    state: &RunState,
    paths: &SparPaths,
    criteria: &[String],
    cfg: &Config,
) -> Vec<Blocker> {
    let mut out = Vec::new();
    if matches!(state.suite_outcome, Some(SuiteOutcome::Fail)) {
        out.push(Blocker::Note(format!(
            "suite channel red last round: see {}",
            paths.artifact(&state.id, "suite.md").display()
        )));
    }
    for rev in state.slots.iter().filter(|s| s.role == SlotRole::Reviewer) {
        let path = paths.artifact(&state.id, &format!("review-{}.md", rev.id));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Whether that review answered a scoped prompt is on its recorded entry.
        // Writer ids are never shared across seats, so an id match is this
        // seat's own record.
        let scoped = state
            .review_history
            .iter()
            .filter(|e| e.slot_id == rev.id)
            .max_by_key(|e| e.round)
            .is_some_and(|e| e.review.scoped);
        let res = review_result::ReviewResult {
            scoped,
            ..review_result::parse_review(&text)
        };
        let reasons = acceptance_block_reasons(criteria, &res, cfg);
        if res.blocks(cfg.review.block_on) {
            out.push(Blocker::Verdict(verdict_blocker(
                state, rev, &res, &reasons, &path,
            )));
        } else {
            for r in &reasons {
                out.push(Blocker::Note(format!(
                    "acceptance (review `{}`): {r}",
                    rev.id
                )));
            }
        }
    }
    // A round cut short at its hard ceiling left its work behind as a partial
    // round; the round after it starts from that commit whether this process
    // saw the kill or not, so the notice is rebuilt here rather than carried in
    // memory. Only the immediately previous round qualifies: anything older was
    // either consumed by the round after it or approved as-is. The recorded kind
    // picks the wording: only a saved WIP commit may be claimed as one.
    if let Some(prev) = state.round.checked_sub(1) {
        if let Some(entry) = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == prev)
        {
            out.push(Blocker::Note(ceiling_partial_blocker(entry.kind, prev)));
        }
    }
    // A same-round resume after a ceiling kill (`continue` without a new round):
    // the resumed implementer would otherwise start with no idea its own round
    // was cut short. The round-start snapshot is not re-stamped on resume, so
    // this notice cannot make the round's own banked commit read as new work.
    if let Some(entry) = state
        .ceiling_partial_rounds
        .iter()
        .find(|e| e.round == state.round)
    {
        out.push(Blocker::Note(ceiling_partial_blocker(
            entry.kind,
            state.round,
        )));
    }
    out
}

/// Read and consume the previous round's carry-forward brief.
///
/// **Consumed on read.** A round whose implementer died without writing one gets
/// nothing, rather than a brief describing a worktree two rounds stale — which reads as
/// "what I just tried" and is worse than silence.
///
/// Per **slot**, not per run: `artifacts_dir` is shared and arena spawns N concurrent
/// implementers off one template, so an unscoped name would have them overwriting each
/// other. Rounds re-dispatch the same slot id (rotation changes the provider and keeps
/// the id and the worktree), so a slot always reads back its own last round.
/// The brief's filename. Shared with the nudge watcher, which must name it alongside the
/// summary: a nudge that names only the summary gets a slot that lands it and stops, and
/// the next round then finds no brief in exactly the case the brief exists for.
pub fn carry_forward_name(slot_id: &str) -> String {
    format!("carry-forward-{slot_id}.md")
}

fn take_carry_forward(paths: &SparPaths, run_id: &str, slot_id: &str) -> Option<String> {
    let path = paths.artifact(run_id, &carry_forward_name(slot_id));
    let body = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let body = body.trim().to_string();
    (!body.is_empty()).then_some(body)
}

/// A blocking review's carry-forward entry: the anchor head plus the failing
/// acceptance lines and the open findings as separate lists, so the section
/// can budget the acceptance lines first and never truncate them behind
/// findings. One entry per review: a panel that repeats the same criterion
/// failure in every review would otherwise pay the line once per seat and
/// squeeze the slot's own brief out of the budget. The plain statement that
/// these reviews are the current verdict lives once in
/// `carry_forward_section`, not in every entry, for the same reason.
#[derive(Debug)]
struct VerdictBlocker {
    review_id: String,
    review_path: String,
    head: String,
    compact: String,
    acceptance: Vec<String>,
    findings: Vec<String>,
    dropped_findings: usize,
}

/// One carry-forward blocker: a blocking review with its acceptance lines
/// intact, or a context note the cap may eat.
#[derive(Debug)]
enum Blocker {
    Note(String),
    Verdict(VerdictBlocker),
}

/// One blocking review, split into head, acceptance lines, and findings: the
/// head anchors which round wrote it and which commit it judged, with the
/// failing ids up front as a manifest. The acceptance lines stay whole here;
/// the section clamps them to a share of the cap, so a clamp only ever eats
/// evidence and finding tails, never an id.
fn verdict_blocker(
    state: &RunState,
    rev: &SlotState,
    res: &ReviewResult,
    acceptance: &[AcceptanceReason],
    review_path: &Path,
) -> VerdictBlocker {
    let (round, commit) = blocking_review_anchor(state, rev, res);
    let mut head = format!("review `{}` requested changes", rev.id);
    match (round, commit) {
        (Some(r), Some(c)) => {
            head.push_str(&format!(
                " (round {r}, Reviewed-Commit {})",
                short_commit(&c)
            ));
        }
        (None, Some(c)) => head.push_str(&format!(" (Reviewed-Commit {})", short_commit(&c))),
        (Some(r), None) => head.push_str(&format!(" (round {r}, no Reviewed-Commit recorded)")),
        (None, None) => {}
    }
    let ids = acceptance
        .iter()
        .map(|a| a.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !ids.is_empty() {
        head.push_str(&format!(": failing {ids}"));
    }
    // Degraded form for caps even the full heads exceed: keeps the review id,
    // the round, and the failing-ids manifest, which is what the next round
    // must fix. The commit anchor stays in state and on the review artifact.
    let mut compact = format!("review `{}`", rev.id);
    if let Some(r) = round {
        compact.push_str(&format!(" (round {r})"));
    }
    if !ids.is_empty() {
        compact.push_str(&format!(": failing {ids}"));
    }
    let lines: Vec<String> = acceptance
        .iter()
        .map(|a| {
            a.to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let room = MAX_LISTED_FINDINGS.saturating_sub(lines.len().min(MAX_LISTED_FINDINGS));
    let findings: Vec<String> = res
        .findings
        .iter()
        .filter(|f| !f.synthetic)
        .map(open_finding_line)
        .chain(
            res.untagged
                .iter()
                .filter(|t| !t.trim().is_empty())
                .map(|t| open_untagged_line(t)),
        )
        .collect();
    let dropped_findings = findings.len().saturating_sub(room);
    let mut findings = findings;
    findings.truncate(room);
    if lines.is_empty() && findings.is_empty() {
        head.push_str(": no open findings recorded; see the review artifact.");
    }
    VerdictBlocker {
        review_id: rev.id.clone(),
        review_path: review_artifact_name(review_path),
        head,
        compact,
        acceptance: lines,
        findings,
        dropped_findings,
    }
}

/// Display form of a reviewed commit: twelve hex digits name it the way short
/// SHAs do, so the anchor stays a fixed small size under tight budgets. The
/// full value stays in state and on the review artifact.
fn short_commit(c: &str) -> &str {
    c.get(..12).unwrap_or(c)
}

/// The review artifact's file name, not its absolute path: the implementer
/// prompt already prints the artifacts dir, and an absolute path per review
/// comes out of the acceptance reservation, evicting failing-criterion
/// evidence under tight caps.
fn review_artifact_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// At most this many findings ride along in one blocker line or warning alongside
/// the failing criteria, which always print first; the carry-forward clamp bounds
/// the characters on top.
const MAX_LISTED_FINDINGS: usize = 5;

/// The narrowest an acceptance line may render: an id plus a stub of evidence.
const MIN_AC_SHARE: usize = 32;

/// Severity plus one clamped line, so a finding can never smuggle a paragraph
/// into the brief.
fn open_finding_line(f: &review_result::Finding) -> String {
    let at = match (&f.file, f.line) {
        (Some(file), Some(line)) => format!("{file}:{line}"),
        _ => String::new(),
    };
    let text = f.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let located = !at.is_empty() && text.contains(&at);
    let shown = if located || at.is_empty() {
        text
    } else {
        format!("{at} {text}")
    };
    clamp_line(&format!("{} - {shown}", f.severity.as_str()), 160)
}

/// An untagged remark in the same one-line shape as a finding, so the budget
/// treats both alike.
fn open_untagged_line(text: &str) -> String {
    clamp_line(
        &format!(
            "untagged - {}",
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
        160,
    )
}

/// Which round wrote this blocking review and which commit it judged: the latest
/// review-history entry for the seat (renames included) first, the seat's own
/// recorded round and commit as fallback.
fn blocking_review_anchor(
    state: &RunState,
    rev: &SlotState,
    res: &ReviewResult,
) -> (Option<u32>, Option<String>) {
    if let Some(entry) = state
        .review_history
        .iter()
        .filter(|e| e.slot_id == rev.id)
        .max_by_key(|e| e.round)
    {
        return (Some(entry.round), entry.review.reviewed_commit.clone());
    }
    (
        Some(rev.round),
        rev.reviewed_commit.clone().or(res.reviewed_commit.clone()),
    )
}

/// The `{{carry_forward_section}}` seeded into the next round's implementer.
///
/// Blockers first, deliberately: they are the machine-known half (which `AC-n` failed and
/// why, whether the suite went red) and the truncation eats the tail, so a slot that
/// wrote a 40k-character essay cannot squeeze them out. The whole section is capped;
/// carrying more every round is the quadratic problem this exists to avoid.
fn carry_forward_section(
    round: u32,
    persisted: &[String],
    blockers: &[Blocker],
    brief: Option<&str>,
    max_chars: usize,
) -> String {
    if persisted.is_empty() && blockers.is_empty() && brief.is_none() {
        return String::new();
    }
    let mut out = format!(
        "## Carry-forward (you already ran on this worktree, this is round {round})\n\
         Context from the previous round.\n\n"
    );
    if !persisted.is_empty() {
        // First, so the cap cannot cut an operator ruling; these stand until the sender
        // withdraws them, which is the point of `--persist`. The phase is capped so
        // the reservation below always fits; over-budget messages are marked withheld,
        // never silently dropped.
        out.push_str(
            "### Operator messages (they stand until withdrawn; follow them over your own notes)\n",
        );
        let phase_budget = max_chars / 4;
        let phase_start = out.chars().count();
        let mut withheld = 0;
        let mut first = true;
        for m in persisted {
            // Clamp to the phase budget minus the bullet overhead, so a long
            // message renders clamped instead of missing the budget outright.
            // The first message always renders: an operator ruling must never
            // be silently absent from the round it was written for.
            let line = format!("- {}\n", clamp_line(m, phase_budget.saturating_sub(3)));
            if first || out.chars().count() - phase_start + line.chars().count() <= phase_budget {
                out.push_str(&line);
                first = false;
            } else {
                withheld += 1;
            }
        }
        if withheld > 0 {
            out.push_str(&format!(
                "- (+{withheld} more operator messages withheld by budget; they stand until withdrawn)\n"
            ));
        }
        out.push('\n');
    }
    if !blockers.is_empty() {
        out.push_str("### What blocked the ship last round\n");
        // Once per section, not once per bullet: the sentence is the same for every
        // review below, and repeating it per bullet costs a multi-review panel its
        // brief under a tight budget.
        out.push_str(
            "These reviews are the current verdict on the implementer's HEAD and \
             supersede the previous round's own notes.\n",
        );
        // Acceptance lines render before everything expendable, sized to a share
        // of what the cap has left after the heading and the persisted phase,
        // so findings can never push a failing criterion id out of the brief.
        // Heads and the manifest ride the same reservation; notes and findings
        // follow, and the tail truncation below eats them first. When even the
        // floored share does not fit, the block degrades to manifest-only heads
        // rather than dropping whole reviews.
        let verdicts: Vec<&VerdictBlocker> = blockers
            .iter()
            .filter_map(|b| match b {
                Blocker::Verdict(v) => Some(v),
                Blocker::Note(_) => None,
            })
            .collect();
        let total_ac: usize = verdicts.iter().map(|v| v.acceptance.len()).sum();
        let heads_len: usize = verdicts.iter().map(|v| v.head.chars().count()).sum();
        let ac_budget = (max_chars / 2).min(max_chars.saturating_sub(out.chars().count()));
        // Bullets pay their own punctuation: the "- " prefix, the "; " joins
        // and the newline come out of the same reservation, or the share math
        // promises lines that do not fit and the tail cut eats a whole review.
        let manifest_overhead = verdicts.len() * 3;
        let joins_overhead = total_ac * 2;
        // The `full review:` file-name lines ride the same reservation: they
        // render once per review, so sizing acceptance lines without them
        // pushes later reviews' bullets past the tail cut.
        let paths_overhead: usize = verdicts
            .iter()
            .map(|v| v.review_path.chars().count() + "  full review: ``\n".chars().count())
            .sum();
        // When even the full heads exceed the reservation, degrade to compact
        // heads that keep every review id and manifest rather than dropping
        // whole reviews. Below that, the tail cut discloses the loss.
        let compact_len: usize = verdicts.iter().map(|v| v.compact.chars().count()).sum();
        let compact = heads_len + manifest_overhead > ac_budget
            && compact_len + manifest_overhead <= ac_budget;
        let rendered_heads = if compact { compact_len } else { heads_len };
        // No fixed ceiling: with budget left, an acceptance line renders whole.
        // The tail truncation below still caps the section.
        let share = ac_budget
            .saturating_sub(rendered_heads + manifest_overhead + joins_overhead + paths_overhead)
            .checked_div(total_ac.max(1))
            .unwrap_or(0)
            .max(MIN_AC_SHARE);
        let manifest_only = rendered_heads
            + manifest_overhead
            + joins_overhead
            + paths_overhead
            + MIN_AC_SHARE * total_ac
            > ac_budget;
        for (i, v) in verdicts.iter().enumerate() {
            let head = if compact {
                v.compact.clone()
            } else {
                v.head.clone()
            };
            // The first bullet always survives the tail cut below: under a cap
            // even the heads exceed, truncation would otherwise leave the
            // heading with no bullets at all.
            let mut items = vec![if i == 0 {
                clamp_line(&head, ac_budget.saturating_sub(3))
            } else {
                head
            }];
            if !manifest_only {
                items.extend(v.acceptance.iter().map(|a| clamp_line(a, share)));
            }
            if v.findings.is_empty() && v.dropped_findings > 0 {
                items.push(format!("(+{} more)", v.dropped_findings));
            }
            out.push_str(&format!("- {}\n", items.join("; ")));
            out.push_str(&format!("  full review: `{}`\n", v.review_path));
        }
        for b in blockers {
            // Clamped per note, not just in aggregate. A reviewer that pastes a stack
            // trace as its evidence produces one line longer than the whole budget, and
            // the line-boundary truncation below would drop it whole, rendering an
            // empty heading, which reads as "nothing blocked".
            if let Blocker::Note(line) = b {
                out.push_str(&format!("- {}\n", clamp_line(line, max_chars / 4)));
            }
        }
        for v in &verdicts {
            for (i, f) in v.findings.iter().enumerate() {
                let mut line = format!("- {f} (review `{}`)", v.review_id);
                if i + 1 == v.findings.len() && v.dropped_findings > 0 {
                    line.push_str(&format!(" (+{} more)", v.dropped_findings));
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    if let Some(brief) = brief {
        out.push_str("### The previous round's own notes\n");
        out.push_str("This is context, not a verdict. The acceptance gate never reads it.\n");
        out.push_str(brief);
        out.push('\n');
    }
    truncate_at_line(&out, max_chars)
}

/// Cut to `max` characters on a line boundary, saying so where the cut happened.
/// A silent truncation reads as an agent that stopped mid-sentence for its own reasons.
fn truncate_at_line(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let head = match cut.rfind('\n') {
        Some(i) if i > 0 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{head}\n\n_(carry-forward truncated by spar at {max} characters.)_\n")
}

/// Clamp one line so no single bullet can eat the whole section budget.
fn clamp_line(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let head: String = one_line.chars().take(max.saturating_sub(3)).collect();
    format!("{head}...")
}

fn write_stuck(paths: &SparPaths, run_id: &str) -> Result<()> {
    std::fs::write(
        paths.artifact(run_id, "escalation.md"),
        "# Escalation\n\nStuck policy exhausted. Human intervention required.\n",
    )?;
    Ok(())
}

fn finish_out(state: &RunState, json: bool) -> Result<()> {
    reclaim_own_cache(state, json);
    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
    }
    Ok(())
}

/// Drop this run's build output once its orchestrator is done with it.
///
/// Scoped to the run's **own** worktrees, at the moment its own orchestrator concludes —
/// not a project-wide sweep, which is `spar reclaim --all` and stays explicit. Nothing here
/// is unrecoverable: the worktree, its branch, its commits and any uncommitted changes all
/// survive, so this asks no permission and needs no evidence. It is the largest single
/// source of disk on the box (457 GB of 587 GB measured), and a *stopped* run's target dir
/// was the biggest object on the machine.
/// Phases the *automatic* reclaim may take: finished, and nothing can resume them.
///
/// Narrower than `is_terminal()` on purpose. `Quota`, `Stuck` and `Failed` are terminal
/// yet are exactly what `spar implement --run <id>` exists to pick up — a run that paused
/// because a provider bucket ran dry is meant to be resumed when it refills. Deleting
/// `node_modules` there is not free: regenerating it needs the network and an agent that
/// knows to reinstall, and the resumed implementer just meets a broken suite. Those stay
/// for the explicit `spar reclaim`, which is the operator's call.
fn auto_reclaimable(phase: Phase) -> bool {
    matches!(phase, Phase::Done | Phase::PlanRejected | Phase::Escalated)
}

fn reclaim_own_cache(state: &RunState, json: bool) {
    if state.dry_run
        || !auto_reclaimable(state.phase)
        || !crate::config::Config::for_run(&SparPaths::new(&state.project_root), &state.id)
            .map(|c| c.auto_reclaim)
            .unwrap_or(true)
    {
        return;
    }
    let reap = worktree::reap_build_cache(state, &worktree::LiveCwds::snapshot());
    if reap.freed_bytes > 0 && !json {
        eprintln!(
            "reclaimed {:.1} GB of build cache from this run's worktrees",
            reap.freed_bytes as f64 / 1024.0 / 1024.0 / 1024.0
        );
    }
}

fn detach_implement(state: &RunState, paths: &SparPaths, json: bool) -> Result<ExitCode> {
    if let Some(owner) = crate::runlock::RunLock::owner(paths, &state.id) {
        if owner.alive() {
            return Err(crate::runlock::OrchestratorBusy {
                run_id: state.id.clone(),
                owner_pid: owner.pid,
            }
            .into());
        }
    }
    if let Some(msg) = crate::daemon::maybe_enqueue(paths, state)? {
        if json {
            executor::emit_run_json(state)?;
        } else {
            executor::print_run_human(state);
            println!("{msg}");
        }
        return Ok(ExitCode::Success);
    }
    let detached = match crate::process::spawn_detached_orchestrator(paths, &state.id) {
        Ok(d) => d,
        Err(e) => {
            crate::daemon::release_reservation(paths, &state.id);
            return Err(e);
        }
    };
    let outcome = match crate::process::await_detached_start(paths, &state.id, detached) {
        Ok(o) => o,
        Err(e) => {
            crate::daemon::release_reservation(paths, &state.id);
            return Err(e);
        }
    };
    match outcome {
        crate::process::DetachOutcome::Confirmed { pid } => {
            if json {
                executor::emit_run_json(state)?;
            } else {
                executor::print_run_human(state);
                println!(
                    "detached (pid {pid}, session of its own); wait with: spar wait {}",
                    state.id
                );
            }
            Ok(ExitCode::Success)
        }
        crate::process::DetachOutcome::Completed => {
            // The run settled inside the handshake window without ever holding a
            // `Running` slot for `effective_supply` to see, so its admission
            // reservation (if `maybe_enqueue` granted one) would otherwise squat on
            // capacity for the full TTL.
            crate::daemon::release_reservation(paths, &state.id);
            let state = RunState::load_for_display(paths, &state.id)?;
            if json {
                executor::emit_run_json(&state)?;
            } else {
                executor::print_run_human(&state);
            }
            Ok(state.exit_code())
        }
    }
}

/// A run whose orchestrator (or `spar ship`) died in `shipping` goes back to the ship
/// gate: shipping is the operator's step, and every resume entry, the daemon's included,
/// must route here rather than re-running the rounds that produced the work (O117).
pub fn park_interrupted_ship(paths: &SparPaths, run_id: &str, json: bool) -> Result<ExitCode> {
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    let mut state = RunState::load(paths, run_id)?;
    if state.phase == Phase::Shipping {
        state.set_phase(Phase::AwaitingShipConfirm);
        state.save(paths)?;
    }
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!(
            "run {run_id} died while shipping; back at the ship gate: spar ship {run_id} --confirm"
        );
    }
    Ok(state.exit_code())
}

pub fn continue_run(paths: &SparPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase == Phase::Shipping {
        return park_interrupted_ship(paths, run_id, false);
    }
    // An approved plan has finished planning: continuing it is implementation, never a
    // second plan round asking for the approval again (O117).
    if state.workflow == crate::cli::WorkflowKind::Plan && state.gates.plan_approved {
        let opts = CommonOpts {
            dry_run: state.dry_run,
            ..CommonOpts::default()
        };
        return run_from_approved(run_id, None, opts, paths, BoundAmendInputs::default());
    }
    if state.workflow == crate::cli::WorkflowKind::Plan {
        return crate::workflow::plan::continue_run(paths, cfg, run_id);
    }
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    // Re-read under the lock: the routing load above can be stale, and the
    // stamp below writes, so stamping the stale copy would clobber a
    // concurrent writer's fields.
    state = RunState::load(paths, run_id)?;
    // A resume is a new dispatch in this process: re-stamp the horizon and the
    // profile so a pause written before this resume steers the seat, and reads
    // key the seats this process actually dispatches. Fix passes inside the
    // loop below never re-stamp; see `stamp_dispatch`.
    state.stamp_dispatch();
    match state.workflow {
        crate::cli::WorkflowKind::Loop => {
            // A daemon/`continue` resume is nobody's deliberate amendment, so it never
            // adopts a contract that moved under the previous round.
            execute_loop(&mut state, paths, cfg, false)?;
        }
        crate::cli::WorkflowKind::Arena => {
            crate::workflow::arena::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Roles => {
            crate::workflow::roles::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Peer => {
            crate::workflow::peer::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Review => {
            crate::workflow::review::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Plan => unreachable!("plan handled above"),
    }
    Ok(state.exit_code())
}

#[cfg(test)]
mod suite_parse_tests {
    use super::{
        acceptance_block_reasons, acceptance_blocks_ship, auto_reclaimable, command_is_selective,
        command_names, derive_suite_outcome, is_test_path, is_test_target, should_stop,
        suite_blocks_ship, suite_commands, suite_guidance, Phase, SuiteOutcome,
    };
    use crate::config::Config;
    use crate::paths::SparPaths;
    use crate::workflow::review_result::parse_review;
    use tempfile::tempdir;

    fn criteria(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn relaxed() -> Config {
        let mut cfg = Config::default();
        cfg.review.require_all_criteria = false;
        cfg
    }

    #[test]
    fn acceptance_empty_criteria_never_blocks() {
        let res = parse_review("");
        assert!(!acceptance_blocks_ship(&[], &res, &Config::default()));
    }

    #[test]
    fn acceptance_fail_blocks() {
        let res = parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: fail — broken\n");
        assert!(acceptance_blocks_ship(
            &criteria(&["AC-1"]),
            &res,
            &Config::default()
        ));
    }

    #[test]
    fn acceptance_missing_criterion_blocks() {
        let res = parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: pass — ok\n");
        let c = criteria(&["AC-1", "AC-2"]);
        assert!(acceptance_blocks_ship(&c, &res, &Config::default()));
        let reasons = acceptance_block_reasons(&c, &res, &Config::default());
        assert!(
            reasons.iter().any(|r| r.id == "AC-2"),
            "AC-2 missing: {reasons:?}"
        );
        // One entry per criterion: a joined line is dropped whole by the carry-forward's
        // line-boundary truncation, leaving a heading that reads as "nothing blocked".
        assert!(
            reasons
                .iter()
                .all(|r| !r.id.contains(';') && !r.reason.contains(';')),
            "reasons must stay per-criterion: {reasons:?}"
        );
        // Unmentioned criteria block even when unverified is tolerated.
        assert!(acceptance_blocks_ship(&c, &res, &relaxed()));
    }

    #[test]
    fn acceptance_unverified_blocks_by_default() {
        let res =
            parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: unverified — no time\n");
        assert!(acceptance_blocks_ship(
            &criteria(&["AC-1"]),
            &res,
            &Config::default()
        ));
    }

    #[test]
    fn acceptance_unverified_allowed_when_relaxed() {
        let res = parse_review(
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass — ok\nAC-2: unverified — no time\n",
        );
        assert!(!acceptance_blocks_ship(
            &criteria(&["AC-1", "AC-2"]),
            &res,
            &relaxed()
        ));
    }

    #[test]
    fn acceptance_all_pass_does_not_block() {
        let res =
            parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: pass — a\nAC-2: pass — b\n");
        assert!(!acceptance_blocks_ship(
            &criteria(&["AC-1", "AC-2"]),
            &res,
            &Config::default()
        ));
    }

    #[test]
    fn should_stop_tracks_marker() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert!(!should_stop(&paths, "r1"));
        crate::markers::write_marker(&paths, "r1", "stopped", "by operator").unwrap();
        assert!(should_stop(&paths, "r1"));
    }

    #[test]
    fn clean_exit_fail_report_is_fail() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\nfail\n")),
            SuiteOutcome::Fail
        );
    }

    #[test]
    fn clean_exit_pass_or_skipped_is_pass() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\npass\n")),
            SuiteOutcome::Pass
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\nskipped\n")),
            SuiteOutcome::Pass
        );
    }

    #[test]
    fn clean_exit_no_body_is_inconclusive() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), None),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn clean_exit_unparsable_result_is_inconclusive() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n\n")),
            SuiteOutcome::Inconclusive
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("no result header at all")),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn signal_death_with_fail_body_is_inconclusive_not_fail() {
        // Body is not trustworthy when the runner was signal-killed (no exit code captured).
        assert_eq!(
            derive_suite_outcome(false, None, Some("## Result\nfail\n")),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn timeout_is_inconclusive() {
        // Timeout kills the process group: no exit code captured.
        assert_eq!(
            derive_suite_outcome(false, None, None),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn fail_markup_tolerated() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n**fail**\n")),
            SuiteOutcome::Fail
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n`fail`\n")),
            SuiteOutcome::Fail
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n- fail\n")),
            SuiteOutcome::Fail
        );
    }

    #[test]
    fn inconclusive_and_fail_both_block_ship() {
        assert!(suite_blocks_ship(SuiteOutcome::Fail));
        assert!(suite_blocks_ship(SuiteOutcome::Inconclusive));
        assert!(!suite_blocks_ship(SuiteOutcome::Pass));
    }

    /// `suite.md` is what the reviewers and the operator read; `report.outcome` is what
    /// the gate holds. The built-in path sets the gate from exit codes and renders the
    /// body separately, so nothing but this test stops the two from drifting apart and
    /// telling a reviewer the suite passed on a run that blocked.
    #[test]
    fn a_builtin_report_reads_back_as_the_verdict_the_gate_holds() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("suite.log");
        let cases = [
            (vec!["true".to_string()], std::time::Duration::from_secs(30)),
            (
                vec!["exit 1".to_string()],
                std::time::Duration::from_secs(30),
            ),
            (
                vec!["sleep 30".to_string()],
                std::time::Duration::from_millis(300),
            ),
        ];
        for (cmds, budget) in cases {
            let rep = crate::suite::run(&crate::suite::Options {
                cwd: d.path(),
                commands: &cmds,
                log_path: &log,
                budget,
                isolation: crate::config::IsolationMode::Worktree,
                on_pid: &|_| {},
                stop: &|| false,
            });
            assert_eq!(
                derive_suite_outcome(true, Some(0), Some(&rep.body)),
                rep.outcome,
                "{}",
                rep.body
            );
        }
        let rep = crate::suite::dry(&["cargo test".to_string()]);
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some(&rep.body)),
            rep.outcome
        );
    }

    /// The coverage check (O32) parses `## Commands` back out of `suite.md`. That is a
    /// contract between two modules with no compiler between them: a format change on
    /// either side silently disables the warning rather than breaking anything.
    #[test]
    fn the_coverage_check_can_still_read_a_builtin_command_list() {
        let body = crate::suite::dry(&[
            "cargo test --test foo".to_string(),
            "pytest tests/unit".to_string(),
        ])
        .body;
        assert_eq!(
            suite_commands(&body),
            vec!["cargo test --test foo", "pytest tests/unit"]
        );
    }

    #[test]
    fn tester_template_forbids_backgrounding_and_warns_pkill() {
        let tester = include_str!("../../templates/tester.md");
        let lower = tester.to_lowercase();
        assert!(lower.contains("foreground"), "must mandate foreground");
        assert!(lower.contains("background"), "must address backgrounding");
        assert!(
            tester.contains("nohup") && tester.contains("disown") && tester.contains('&'),
            "must forbid the concrete backgrounding mechanisms"
        );
        assert!(
            tester.contains("pkill -f"),
            "must carry the pkill -f warning"
        );
    }

    #[test]
    fn implementer_template_warns_pkill() {
        let implementer = include_str!("../../templates/implementer.md");
        assert!(implementer.contains("pkill -f"));
    }

    #[test]
    fn tester_template_never_routes_budget_exhaustion_to_green() {
        let tester = include_str!("../../templates/tester.md");
        let lower = tester.to_lowercase();
        // Keyed on the verdict the rule assigns, not on its prose: the wording moved once
        // already when `suite.timeout_secs` stopped being a kill.
        let budget_rule = lower
            .lines()
            .find(|l| l.contains("`## result` = `inconclusive`"))
            .expect("budget-exhaustion rule must exist");
        assert!(
            budget_rule.contains("inconclusive"),
            "budget-exhaustion must be reported as inconclusive, got: {budget_rule}"
        );
        assert!(
            !budget_rule.contains("= `skipped`"),
            "budget-exhaustion must not be assigned `skipped` (skipped maps to a green Pass): {budget_rule}"
        );
        // `skipped -> Pass` stays reserved strictly for a repo with no test suite.
        assert!(
            lower.contains("skipped` only when no suite could be found")
                || lower.contains("skipped only when no suite could be found"),
            "skipped must remain reserved for 'no suite could be found'"
        );
        // The verdict the template now mandates for budget exhaustion must gate the ship.
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\ninconclusive\n")),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn guidance_distinguishes_inconclusive_from_fail() {
        let inconclusive = suite_guidance(SuiteOutcome::Inconclusive).to_lowercase();
        assert!(inconclusive.contains("did not run"));
        assert!(!inconclusive.contains("treats suite"));

        let fail = suite_guidance(SuiteOutcome::Fail).to_lowercase();
        assert!(fail.contains("request_changes"));
        assert!(fail.contains("treats suite"));
        assert!(!fail.contains("did not run"));
    }

    #[test]
    fn test_author_template_emits_criterion_ids() {
        let test_author = include_str!("../../templates/test_author.md");
        assert!(
            test_author.contains("AC-1:"),
            "contract format must show the AC-n criterion id shape"
        );
        assert!(
            test_author.contains("verify:"),
            "each criterion must carry a verify: hint"
        );
    }

    #[test]
    fn reviewer_template_sees_plan_and_contract() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("{{plan_body}}"),
            "reviewer must receive the plan"
        );
        assert!(
            reviewer.contains("{{test_contract_body}}"),
            "reviewer must receive the acceptance contract"
        );
    }

    #[test]
    fn reviewer_template_declares_acceptance_block() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("## Acceptance"),
            "reviewer output contract must declare the ## Acceptance section"
        );
        assert!(
            reviewer.contains("unverified"),
            "reviewer must be told the unverified status exists"
        );
    }

    /// The gate is model-authored, so a new test binary it forgets to name passes green
    /// without ever being compiled. Warn on that — but only when the commands actually
    /// select targets, or the warning fires on every `cargo test` and gets tuned out.
    #[test]
    fn coverage_warning_only_fires_on_a_selective_suite() {
        let selective = "## Commands\n- `cargo test --test existing` → exit 0\n";
        assert!(command_is_selective("cargo test --test existing"));
        assert_eq!(
            suite_commands(selective),
            vec!["cargo test --test existing"]
        );

        assert!(suite_commands("## Commands\n- `cargo test` → exit 0\n")
            .iter()
            .all(|c| !command_is_selective(c)));

        assert!(command_names(
            "cargo test --test existing",
            "tests/existing.rs"
        ));
        assert!(!command_names(
            "cargo test --test existing",
            "tests/brand_new.rs"
        ));
        // A stem that merely appears inside a longer word is not coverage.
        assert!(!command_names(
            "cargo test --test existing_more",
            "tests/existing.rs"
        ));
    }

    /// The canonical full-suite command of each ecosystem compiles or collects every new
    /// test file, so warning on one is a false claim that fires every run. This is the
    /// whole risk of the check, and the earlier "any token with a `/`" rule tripped on
    /// `pytest tests/`, `go test ./...` and `./scripts/test.sh` alike.
    #[test]
    fn full_suite_commands_are_never_selective() {
        for cmd in [
            "cargo test",
            "cargo test --workspace",
            "cargo test -p mycrate",
            "cargo nextest run -p spar",
            "pnpm test",
            "pytest",
            "pytest tests/",
            "go test ./...",
            "./scripts/test.sh",
            "cargo test 2>&1 | tee /tmp/suite.log",
            "cd /repo && cargo test",
            "source .venv/bin/activate && pytest",
        ] {
            assert!(!command_is_selective(cmd), "must not be selective: {cmd}");
        }
    }

    #[test]
    fn naming_specific_targets_is_selective() {
        for cmd in [
            "pytest tests/test_users.py",
            "cargo test --test existing",
            "cargo test --lib",
            "cargo test mymod::case",
            "go test ./pkg/thing_test.go",
            // Every stage narrow ⇒ the line is narrow.
            "cargo test --test a && cargo test --test b",
        ] {
            assert!(command_is_selective(cmd), "must be selective: {cmd}");
        }
    }

    /// `a && b` runs both. Judging the line off one stage was wrong in both directions:
    /// a narrow stage beside a full-suite stage is covered, and a `cd` prefix is not a
    /// verdict on the command behind it.
    #[test]
    fn every_stage_must_be_narrow() {
        // `--tests` builds every integration target, so the line covers new files.
        assert!(!command_is_selective(
            "cargo test --lib && cargo test --tests"
        ));
        // A `cd` prefix is setup, not a verdict on the command behind it.
        assert!(command_is_selective("cd repo && cargo test --test foo"));
        assert!(!command_is_selective("cd repo && cargo test"));
        assert!(!command_is_selective("cargo build && cargo test"));
        // Both spellings of the target flags.
        assert!(command_is_selective("cargo test --test=foo"));
        assert!(command_is_selective("cargo test --bin=cli"));
        // A path handed to a flag is not a target.
        assert!(!command_is_selective("pytest -c tests/pytest.ini"));
    }

    /// Only files a suite command could actually name are reportable. A helper module or
    /// a fixture is neither, and a false "not covered" claim is most expensive in the
    /// reviewers' suite report.
    #[test]
    fn only_real_test_targets_are_reportable() {
        assert!(is_test_target("tests/acceptance.rs"));
        assert!(is_test_target("tests/test_users.py"));
        assert!(is_test_target("pkg/thing_test.go"));
        assert!(!is_test_target("tests/common/mod.rs"));
        assert!(!is_test_target("tests/scenarios/plan.rs"));
        assert!(!is_test_target("tests/data/golden.json"));
    }

    #[test]
    fn test_paths_are_recognized_across_stacks() {
        assert!(is_test_path("tests/scenarios/plan.rs"));
        assert!(is_test_path("src/__tests__/thing.ts"));
        assert!(is_test_path("pkg/thing_test.go"));
        assert!(is_test_path("app/foo.spec.ts"));
        assert!(is_test_path("api/test_users.py"));
        assert!(!is_test_path("src/executor.rs"));
        assert!(!is_test_path("docs/testing.md"));
    }

    /// Automatic reclaim is for runs nothing can resume. A quota pause is meant to be
    /// picked back up when the bucket refills, and node_modules is not free to regenerate
    /// -- it needs the network and an agent that knows to reinstall.
    #[test]
    fn auto_reclaim_spares_runs_that_can_be_resumed() {
        for phase in [Phase::Done, Phase::PlanRejected, Phase::Escalated] {
            assert!(auto_reclaimable(phase), "{phase:?}");
        }
        for phase in [
            Phase::Quota,
            Phase::Stuck,
            Phase::Failed,
            Phase::Stopped,
            Phase::PlanApproved,
            Phase::AwaitingShipConfirm,
            Phase::Review,
        ] {
            assert!(!auto_reclaimable(phase), "{phase:?} is resumable or live");
        }
    }

    #[test]
    fn reviewer_template_uses_suite_body() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("{{suite_body}}"),
            "suite_body is seeded in base_vars and must be referenced"
        );
    }

    #[test]
    fn reviewer_template_requires_the_judged_commit() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("Reviewed-Commit:"),
            "the verdict must name the commit it judged"
        );
        assert!(
            reviewer.contains("git rev-parse HEAD"),
            "the reviewer must be told how to learn HEAD"
        );
        assert!(
            reviewer.contains("cannot be counted"),
            "the template must say plainly why a commit-less verdict is not a verdict"
        );
    }
}

#[cfg(test)]
mod reviewed_commit_gate_tests {
    use super::{decide_reviewed_commit, ReviewedCommitDecision};
    use crate::workflow::review_result::parse_review;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn decide(head: Option<&str>, body: &str) -> ReviewedCommitDecision {
        decide_reviewed_commit(head, &parse_review(body))
    }

    #[test]
    fn full_sha_match_is_a_verdict() {
        let body = format!("## Verdict\napprove\n\nReviewed-Commit: {HEAD}\n");
        assert_eq!(decide(Some(HEAD), &body), ReviewedCommitDecision::Match);
    }

    #[test]
    fn short_prefix_match_is_a_verdict() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: 0123456\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Match);
    }

    #[test]
    fn full_mismatch_is_not_a_verdict() {
        let body =
            "## Verdict\napprove\n\nReviewed-Commit: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Mismatch);
    }

    #[test]
    fn short_non_prefix_is_not_a_verdict() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: deadbee\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Mismatch);
    }

    #[test]
    fn absent_line_is_not_a_verdict() {
        assert_eq!(
            decide(Some(HEAD), "## Verdict\napprove\n"),
            ReviewedCommitDecision::Absent
        );
    }

    #[test]
    fn malformed_line_is_absence_not_mismatch() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: <full-sha-of-HEAD>\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Absent);
    }

    #[test]
    fn unresolvable_panel_head_skips_either_way() {
        assert_eq!(
            decide(None, "## Verdict\napprove\n"),
            ReviewedCommitDecision::Skipped
        );
        let body = format!("## Verdict\napprove\n\nReviewed-Commit: {HEAD}\n");
        assert_eq!(decide(None, &body), ReviewedCommitDecision::Skipped);
    }
}

#[cfg(test)]
mod contract_freeze_tests {
    use super::FrozenContract;
    use crate::workflow::review_result::parse_contract_criteria;

    const BODY: &str = "## Scenarios\n- [ ] AC-1: a — verify: `x`\n- [ ] AC-2: b — verify: `x`\n";

    /// AC-8.
    #[test]
    fn freeze_captures_the_declared_criteria_and_the_bytes_on_disk() {
        let frozen = FrozenContract::freeze(BODY);
        assert_eq!(frozen.criteria, ["AC-1", "AC-2"]);
        assert_eq!(frozen.body, BODY);
        assert!(
            !frozen.fingerprint.is_empty(),
            "the fingerprint is what `status --json` reports; it cannot be blank"
        );
    }

    /// AC-8.
    #[test]
    fn fingerprint_is_stable_for_the_same_body_and_moves_with_it() {
        let a = FrozenContract::freeze(BODY);
        let b = FrozenContract::freeze(BODY);
        assert_eq!(a.fingerprint, b.fingerprint);
        let amended = format!("{BODY}- [ ] AC-3: c — verify: `x`\n");
        assert_ne!(a.fingerprint, FrozenContract::freeze(&amended).fingerprint);
    }

    /// AC-9.
    #[test]
    fn drift_is_measured_against_the_body_that_was_frozen() {
        let frozen = FrozenContract::freeze(BODY);
        assert!(!frozen.drifted(BODY));
        // Same length, one byte different: a fingerprint that only counted bytes would
        // miss a criterion being reworded in place.
        let same_len = BODY.replace("AC-2: b —", "AC-2: q —");
        assert_eq!(same_len.len(), BODY.len());
        assert!(frozen.drifted(&same_len));
        assert!(frozen.drifted(&format!("{BODY}- [ ] AC-3: added mid-run\n")));
    }

    /// AC-9, AC-10. The invariant that survived the round loop only by accident of
    /// ordering: the
    /// overlay note is appended to the *prompt copy*, and the note names git-ignored
    /// paths, so a file called `AC-99:fixture.json` reads as a criterion declaration.
    /// The frozen criteria must come from the bytes on disk, never from that copy.
    #[test]
    fn overlay_note_on_the_prompt_copy_cannot_reach_the_gate() {
        let frozen = FrozenContract::freeze(BODY);
        let mut prompt_copy = frozen.body.clone();
        prompt_copy.push_str(
            "\n## Not copied (git-ignored in the test-author worktree)\n- `AC-99:fixture.json`\n",
        );
        assert!(
            parse_contract_criteria(&prompt_copy).contains(&"AC-99".to_string()),
            "guard is vacuous unless the note is declaration-shaped"
        );
        assert_eq!(frozen.criteria, ["AC-1", "AC-2"]);
        assert!(!frozen.drifted(BODY), "the prompt copy is not the contract");
    }
}

#[cfg(test)]
mod carry_forward_tests {
    use super::*;

    fn brief_of(round: u32, blockers: &[Blocker], brief: Option<&str>, max: usize) -> String {
        carry_forward_section(round, &[], blockers, brief, max)
    }

    fn note(s: &str) -> Blocker {
        Blocker::Note(s.to_string())
    }

    /// An operator's persisted message leads the section and survives the cap that
    /// eats the slot's own notes: a relaunch must not drop it (O135).
    #[test]
    fn persisted_messages_lead_and_survive_truncation() {
        let persisted = vec!["from human (message abc): merge main first".to_string()];
        let essay = "my own plan, which differs\n".repeat(400);
        let out = carry_forward_section(2, &persisted, &[], Some(&essay), 1200);
        let ruling = out
            .find("merge main first")
            .expect("persisted message kept");
        assert!(
            ruling < out.find("my own plan").unwrap_or(usize::MAX),
            "{out}"
        );
        assert!(out.contains("Operator messages"), "{out}");
        assert!(!carry_forward_section(1, &persisted, &[], None, 4000).is_empty());
    }

    /// A first round has nothing to carry, and an empty section must render as nothing
    /// at all — an empty "## Carry-forward" heading is a prompt telling the model there
    /// was a previous round when there was not.
    #[test]
    fn nothing_to_carry_renders_nothing() {
        assert_eq!(brief_of(2, &[], None, 4000), "");
    }

    /// The whole point: the failed criterion and its evidence reach the next round's
    /// implementer, which before this only ever went to the bus.
    #[test]
    fn blockers_name_the_failed_criterion() {
        let blockers = vec![note(
            "acceptance (review `rev-1`): AC-2: fail - no test exists",
        )];
        let out = brief_of(4, &blockers, None, 4000);
        assert!(out.contains("AC-2: fail - no test exists"), "{out}");
        assert!(out.contains("round 4"), "{out}");
    }

    /// The cap is on the whole section, and blockers lead, so a slot that wrote an essay
    /// cannot push the machine-known half out of the prompt.
    #[test]
    fn truncation_eats_the_slot_prose_not_the_blockers() {
        let blockers = vec![note(
            "acceptance (review `rev-1`): AC-2: fail - missing guard",
        )];
        let essay = "padding line that goes on and on\n".repeat(400);
        assert!(
            essay.len() > 4000,
            "guard is vacuous unless the brief is long"
        );
        let out = brief_of(3, &blockers, Some(&essay), 1200);
        assert!(out.chars().count() <= 1400, "capped, got {}", out.len());
        assert!(out.contains("AC-2: fail - missing guard"), "{out}");
        assert!(
            out.contains("truncated by spar"),
            "a silent cut is a lie:\n{out}"
        );
    }

    /// Two rounds of the same size must produce the same size. A section that appended
    /// would recreate the quadratic context growth it exists to avoid.
    #[test]
    fn the_section_does_not_grow_with_the_round_number() {
        let blockers = vec![note("acceptance (review `rev-1`): AC-1: fail - x")];
        let brief = "- touched src/a.rs: added the guard\n- rejected: a feature flag\n";
        let round_3 = brief_of(3, &blockers, Some(brief), 4000);
        let round_9 = brief_of(9, &blockers, Some(brief), 4000);
        assert_eq!(round_3.len(), round_9.len());
    }

    /// The measured defect: `acceptance_block_reasons` used to join every failing
    /// criterion into one line, real reviews produced 4k-10k of it, and a line-boundary
    /// truncation drops an over-cap line **whole** — leaving a bare "What blocked the
    /// ship last round" heading, which reads as "nothing blocked".
    #[test]
    fn one_giant_blocker_never_leaves_an_empty_heading() {
        let huge = format!(
            "acceptance (review `rev-1`): AC-2: fail - {}",
            "x".repeat(9000)
        );
        let out = brief_of(4, &[note(&huge)], None, 1000);
        assert!(
            out.contains("### What blocked the ship last round"),
            "{out}"
        );
        let after_heading = out
            .split_once("### What blocked the ship last round\n")
            .map(|(_, rest)| rest.trim().to_string())
            .unwrap_or_default();
        assert!(
            after_heading.starts_with("These reviews are the current verdict"),
            "the sentence renders once under the heading:\n{out}"
        );
        assert!(
            after_heading.contains("- acceptance"),
            "the heading must still carry its bullet:\n{out}"
        );
        assert!(after_heading.contains("AC-2: fail"), "{out}");
    }

    /// Many failing criteria on the non-blocking path (a review that does not
    /// itself block but fails acceptance) stay separately visible instead of
    /// collapsing into one line that the cap then eats entirely. Blocking
    /// reviews share one bullet per review with an id manifest up front.
    #[test]
    fn each_nonblocking_criterion_is_its_own_bullet() {
        let blockers: Vec<Blocker> = (1..=6)
            .map(|n| {
                note(&format!(
                    "acceptance (review `rev-1`): AC-{n}: fail - evidence {n}"
                ))
            })
            .collect();
        let out = brief_of(3, &blockers, None, 4000);
        for n in 1..=6 {
            assert!(
                out.contains(&format!("AC-{n}: fail")),
                "AC-{n} missing:\n{out}"
            );
        }
    }

    #[test]
    fn clamp_line_collapses_and_caps() {
        assert_eq!(clamp_line("short", 40), "short");
        assert_eq!(clamp_line("a\nb", 40), "a b");
        let out = clamp_line(&"y".repeat(100), 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("..."), "{out}");
    }

    #[test]
    fn truncate_keeps_whole_lines() {
        let out = truncate_at_line("alpha\nbravo\ncharlie\n", 9);
        assert!(out.starts_with("alpha\n"), "{out}");
        assert!(!out.contains("charlie"), "{out}");
    }

    /// Consumed on read: a round whose implementer died writes nothing, and must inherit
    /// nothing, rather than a brief describing a worktree two rounds stale.
    #[test]
    fn carry_forward_is_consumed_on_read() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let f = paths.artifact("r1", "carry-forward-impl-cli-claude.md");
        std::fs::write(&f, "- touched src/a.rs\n").unwrap();

        let first = take_carry_forward(&paths, "r1", "impl-cli-claude");
        assert_eq!(first.as_deref(), Some("- touched src/a.rs"));
        assert!(!f.exists(), "the brief must not survive its own round");
        assert!(take_carry_forward(&paths, "r1", "impl-cli-claude").is_none());
    }

    /// The round bought at a gate is a fresh process with an empty `blockers` list, so
    /// the list is rebuilt from the review artifacts rather than carried in memory.
    #[test]
    fn blockers_are_recovered_from_the_review_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        std::fs::write(
            paths.artifact("r1", "review-rev-1.md"),
            "## Verdict\nrequest_changes\n\n## Acceptance\nAC-1: fail - no guard exists\n",
        )
        .unwrap();

        let out = blockers_from_disk(&state, &paths, &["AC-1".to_string()], &Config::default());
        let verdict = out.iter().find_map(|b| match b {
            Blocker::Verdict(v) => Some(v),
            Blocker::Note(_) => None,
        });
        let verdict = verdict.expect("a verdict blocker: {out:?}");
        assert!(
            verdict
                .acceptance
                .iter()
                .any(|a| a.contains("AC-1:") && a.contains("no guard exists")),
            "{verdict:?}"
        );
        assert!(verdict.head.contains("requested changes"), "{verdict:?}");
        assert!(verdict.head.contains("(round "), "{verdict:?}");
        assert!(
            verdict.head.contains("no Reviewed-Commit recorded"),
            "{verdict:?}"
        );
        assert!(
            verdict.head.contains("failing AC-1"),
            "a verdict with no findings still anchors round and commit state, \
             carrying its failing criterion: {verdict:?}"
        );
    }

    /// A stale *approval* is not a blocker; only the live round's review can clear or
    /// re-raise the gate.
    #[test]
    fn an_approving_review_seeds_no_blockers() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        std::fs::write(
            paths.artifact("r1", "review-rev-1.md"),
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass — done\n",
        )
        .unwrap();
        assert!(
            blockers_from_disk(&state, &paths, &["AC-1".to_string()], &Config::default())
                .is_empty()
        );
    }

    /// `artifacts_dir` is shared per run and arena spawns N implementers at once, so the
    /// brief has to be slot-scoped or they clobber each other.
    #[test]
    fn briefs_are_scoped_per_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        std::fs::write(paths.artifact("r1", "carry-forward-a.md"), "from a\n").unwrap();
        std::fs::write(paths.artifact("r1", "carry-forward-b.md"), "from b\n").unwrap();
        assert_eq!(
            take_carry_forward(&paths, "r1", "a").as_deref(),
            Some("from a")
        );
        assert_eq!(
            take_carry_forward(&paths, "r1", "b").as_deref(),
            Some("from b")
        );
    }
    /// A minors-only `request_changes` on disk is not a blocker for the next round
    /// either: `blockers_from_disk` uses the same rule as the live gate (O118).
    #[test]
    fn a_minors_only_review_seeds_no_verdict_blocker() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        std::fs::write(
            paths.artifact("r1", "review-rev-1.md"),
            "## Verdict\nrequest_changes\n\n## Acceptance\nAC-1: pass — done\n\n## Findings\n- severity: minor — stale comment at src/a.rs:3\n",
        )
        .unwrap();
        let ac = ["AC-1".to_string()];
        assert!(blockers_from_disk(&state, &paths, &ac, &Config::default()).is_empty());
        let mut strict = Config::default();
        strict.review.block_on = review_result::Severity::Minor;
        assert_eq!(blockers_from_disk(&state, &paths, &ac, &strict).len(), 1);
    }

    /// The carry-forward names the round that wrote the blocking review, the
    /// commit it judged, and its open findings (not just the review's file path)
    /// and says plainly the reviews are the current verdict, not stale notes. The
    /// sentence lives once in the section, the anchor and findings in each bullet,
    /// so a multi-review panel fits under a tight budget with the brief.
    #[test]
    fn verdict_blocker_names_round_commit_and_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        state.review_history.push(crate::state::ReviewHistoryEntry {
            round: 3,
            slot_id: "rev-1".to_string(),
            review: review_result::parse_review(
                "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Findings\n- severity: major - the retry at src/net.rs:40 never stops\n",
            ),
        });
        std::fs::write(
            paths.artifact("r1", "review-rev-1.md"),
            "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Acceptance\nAC-1: fail - no guard exists\n\n## Findings\n- severity: major - the retry at src/net.rs:40 never stops\n",
        )
        .unwrap();

        let out = blockers_from_disk(&state, &paths, &["AC-1".to_string()], &Config::default());
        assert_eq!(out.len(), 1, "one bullet per review: {out:?}");
        let Blocker::Verdict(verdict) = &out[0] else {
            panic!("a verdict blocker: {out:?}");
        };
        assert!(
            verdict.head.contains("(round 3, Reviewed-Commit"),
            "{verdict:?}"
        );
        assert!(verdict.head.contains("requested changes"), "{verdict:?}");
        assert!(
            verdict
                .head
                .contains("(round 3, Reviewed-Commit abcdef123456)"),
            "{verdict:?}"
        );
        assert!(
            verdict
                .findings
                .iter()
                .any(|f| f.contains("major - the retry at src/net.rs:40 never stops")),
            "{verdict:?}"
        );
        assert!(
            verdict
                .acceptance
                .iter()
                .any(|a| a.contains("AC-1:") && a.contains("no guard exists")),
            "the review's failing criterion rides in its own bullet: {verdict:?}"
        );

        let section = carry_forward_section(4, &[], &out, None, 4000);
        assert!(
            section.contains("current verdict on the implementer's HEAD"),
            "{section}"
        );
        assert!(
            section.contains("supersede the previous round's own notes"),
            "{section}"
        );
        assert_eq!(
            section.matches("current verdict").count(),
            1,
            "the sentence renders once per section, not per bullet:\n{section}"
        );
    }

    /// The in-loop gate feeds the just-parsed review straight into the same
    /// builder the disk path uses: with no history entry yet, the anchor falls
    /// back to the slot's own round and the review's own commit.
    #[test]
    fn verdict_blocker_falls_back_to_slot_anchor_without_history() {
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/nonexistent".into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        let res = review_result::parse_review(
            "## Verdict\nrequest_changes\n\nReviewed-Commit: 1234567890abcdef1234\n\n## Findings\n- severity: major - nothing was changed at src/main.rs:9\n",
        );
        let rev = state.slots[0].clone();
        let out = verdict_blocker(
            &state,
            &rev,
            &res,
            &[AcceptanceReason {
                id: "AC-9".to_string(),
                reason: "fail - seized".to_string(),
            }],
            std::path::Path::new("review-rev-1.md"),
        );
        assert!(out.head.contains("requested changes"), "{out:?}");
        assert!(
            out.head.contains(&format!(
                "(round {}, Reviewed-Commit 1234567890ab)",
                rev.round
            )),
            "{out:?}"
        );
        assert!(
            out.findings
                .iter()
                .any(|f| f.contains("major - nothing was changed at src/main.rs:9")),
            "{out:?}"
        );
        assert!(
            out.acceptance
                .iter()
                .any(|a| a.contains("AC-9: fail - seized")),
            "the review's failing criterion rides in its own bullet: {out:?}"
        );
    }

    /// Findings ride along bounded: at most five, one line each.
    #[test]
    fn verdict_blocker_bounds_findings() {
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/nonexistent".into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        let mut body = String::from(
            "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Findings\n",
        );
        for i in 1..=7 {
            body.push_str(&format!(
                "- severity: major - finding number {i}\n  with a wrapped second line that must not survive\n"
            ));
        }
        let res = review_result::parse_review(&body);
        let rev = state.slots[0].clone();
        let out = verdict_blocker(
            &state,
            &rev,
            &res,
            &[],
            std::path::Path::new("review-rev-1.md"),
        );
        assert_eq!(out.findings.len(), 5, "{out:?}");
        assert!(
            out.findings.iter().all(|f| !f.contains('\n')),
            "one line per finding: {out:?}"
        );
        assert_eq!(out.dropped_findings, 2, "{out:?}");
        let section = carry_forward_section(2, &[], &[Blocker::Verdict(out)], None, 4000);
        assert!(section.contains("(+2 more)"), "{section}");
    }

    /// Failing criteria lead the bullet, so five long findings cannot push them
    /// out at the default cap, and long evidence cannot either: three failing
    /// criteria with ~300 chars of evidence each used to join into a ~1000-char
    /// bullet whose clamp ate AC-3.
    #[test]
    fn verdict_blocker_keeps_failing_criteria_before_long_findings() {
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/nonexistent".into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        let mut body = String::from(
            "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Findings\n",
        );
        for i in 1..=5 {
            body.push_str(&format!(
                "- severity: major - finding number {i} {}\n",
                "x".repeat(200)
            ));
        }
        let res = review_result::parse_review(&body);
        let rev = state.slots[0].clone();
        let acceptance: Vec<AcceptanceReason> = (1..=3)
            .map(|n| AcceptanceReason {
                id: format!("AC-{n}"),
                reason: format!("fail: {}", "e".repeat(300)),
            })
            .collect();
        let out = verdict_blocker(
            &state,
            &rev,
            &res,
            &acceptance,
            std::path::Path::new("review-rev-1.md"),
        );
        assert!(
            out.head.contains("failing AC-1, AC-2, AC-3"),
            "the manifest names every failing id up front: {out:?}"
        );
        let section = carry_forward_section(2, &[], &[Blocker::Verdict(out)], None, 4000);
        for n in 1..=3 {
            assert!(
                section.contains(&format!("AC-{n}")),
                "AC-{n} survives the default cap:\n{section}"
            );
        }
        assert!(
            section.contains("failing AC-1, AC-2, AC-3"),
            "the manifest survives the default cap:\n{section}"
        );
    }

    /// A blocking review with no tagged findings still names its artifact and
    /// surfaces its untagged remarks, bounded like the rest.
    #[test]
    fn verdict_blocker_names_artifact_and_untagged_findings() {
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/nonexistent".into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        let res = review_result::parse_review(
            "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Findings\n- the retry never stops\n- severity: major - tagged finding\n",
        );
        assert_eq!(res.untagged, ["the retry never stops"]);
        let rev = state.slots[0].clone();
        let out = verdict_blocker(
            &state,
            &rev,
            &res,
            &[],
            std::path::Path::new("review-rev-1.md"),
        );
        let section = carry_forward_section(2, &[], &[Blocker::Verdict(out)], None, 4000);
        assert!(
            section.contains("full review: `review-rev-1.md`"),
            "the carry-forward names the artifact:\n{section}"
        );
        assert!(
            section.contains("untagged - the retry never stops"),
            "untagged remarks reach the implementer:\n{section}"
        );
        assert!(
            section.contains("major - tagged finding"),
            "tagged findings still render:\n{section}"
        );
    }

    /// Acceptance evidence renders past the old fixed clamp when the cap
    /// leaves budget: one failing criterion keeps its full reason.
    #[test]
    fn acceptance_evidence_scales_with_budget() {
        let blockers = vec![verdict_with_ac(1, 1, 300)];
        let out = carry_forward_section(3, &[], &blockers, None, 4000);
        assert!(
            out.contains(&"e".repeat(300)),
            "budget left means evidence renders whole:\n{out}"
        );
    }

    /// Under a cap even the heads exceed, the first bullet still renders
    /// (clamped) with a truncation notice instead of leaving the heading bare.
    #[test]
    fn first_bullet_survives_a_cap_the_heads_exceed() {
        let blockers = vec![verdict_with_ac(1, 20, 50)];
        let out = carry_forward_section(3, &[], &blockers, None, 380);
        let after_heading = out
            .split_once("### What blocked the ship last round\n")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
        assert!(
            after_heading.contains("- review `rev-1`"),
            "the first bullet survives:\n{out}"
        );
        assert!(
            after_heading.contains("carry-forward truncated"),
            "the cut is disclosed, not silent:\n{out}"
        );
    }

    /// Five blocking reviews with five long findings each: at the default cap
    /// the section keeps every failing criterion id and its evidence, cutting
    /// findings instead. Findings used to share one bullet with the criteria,
    /// so a multi-review panel pushed the criteria past the cap.
    #[test]
    fn five_reviews_keep_every_failing_criterion_at_the_default_cap() {
        let blockers: Vec<Blocker> = (1..=5)
            .map(|r| {
                Blocker::Verdict(VerdictBlocker {
                    review_id: format!("rev-{r}"),
                    review_path: format!("review-rev-{r}.md"),
                    head: format!(
                        "review `rev-{r}` requested changes (round 2, Reviewed-Commit abcdef123456): failing AC-1, AC-2, AC-3, AC-4, AC-5"
                    ),
                    compact: format!(
                        "review `rev-{r}` (round 2): failing AC-1, AC-2, AC-3, AC-4, AC-5"
                    ),
                    acceptance: (1..=5)
                        .map(|n| format!("AC-{n}: fail: evid-r{r}c{n} {}", "e".repeat(300)))
                        .collect(),
                    findings: (1..=5)
                        .map(|n| format!("major - finding {n} {}", "x".repeat(100)))
                        .collect(),
                    dropped_findings: 0,
                })
            })
            .collect();
        let out = carry_forward_section(3, &[], &blockers, None, 4000);
        for r in 1..=5 {
            for n in 1..=5 {
                assert!(
                    out.contains(&format!("evid-r{r}c{n}")),
                    "review {r} AC-{n} evidence survives the default cap:\n{out}"
                );
            }
        }
        assert_eq!(
            out.matches("failing AC-1, AC-2, AC-3, AC-4, AC-5").count(),
            5,
            "every review keeps its manifest:\n{out}"
        );
    }

    fn verdict_with_ac(review: u32, criteria: u32, evidence_len: usize) -> Blocker {
        let ids: Vec<String> = (1..=criteria).map(|n| format!("AC-{n}")).collect();
        Blocker::Verdict(VerdictBlocker {
            review_id: format!("rev-{review}"),
            review_path: format!("review-rev-{review}.md"),
            head: format!(
                "review `rev-{review}` requested changes (round 2, Reviewed-Commit abcdef123456): failing {}",
                ids.join(", ")
            ),
            compact: format!(
                "review `rev-{review}` (round 2): failing {}",
                ids.join(", ")
            ),
            acceptance: (1..=criteria)
                .map(|n| {
                    format!(
                        "AC-{n}: fail: evid-r{review}c{n} {}",
                        "e".repeat(evidence_len)
                    )
                })
                .collect(),
            findings: Vec::new(),
            dropped_findings: 0,
        })
    }

    /// Persisted operator messages render before the reserved block, but the
    /// reservation must still hold every review: two long operator messages
    /// beside five failing reviews keep all five manifests at the default cap.
    #[test]
    fn persisted_messages_do_not_evict_blocking_reviews() {
        let blockers: Vec<Blocker> = (1..=5).map(|r| verdict_with_ac(r, 5, 300)).collect();
        let persisted = vec!["m1".to_string(), "m2".to_string()]
            .into_iter()
            .map(|m| format!("from human (message {m}): {}", "o".repeat(2000)))
            .collect::<Vec<_>>();
        let out = carry_forward_section(3, &persisted, &blockers, None, 4000);
        assert!(
            out.contains("from human (message m1)"),
            "the first operator message renders clamped, never withheld:\n{out}"
        );
        assert!(
            out.contains("+1 more operator messages withheld"),
            "the over-budget message is marked withheld, not silently dropped:\n{out}"
        );
        for r in 1..=5 {
            for n in 1..=5 {
                assert!(
                    out.contains(&format!("evid-r{r}c{n}")),
                    "review {r} AC-{n} evidence survives beside operator messages:\n{out}"
                );
            }
        }
        assert_eq!(
            out.matches("failing AC-1, AC-2, AC-3, AC-4, AC-5").count(),
            5,
            "every review keeps its manifest beside operator messages:\n{out}"
        );
    }

    /// A single operator message longer than the phase budget renders clamped:
    /// the bullet overhead used to push it over the budget outright.
    #[test]
    fn a_single_long_operator_message_renders_clamped() {
        let out = carry_forward_section(3, &["o".repeat(1200)], &[], None, 4000);
        assert!(
            out.contains(&"o".repeat(994)),
            "the message head survives the phase clamp:\n{out}"
        );
        assert!(!out.contains("withheld"), "nothing is withheld:\n{out}");
    }

    /// When even manifest-only full heads exceed the reservation, the block
    /// degrades to compact heads that keep every review id, round, and
    /// failing-ids manifest, rather than dropping whole reviews to the tail
    /// cut. Five reviews keep all five manifests at a cap of 800.
    #[test]
    fn heads_beyond_the_reservation_degrade_to_compact() {
        let blockers: Vec<Blocker> = (1..=5).map(|r| verdict_with_ac(r, 5, 300)).collect();
        let out = carry_forward_section(3, &[], &blockers, None, 800);
        assert_eq!(
            out.matches("failing AC-1, AC-2, AC-3, AC-4, AC-5").count(),
            5,
            "every review keeps its manifest at a small cap:\n{out}"
        );
        assert!(
            out.contains("review `rev-5` (round 2)"),
            "the last review degrades instead of dropping:\n{out}"
        );
    }

    /// `verdict_blocker` keeps the artifact's file name, never its absolute
    /// path: the implementer prompt already prints the artifacts dir.
    #[test]
    fn review_artifacts_render_as_bare_names() {
        let abs = Path::new(
            "/home/operator/projects/acme-widget-factory/.spar/runs/3dd3f29d-01kn6v/artifacts/review-rev-1.md",
        );
        assert!(abs.display().to_string().chars().count() > 90);
        assert_eq!(review_artifact_name(abs), "review-rev-1.md");
    }

    /// The `full review:` lines name the artifact's file, not its absolute
    /// path: two blocking reviews beside a realistic run dir keep their
    /// failing criterion's evidence, both artifact lines and the slot's own
    /// brief at a 800-char cap. Absolute paths used to tip `manifest_only`
    /// and evict the evidence and the brief here.
    #[test]
    fn bare_review_names_keep_evidence_and_brief_at_a_tight_cap() {
        let run_dir = "/home/operator/projects/acme-widget-factory/.spar/runs/3dd3f29d-01kn6v";
        let blockers: Vec<Blocker> = (1..=2)
            .map(|r| {
                let mut b = verdict_with_ac(r, 1, 20);
                if let Blocker::Verdict(v) = &mut b {
                    v.review_path = review_artifact_name(Path::new(&format!(
                        "{run_dir}/artifacts/review-rev-{r}.md"
                    )));
                }
                b
            })
            .collect();
        let brief = format!(
            "SENTINEL-BRIEF\n{}\n",
            "padding that a real slot would write\n".repeat(20)
        );
        let out = carry_forward_section(3, &[], &blockers, Some(&brief), 800);
        assert_eq!(
            out.matches("failing AC-1").count(),
            2,
            "both reviews keep their manifests:\n{out}"
        );
        for r in 1..=2 {
            assert!(
                out.contains(&format!("evid-r{r}c1")),
                "review {r} evidence survives:\n{out}"
            );
            assert!(
                out.contains(&format!("full review: `review-rev-{r}.md`")),
                "review {r} keeps its artifact line as a bare name:\n{out}"
            );
        }
        assert!(
            !out.contains(run_dir),
            "no absolute run path leaks into the section:\n{out}"
        );
        assert!(
            out.contains("SENTINEL-BRIEF"),
            "the slot's own brief survives beside the blockers:\n{out}"
        );
        assert!(
            out.contains("truncated by spar at 800"),
            "the tail cut discloses itself where it happened:\n{out}"
        );
    }

    /// The per-line share floors at 32 characters; when even floored lines do
    /// not fit the reservation, the block degrades to manifest-only heads
    /// rather than dropping whole reviews. Five reviews with twenty failing
    /// criteria each keep all five manifests at the default cap.
    #[test]
    fn an_overflowing_share_degrades_to_manifest_only_heads() {
        let blockers: Vec<Blocker> = (1..=5).map(|r| verdict_with_ac(r, 20, 50)).collect();
        let out = carry_forward_section(3, &[], &blockers, None, 4000);
        let manifest = (1..=20)
            .map(|n| format!("AC-{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            out.matches(&format!("failing {manifest}")).count(),
            5,
            "every review keeps its manifest when the share overflows:\n{out}"
        );
    }

    /// A git repo with one commit, for the no-progress matrix below.
    fn stalled_repo() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .status()
                .unwrap();
        }
        std::fs::write(tmp.path().join("work.txt"), "v1\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        let head = worktree::git_out(tmp.path(), &["rev-parse", "HEAD"]).unwrap();
        (tmp, head)
    }

    /// History for a round-1 panel that judged `head` and asked for changes.
    fn stalled_history(head: &str, verdict_body: &str) -> crate::state::ReviewHistoryEntry {
        crate::state::ReviewHistoryEntry {
            round: 1,
            slot_id: "rev-1".to_string(),
            review: review_result::parse_review(&verdict_body.replace("{head}", head)),
        }
    }

    const STALLED_RC: &str = "## Verdict\nrequest_changes\n\nReviewed-Commit: {head}\n\n## Findings\n- severity: major - the retry never stops\n";

    /// HEAD and the tree unchanged while a `request_changes` review against that
    /// same commit is open is a no-progress round, and the warning names the
    /// review, its round and commit, and the open findings.
    #[test]
    fn unchanged_head_and_tree_with_open_review_is_no_progress() {
        let (tmp, head) = stalled_repo();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 2;
        state
            .review_history
            .push(stalled_history(&head, STALLED_RC));

        let stalled = detect_no_progress(
            &state,
            &paths,
            &Config::default(),
            &[],
            tmp.path(),
            Some(&head),
        )
        .unwrap();
        assert_eq!(stalled.head, head);
        assert_eq!(stalled.reviews.len(), 1);
        let prompt = stalled.prompt();
        assert!(prompt.contains("## No-progress warning"), "{prompt}");
        assert!(prompt.contains("`rev-1`"), "{prompt}");
        assert!(prompt.contains("(round 1, Reviewed-Commit"), "{prompt}");
        assert!(prompt.contains(&head), "{prompt}");
        assert!(prompt.contains("major - the retry never stops"), "{prompt}");
    }

    /// A short Reviewed-Commit prefix still anchors the stall detector: the
    /// ship gate accepts 7-39 char prefixes, so the detector must too.
    #[test]
    fn short_reviewed_commit_prefix_still_stalls() {
        let (tmp, head) = stalled_repo();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 2;
        state
            .review_history
            .push(stalled_history(&head[..12], STALLED_RC));
        let stalled = detect_no_progress(
            &state,
            &paths,
            &Config::default(),
            &[],
            tmp.path(),
            Some(&head),
        )
        .expect("a 12-char prefix of HEAD is that commit");
        assert_eq!(stalled.reviews.len(), 1);
    }

    /// An approval with a failing acceptance criterion still blocks the ship
    /// (O19), so an unchanged HEAD under it is a no-progress round too.
    #[test]
    fn approve_with_failing_acceptance_is_no_progress() {
        let (tmp, head) = stalled_repo();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 2;
        state.review_history.push(stalled_history(
            &head,
            "## Verdict\napprove\n\nReviewed-Commit: {head}\n\n## Acceptance\nAC-1: fail - the retry loop never terminates\n",
        ));
        let criteria = vec!["AC-1".to_string()];
        let stalled = detect_no_progress(
            &state,
            &paths,
            &Config::default(),
            &criteria,
            tmp.path(),
            Some(&head),
        )
        .expect("an approve with failing acceptance still blocks the ship");
        assert_eq!(stalled.reviews.len(), 1);
    }

    /// `--re-review` is a one-shot: taking it clears the arm and restarts the
    /// stall counter, and a second take finds nothing.
    #[test]
    fn re_review_override_is_one_shot_and_restarts_the_stall_count() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        assert!(!take_panel_rerun(&mut state));
        state.panel_rerun = true;
        state.no_progress_rounds = 1;
        state.no_progress_warning = Some("w".into());
        assert!(take_panel_rerun(&mut state));
        assert!(!state.panel_rerun);
        assert_eq!(state.no_progress_rounds, 0);
        assert!(state.no_progress_warning.is_none());
        assert!(!take_panel_rerun(&mut state));
    }

    /// Anything the implementer committed, wrote, or earned counts as progress.
    #[test]
    fn progress_or_no_open_review_is_not_no_progress() {
        let (tmp, head) = stalled_repo();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 2;
        state
            .review_history
            .push(stalled_history(&head, STALLED_RC));
        let cfg = Config::default();

        // First round: nothing open yet.
        let mut first = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        first.round = 1;
        assert!(
            detect_no_progress(&first, &paths, &cfg, &[], tmp.path(), Some(&head)).is_none(),
            "no previous panel, no stall"
        );

        // The implementer moved HEAD.
        std::fs::write(tmp.path().join("work.txt"), "v2\n").unwrap();
        std::process::Command::new("git")
            .args(["commit", "-am", "v2"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(
            detect_no_progress(&state, &paths, &cfg, &[], tmp.path(), Some(&head)).is_none(),
            "a moved HEAD is progress"
        );

        // Back on the judged commit, but the tree is dirty.
        std::process::Command::new("git")
            .args(["reset", "--hard", &head])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::fs::write(tmp.path().join("draft.txt"), "uncommitted\n").unwrap();
        assert!(
            detect_no_progress(&state, &paths, &cfg, &[], tmp.path(), Some(&head)).is_none(),
            "uncommitted work still flows to the panel"
        );
        std::fs::remove_file(tmp.path().join("draft.txt")).unwrap();

        // The panel approved the commit.
        let mut approved = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        approved.round = 2;
        approved.review_history.push(stalled_history(
            &head,
            "## Verdict\napprove\n\nReviewed-Commit: {head}\n",
        ));
        assert!(
            detect_no_progress(&approved, &paths, &cfg, &[], tmp.path(), Some(&head)).is_none(),
            "an approval is not an open review"
        );

        // The open review judged a different commit.
        let mut other = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        other.round = 2;
        other.review_history.push(stalled_history(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            STALLED_RC,
        ));
        assert!(
            detect_no_progress(&other, &paths, &cfg, &[], tmp.path(), Some(&head)).is_none(),
            "a review against another commit does not stall this one"
        );
    }

    /// The second consecutive no-progress round parks at the existing human gate
    /// (exit 2), with the unaddressed findings in the reason.
    #[test]
    fn second_consecutive_no_progress_parks_at_the_human_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 2;
        let stalled = NoProgress {
            head: "abcdef1234567".to_string(),
            reviews: vec![NoProgressReview {
                slot_id: "rev-1".to_string(),
                round: 1,
                reviewed_commit: "abcdef1234567".to_string(),
                acceptance: vec![],
                findings: vec!["major - the retry never stops".to_string()],
            }],
        };
        park_no_progress(&mut state, &paths, &stalled).unwrap();
        assert_eq!(state.phase, Phase::AwaitingRoundExtension);
        assert_eq!(state.exit_code(), crate::exit_codes::ExitCode::HumanGate);
        let err = state.error.clone().unwrap();
        assert!(err.contains("no progress in two consecutive"), "{err}");
        assert!(err.contains("`rev-1`"), "{err}");
        assert!(err.contains("abcdef1234567"), "{err}");
        let escalation = std::fs::read_to_string(paths.artifact("r1", "escalation.md")).unwrap();
        assert!(
            escalation.contains("## No-progress warning"),
            "{escalation}"
        );
        assert!(
            escalation.contains("major - the retry never stops"),
            "{escalation}"
        );
    }

    fn stalled_once(
        round: u32,
        max_rounds: u32,
    ) -> (tempfile::TempDir, SparPaths, RunState, NoProgress) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = round;
        state.max_rounds = max_rounds;
        let stalled = NoProgress {
            head: "abcdef1234567".to_string(),
            reviews: vec![NoProgressReview {
                slot_id: "rev-1".to_string(),
                round: 1,
                reviewed_commit: "abcdef1234567".to_string(),
                acceptance: vec![],
                findings: vec!["major - the retry never stops".to_string()],
            }],
        };
        (tmp, paths, state, stalled)
    }

    /// Below the ceiling the park message re-enters with the bare command: no
    /// flag is needed because the stall gate is not the round ceiling.
    #[test]
    fn stall_park_below_the_ceiling_re_enters_bare() {
        let (_tmp, paths, mut state, stalled) = stalled_once(2, 8);
        park_no_progress(&mut state, &paths, &stalled).unwrap();
        let err = state.error.clone().unwrap();
        assert!(err.contains("`spar implement --run r1`"), "{err}");
        assert!(!err.contains("--max-rounds"), "{err}");
    }

    /// Extending a stall park below the ceiling never lowers it: at round 3 of
    /// 8, `+1` resolves to 4 and the gate keeps 8. An uncapped run stays
    /// uncapped; a genuine raise and a fresh launch still apply.
    #[test]
    fn gate_extension_never_lowers_or_installs() {
        assert_eq!(gate_max_rounds(8, 4, true), 8);
        assert_eq!(gate_max_rounds(8, 8, true), 8);
        assert_eq!(gate_max_rounds(0, 6, true), 0);
        assert_eq!(gate_max_rounds(8, 10, true), 10);
        assert_eq!(gate_max_rounds(8, 4, false), 4);
        assert_eq!(gate_max_rounds(0, 6, false), 6);
    }

    /// At the ceiling a bare re-entry would hit the ceiling gate before the
    /// stall logic, so the park message names the flag that re-entry needs.
    #[test]
    fn stall_park_at_the_ceiling_names_max_rounds() {
        let (_tmp, paths, mut state, stalled) = stalled_once(8, 8);
        park_no_progress(&mut state, &paths, &stalled).unwrap();
        let err = state.error.clone().unwrap();
        assert!(
            err.contains("`spar implement --run r1 --max-rounds 10`"),
            "{err}"
        );
    }

    fn scoped_state(round: u32, reviews: &[(&str, &str)]) -> RunState {
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/nonexistent".into());
        state.round = round;
        for (slot, body) in reviews {
            state.review_history.push(crate::state::ReviewHistoryEntry {
                round: round - 1,
                slot_id: slot.to_string(),
                review: review_result::parse_review(body),
            });
        }
        state
    }

    const PRIOR: &str = "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Findings\n- severity: major - the retry at src/net.rs:40 never stops\n\n## Follow-up\n- severity: minor - split the module later\n";

    #[test]
    fn scoped_review_starts_at_its_round_and_carries_the_prior_findings() {
        let cfg = Config::default();
        let cwd = Path::new("/nonexistent");
        assert!(scoped_review_section(&scoped_state(2, &[("rev-1", PRIOR)]), &cfg, cwd).is_empty());

        let s = scoped_review_section(&scoped_state(3, &[("rev-1", PRIOR)]), &cfg, cwd);
        assert!(s.contains("## Scoped re-review (round 3)"), "{s}");
        assert!(
            s.contains("- major `src/net.rs:40` (rev-1): the retry"),
            "{s}"
        );
        assert!(s.contains("diff abcdef1234567..HEAD"), "{s}");
        assert!(s.contains("`## Follow-up`"), "{s}");
        assert!(
            !s.contains("split the module"),
            "follow-ups are not re-judged: {s}"
        );

        let mut off = Config::default();
        off.review.scoped_from_round = 0;
        assert!(scoped_review_section(&scoped_state(5, &[("rev-1", PRIOR)]), &off, cwd).is_empty());
    }

    /// A panel spar had to stand in for, or one whose reviewers judged different
    /// commits, cannot anchor a scoped round: that round gets a full review.
    #[test]
    fn scoped_review_falls_back_to_a_full_review_without_an_anchor() {
        let cfg = Config::default();
        let cwd = Path::new("/nonexistent");
        let synthetic = format!("{}\n{PRIOR}", review_result::SYNTHETIC_REVIEW_MARKER);
        assert!(scoped_review_section(
            &scoped_state(4, &[("rev-1", PRIOR), ("rev-2", &synthetic)]),
            &cfg,
            cwd
        )
        .is_empty());
        let other = PRIOR.replace("abcdef1234567", "1111111222222");
        assert!(scoped_review_section(
            &scoped_state(4, &[("rev-1", PRIOR), ("rev-2", &other)]),
            &cfg,
            cwd
        )
        .is_empty());
        assert!(scoped_review_section(&scoped_state(4, &[]), &cfg, cwd).is_empty());
    }
    /// A critical filed under Follow-up blocked its own round, so the next scoped
    /// round must still re-judge it rather than drop it with the other follow-ups.
    #[test]
    fn scoped_review_keeps_a_critical_filed_under_follow_up() {
        let body =
            format!("{PRIOR}- severity: critical — the migration drops rows at src/m.rs:5\n");
        let s = scoped_review_section(
            &scoped_state(3, &[("rev-1", &body)]),
            &Config::default(),
            Path::new("/nonexistent"),
        );
        assert!(
            s.contains("- critical follow-up `src/m.rs:5` (rev-1)"),
            "{s}"
        );
        assert!(!s.contains("split the module"), "{s}");
    }
    #[test]
    fn reviewer_template_carries_the_severity_rubric() {
        let t = crate::templates::get("reviewer").unwrap();
        assert!(t.contains("Severity rubric"), "{t}");
        assert!(t.contains("Anything that makes the code do the wrong thing is major or above"));
        assert!(t.contains("Minor never blocks the ship"));
        assert!(t.contains("Be strict."));
    }

    /// O127: an extension queued while the gate was being saved is read once more after
    /// the save, so the orchestrator carries on instead of parking over it.
    #[test]
    fn a_queued_extension_read_after_the_gate_save_unparks() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 3;
        state.max_rounds = 3;
        assert!(state.round_ceiling_reached());
        crate::markers::write_marker(&paths, "r1", QUEUED_MAX_ROUNDS, "5\n").unwrap();
        assert!(!escalate_round_ceiling(&mut state, &paths).unwrap());
        assert_eq!(state.max_rounds, 5);
        assert!(state.error.is_none());
        assert!(!paths.marker("r1", QUEUED_MAX_ROUNDS).exists());

        // Nothing queued: it parks.
        state.round = 5;
        assert!(escalate_round_ceiling(&mut state, &paths).unwrap());
        assert_eq!(state.phase, Phase::AwaitingRoundExtension);
    }
}

#[cfg(test)]
mod ceiling_partial_notice_tests {
    use super::{
        blockers_from_disk, ceiling_partial_clean_notice, ceiling_partial_notice,
        ceiling_partial_refused_blocker, ceiling_partial_refused_notice,
        clear_finished_round_partial_mark, commit_before_review, fail, handle_retry_err,
        record_ceiling_partial, record_ceiling_partial_unless_quota, try_commit_ceiling_partial,
        Blocker, CeilingPartialOutcome, CeilingPartialRecord,
    };
    use crate::config::Config;
    use crate::executor::{self, TimeoutCause};
    use crate::paths::SparPaths;
    use crate::state::{
        CeilingPartialKind, CeilingPartialRound, RunState, SlotRole, SlotState, WorktreeRecord,
    };

    fn blockers(kind: Option<CeilingPartialKind>) -> Vec<String> {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.round = 2;
        if let Some(kind) = kind {
            state.ceiling_partial_rounds = vec![CeilingPartialRound {
                round: 1,
                kind,
                commit: None,
            }];
        }
        notes(blockers_from_disk(&state, &paths, &[], &Config::default()))
    }

    fn notes(out: Vec<Blocker>) -> Vec<String> {
        out.into_iter()
            .filter_map(|b| match b {
                Blocker::Note(s) => Some(s),
                Blocker::Verdict(_) => None,
            })
            .collect()
    }

    #[test]
    fn wip_round_claims_a_saved_wip_commit() {
        let out = blockers(Some(CeilingPartialKind::Wip));
        assert_eq!(out, [ceiling_partial_notice(1)]);
        assert!(out[0].contains("saved as a WIP commit"), "{}", out[0]);
    }

    #[test]
    fn clean_round_must_not_claim_a_wip_commit() {
        let out = blockers(Some(CeilingPartialKind::Clean));
        assert_eq!(out, [ceiling_partial_clean_notice(1)]);
        assert!(!out[0].contains("saved as a WIP commit"), "{}", out[0]);
        assert!(out[0].contains("no WIP commit was written"), "{}", out[0]);
    }

    #[test]
    fn refused_round_must_not_claim_a_wip_commit() {
        let out = blockers(Some(CeilingPartialKind::Refused));
        assert_eq!(out, [ceiling_partial_refused_blocker(1)]);
        assert!(!out[0].contains("saved as a WIP commit"), "{}", out[0]);
        assert!(
            out[0].contains("still uncommitted in the tree"),
            "{}",
            out[0]
        );
    }

    #[test]
    fn live_refused_notice_names_the_commit_error() {
        let note = ceiling_partial_refused_notice(1, Some("hook said no"));
        assert!(note.contains("hook said no"), "{note}");
        assert!(!note.contains("saved as a WIP commit"), "{note}");
    }

    #[test]
    fn no_partial_round_means_no_notice() {
        assert!(blockers(None).is_empty());
    }

    #[test]
    fn refused_notice_judges_head_not_an_uncommitted_tree() {
        for detail in [None, Some("hook said no")] {
            let note = ceiling_partial_refused_notice(1, detail);
            assert!(!note.contains("uncommitted"), "{note}");
            assert!(note.contains("judge the HEAD commit"), "{note}");
        }
    }

    #[test]
    fn same_round_resume_sees_its_own_cut_short_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.round = 1;
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: None,
        }];
        let out = notes(blockers_from_disk(&state, &paths, &[], &Config::default()));
        assert_eq!(out, [ceiling_partial_notice(1)]);
    }

    #[test]
    fn finishing_a_round_clears_only_its_own_partial_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.round = 2;
        state.ceiling_partial_rounds = vec![
            CeilingPartialRound {
                round: 1,
                kind: CeilingPartialKind::Wip,
                commit: Some("abc".into()),
            },
            CeilingPartialRound {
                round: 2,
                kind: CeilingPartialKind::Clean,
                commit: None,
            },
        ];
        clear_finished_round_partial_mark(&mut state, &paths).unwrap();
        assert_eq!(state.ceiling_partial_rounds.len(), 1);
        assert_eq!(state.ceiling_partial_rounds[0].round, 1);
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn ceiling_state(
        tmp: &tempfile::TempDir,
        wt: &std::path::Path,
        branch: &str,
        head: Option<String>,
        tree: Option<String>,
    ) -> (SparPaths, RunState) {
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.task = Some("add work".into());
        let mut slot: SlotState = executor::init_slot("impl", "cli:muse", SlotRole::Implementer);
        slot.timeout_cause = Some(TimeoutCause::HardCeiling);
        slot.dispatch_head = head;
        slot.dispatch_tree = tree;
        state.slots = vec![slot];
        state.worktrees = vec![WorktreeRecord {
            slot_id: "impl".into(),
            path: wt.to_path_buf(),
            branch: branch.into(),
        }];
        (paths, state)
    }

    fn fresh_repo(tmp: &tempfile::TempDir) -> (std::path::PathBuf, String) {
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        git(&wt, &["init", "-q"]);
        git(&wt, &["config", "user.email", "t@t"]);
        git(&wt, &["config", "user.name", "t"]);
        std::fs::write(wt.join("f"), "a\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "base"]);
        let branch = git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]);
        (wt, branch)
    }

    #[test]
    fn first_ceiling_kill_with_new_work_banks_a_conventional_wip() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("new work at the ceiling is a partial round");
        assert_eq!(notice, ceiling_partial_notice(1));
        assert!(matches!(kind, CeilingPartialKind::Wip));
        let sha = commit.expect("the wip commit is banked");
        let subject = git(&wt, &["log", "-1", "--format=%s", &sha]);
        assert!(
            subject.starts_with("chore(spar):"),
            "wip subject must pass conventional-commit hooks: {subject}"
        );
    }

    #[test]
    fn banked_ceiling_partial_keeps_ceiling_kill_true_in_status() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("new work at the ceiling is a partial round");
        let mut out = None;
        record_ceiling_partial(
            &mut state,
            &paths,
            "impl",
            CeilingPartialRecord {
                note: notice,
                kind,
                commit,
            },
            &mut out,
        )
        .unwrap();
        let slot = state
            .slots
            .iter()
            .find(|s| s.id == "impl")
            .expect("implementer seat");
        assert_eq!(
            slot.timeout_cause,
            Some(TimeoutCause::HardCeiling),
            "the bank keeps the kill fact on the slot"
        );
        let mut v = serde_json::json!({"slots": [{"id": "impl"}]});
        crate::liveness::enrich_status_json(&mut v, &state.slots, &Config::default(), &paths, "r1");
        assert_eq!(
            v["slots"][0]["ceiling_kill"],
            serde_json::Value::Bool(true),
            "status still reports the ceiling kill after the bank: {v}"
        );
    }

    #[test]
    fn second_idle_ceiling_kill_continues_on_the_banked_wip() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked.clone()),
        }];
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("the banked partial is still there, so the round goes on");
        assert_eq!(notice, ceiling_partial_notice(1));
        assert!(matches!(kind, CeilingPartialKind::Wip));
        assert_eq!(commit.as_deref(), Some(banked.as_str()));
    }

    #[test]
    fn resumed_round_idle_ceiling_kill_continues_on_the_previous_round_bank() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        // The resume opened a new round (the previous round parked) and added
        // nothing before its own ceiling kill.
        state.round = 2;
        state.slots[0].dispatch_head = Some(banked.clone());
        state.slots[0].dispatch_tree = executor::worktree_snapshot(&wt);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked.clone()),
        }];
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("the previous round's banked partial is still there, so the round goes on");
        assert_eq!(notice, ceiling_partial_notice(1));
        assert!(matches!(kind, CeilingPartialKind::Wip));
        assert_eq!(commit.as_deref(), Some(banked.as_str()));
    }

    #[test]
    fn fix_round_idle_ceiling_kill_after_request_changes_is_not_a_partial_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        // The bank's round reached a verdict that rejected this HEAD, so the
        // fix round that idles into its own ceiling kill fails instead of
        // sending the same commit back for another verdict.
        state.round = 2;
        state.slots[0].dispatch_head = Some(banked.clone());
        state.slots[0].dispatch_tree = executor::worktree_snapshot(&wt);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked),
        }];
        state.review_history.push(crate::state::ReviewHistoryEntry {
            round: 1,
            slot_id: "review-0".into(),
            review: crate::workflow::review_result::parse_review(
                "## Verdict\nrequest_changes\n\n## Findings\n- severity: major: unfinished at src/a.rs:1\n",
            ),
        });
        assert!(
            try_commit_ceiling_partial(&mut state, &paths, "impl").is_none(),
            "the panel already rejected this HEAD, so the idle fix round fails"
        );
    }

    #[test]
    fn fix_round_idle_ceiling_kill_after_a_judged_round_is_not_a_partial_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        // Every reviewer approved, but a red suite still rejected the round at
        // the gate and opened the fix round. The approval alone does not carry
        // the bank: the gate saw the round, so the idle fix round fails.
        state.round = 2;
        state.slots[0].dispatch_head = Some(banked.clone());
        state.slots[0].dispatch_tree = executor::worktree_snapshot(&wt);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked),
        }];
        state.review_history.push(crate::state::ReviewHistoryEntry {
            round: 1,
            slot_id: "review-0".into(),
            review: crate::workflow::review_result::parse_review("## Verdict\napprove\n"),
        });
        assert!(
            try_commit_ceiling_partial(&mut state, &paths, "impl").is_none(),
            "the gate saw round 1, so the idle fix round fails"
        );
    }

    #[test]
    fn second_ceiling_kill_after_a_reset_away_from_the_bank_is_not_a_partial_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["reset", "--hard", "-q", "HEAD~1"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked),
        }];
        assert!(
            try_commit_ceiling_partial(&mut state, &paths, "impl").is_none(),
            "the banked commit is no longer carried, so the bank says nothing"
        );
    }

    #[test]
    fn ceiling_kill_with_no_work_and_no_banked_partial_is_not_a_partial_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        assert!(
            try_commit_ceiling_partial(&mut state, &paths, "impl").is_none(),
            "a first ceiling kill with nothing left behind still fails the round"
        );
    }

    #[test]
    fn ceiling_kill_that_also_hit_quota_parks_instead_of_banking() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        state.slots[0].quota_hit = true;
        let mut note = None;
        assert!(
            matches!(
                record_ceiling_partial_unless_quota(&mut state, &paths, "impl", &mut note).unwrap(),
                CeilingPartialOutcome::Absent
            ),
            "quota wins over the ceiling: no partial is banked"
        );
        assert!(note.is_none());
        assert!(
            state.ceiling_partial_rounds.is_empty(),
            "no banked partial to send to review"
        );
        fail(
            &mut state,
            &paths,
            anyhow::anyhow!("quota and ceiling"),
            true,
        )
        .unwrap();
        assert!(
            matches!(state.phase, crate::state::Phase::Quota),
            "a quota hit parks the run"
        );
        assert_eq!(
            state.exit_code(),
            crate::exit_codes::ExitCode::Quota,
            "exit codes are a public contract: quota parks at 4"
        );
    }

    /// A hook that refuses the WIP commit banks a Refused mark and fails the
    /// round: the panel must never judge a HEAD without the round's work.
    #[test]
    fn ceiling_kill_with_a_refused_wip_commit_fails_the_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        let hooks = wt.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("commit-msg");
        std::fs::write(&hook, "#!/bin/sh\necho hook-said-no >&2\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        crate::markers::write_dispatch_verdict(
            &paths,
            "r1",
            "impl",
            &crate::markers::DispatchVerdict {
                ok: false,
                round: 1,
                pid: None,
                exit_code: None,
                signal: Some(9),
                reason: None,
            },
        )
        .unwrap();
        let mut note = None;
        let outcome =
            record_ceiling_partial_unless_quota(&mut state, &paths, "impl", &mut note).unwrap();
        let CeilingPartialOutcome::Refused(msg) = outcome else {
            panic!("a refused WIP commit is not bankable: {outcome:?}");
        };
        assert!(
            msg.contains("failing the round instead of asking review to judge the HEAD commit"),
            "the failure says what it does instead: {msg}"
        );
        assert!(
            msg.contains("hook-said-no"),
            "the failure carries the hook reason: {msg}"
        );
        let slot = state
            .slots
            .iter()
            .find(|s| s.id == "impl")
            .expect("implementer seat");
        assert!(
            !matches!(slot.status, crate::state::SlotStatus::Done),
            "a refused partial keeps the failed dispatch verdict instead of reading done"
        );
        let mark = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == 1)
            .expect("the refusal is banked for the next round's blockers");
        assert!(matches!(mark.kind, CeilingPartialKind::Refused));
        assert!(mark.commit.is_none());
        assert_eq!(
            crate::markers::terminal_marker(&paths, "r1", "impl"),
            Some(crate::markers::TerminalMarker::Failed),
            "a refused partial keeps the slot's failed marker instead of clearing it"
        );
        let err = fail(&mut state, &paths, anyhow::anyhow!(msg), false).unwrap_err();
        assert!(
            err.to_string().contains("failing the round"),
            "exit 1 carries the reason: {err:#}"
        );
        assert!(
            matches!(state.phase, crate::state::Phase::Failed),
            "a refused partial fails the run"
        );
        assert_eq!(
            state.exit_code(),
            crate::exit_codes::ExitCode::Failure,
            "exit codes are a public contract: failure exits 1"
        );
    }

    #[test]
    fn backup_retry_ceiling_kill_with_new_work_banks_a_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "backup seat work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        let mut note = None;
        assert!(
            handle_retry_err(
                &mut state,
                &paths,
                "impl",
                anyhow::anyhow!("backup ceiling kill"),
                &mut note,
            )
            .unwrap(),
            "a ceiling-cut backup retry goes on to review like the primary dispatch"
        );
        let entry = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == 1)
            .expect("the backup retry banks its partial");
        assert!(matches!(entry.kind, CeilingPartialKind::Wip));
        assert!(entry.commit.is_some());
    }

    #[test]
    fn backup_retry_measures_work_against_the_round_start() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        let (_, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        std::fs::write(wt.join("work.txt"), "primary seat work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "primary seat work"]);
        let slot = state.slots.iter_mut().find(|s| s.id == "impl").unwrap();
        slot.point_at("cli:grok@backup");
        let slot = state.slots.iter().find(|s| s.id == "impl").unwrap();
        assert!(
            executor::dispatch_left_work(
                &wt,
                slot.dispatch_head.as_deref(),
                slot.dispatch_tree.as_deref(),
                None,
            ),
            "rotation to backup keeps the round-start stamps, so the primary seat's commit reads as work"
        );
        let fresh_head = git(&wt, &["rev-parse", "HEAD"]);
        let fresh_tree = executor::worktree_snapshot(&wt);
        assert!(
            !executor::dispatch_left_work(&wt, Some(&fresh_head), fresh_tree.as_deref(), None),
            "re-taken stamps would hide the primary seat's commit: the contrast this test pins"
        );
    }

    #[test]
    fn commit_before_review_banks_a_refused_round_as_wip() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        std::fs::write(wt.join("leftovers.txt"), "not yet committed\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, None, None);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Refused,
            commit: None,
        }];
        let note = commit_before_review(&mut state, &paths, &Config::default(), "impl");
        assert_eq!(note, Some(ceiling_partial_notice(1)));
        let entry = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == 1)
            .unwrap();
        assert!(matches!(entry.kind, CeilingPartialKind::Wip));
        assert!(entry.commit.is_some(), "head now carries the work");
    }

    #[test]
    fn commit_before_review_leaves_no_mark_on_a_normal_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        std::fs::write(wt.join("leftovers.txt"), "not yet committed\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, None, None);
        let note = commit_before_review(&mut state, &paths, &Config::default(), "impl");
        assert!(note.is_none());
        assert!(state.ceiling_partial_rounds.is_empty());
    }
}

#[cfg(test)]
mod frozen_reread_tests {
    use super::*;
    use tempfile::tempdir;

    fn git_fixture(root: &std::path::Path) {
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("README.md"), "test\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// A bound dispatch builds its amend preview from
    /// the frozen snapshot as of lock time. Whatever the launcher read before
    /// the lock for its pre-checks must not leak into the dispatch: a concurrent
    /// amend that froze in between would otherwise be silently overwritten by
    /// this dispatch's snapshot.
    #[test]
    fn bound_dispatch_builds_from_the_snapshot_on_disk() {
        let tmp = tempdir().unwrap();
        git_fixture(tmp.path());
        let paths = crate::paths::SparPaths::new(tmp.path());
        let run_id = "r-frozen-reread";
        let mut frozen = Config::default();
        frozen.roles.implementer = Some("cli:claude".to_string());
        frozen.roles.reviewer = vec!["cli:grok".to_string(), "cli:claude".to_string()];
        frozen.suite.enabled = false;
        frozen.autonomy = crate::config::AutonomyLevel::Full;
        frozen.save_snapshot(&paths, run_id).unwrap();
        let mut st = RunState::new(
            run_id,
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        st.gates.plan_approved = true;
        st.task = Some("add a hello function".to_string());
        st.save(&paths).unwrap();
        // The pre-lock read `implement_ctx` takes for its cheap pre-checks sees
        // the claude implementer with no CLI pin. A concurrent `--role`
        // amend lands before the dispatch takes the run lock and pins the
        // frozen snapshot at grok.
        let stale = Config::for_run(&paths, run_id).unwrap();
        assert_eq!(stale.roles.implementer.as_deref(), Some("cli:claude"));
        assert!(!stale.cli_role_keys.contains(&SlotRole::Implementer));
        frozen.roles.implementer = Some("cli:grok".to_string());
        frozen.cli_role_keys.insert(SlotRole::Implementer);
        frozen.save_snapshot(&paths, run_id).unwrap();
        let opts = CommonOpts {
            providers: vec!["cli:claude".to_string(), "cli:grok".to_string()],
            dry_run: true,
            json: true,
            ..CommonOpts::default()
        };
        let _ = run_from_approved(run_id, None, opts, &paths, BoundAmendInputs::default()).unwrap();
        let after = RunState::load(&paths, run_id).unwrap();
        assert_eq!(
            after.autonomy,
            crate::config::AutonomyLevel::Full,
            "the dispatch carries the frozen snapshot, not a stale pre-lock read"
        );
        let implementers: Vec<&str> = after
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Implementer)
            .map(|s| s.provider.as_str())
            .collect();
        assert_eq!(implementers, vec!["cli:grok"]);
    }
}
