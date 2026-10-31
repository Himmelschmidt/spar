//! Idempotency keys (O140): one unfinished run per key, checked before a run is minted.
//!
//! Check-then-act, not a lock. Two launches racing inside the same instant both see no
//! holder and both start; the key stops the seconds-to-days duplicates (a relaunch, a
//! second box, a requeued brief), not a simultaneous double-click.
use crate::cli::KeyScope;
use crate::config::UserConfig;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::state::Phase;
use anyhow::{bail, Result};

#[derive(Debug, PartialEq)]
struct Holder {
    run_id: String,
    phase: String,
    host: Option<String>,
}

/// `Ok(Some(code))` when a run already holds `key`: the refusal is printed (JSON on
/// stdout with `--json`) and the caller returns `code`. A remote that cannot be reached
/// is warned about and skipped, so an unreachable box never blocks a local launch.
///
/// A dry run neither checks nor holds a key: it is a fixture, and a fixture that blocked
/// real work (or was blocked by it) would make `--dry-run` lie about what a launch does.
pub fn guard(
    paths: &SparPaths,
    key: Option<&str>,
    scope: Option<KeyScope>,
    json: bool,
    dry_run: bool,
) -> Result<Option<ExitCode>> {
    let Some(key) = key else {
        return Ok(None);
    };
    if dry_run || crate::util::env_truthy("SPAR_DRY_RUN") {
        return Ok(None);
    }
    if key.trim().is_empty() {
        bail!("--key must not be empty");
    }
    let holder = match local_holder(paths, key)? {
        Some(h) => Some(h),
        None => fleet_holder(paths, key, scope)?,
    };
    let Some(h) = holder else {
        return Ok(None);
    };
    let at = h
        .host
        .as_deref()
        .map(|x| format!(" on {x}"))
        .unwrap_or_default();
    let msg = format!(
        "run {}{at} already holds key {key:?} (phase {}); wait for it, resume it, or stop it before starting another",
        h.run_id, h.phase
    );
    if json {
        println!(
            "{}",
            serde_json::json!({
                "error": msg,
                "kind": "duplicate",
                "run_id": h.run_id,
                "host": h.host,
            })
        );
    } else {
        eprintln!("error: {msg}");
    }
    Ok(Some(ExitCode::Failure))
}

fn local_holder(paths: &SparPaths, key: &str) -> Result<Option<Holder>> {
    let found = crate::state::list_runs(paths)?.into_iter().find(|r| {
        !r.archived && !r.dry_run && r.key.as_deref() == Some(key) && r.phase.holds_key()
    });
    Ok(found.map(|r| Holder {
        phase: phase_name(r.phase),
        run_id: r.id,
        host: None,
    }))
}

fn fleet_holder(paths: &SparPaths, key: &str, scope: Option<KeyScope>) -> Result<Option<Holder>> {
    let remotes = UserConfig::load()?.registry.remotes;
    let scope = scope.unwrap_or(if remotes.is_empty() {
        KeyScope::Local
    } else {
        KeyScope::Fleet
    });
    if scope == KeyScope::Local {
        return Ok(None);
    }
    if remotes.is_empty() {
        eprintln!(
            "warning: --key-scope fleet with no [registry] remotes; checked this project only"
        );
        return Ok(None);
    }
    let here = ProjectId::of(&paths.project_root);
    for answer in crate::registry::query_remotes(&remotes, false) {
        let runs = match answer.runs {
            Ok(runs) => runs,
            Err(e) => {
                eprintln!(
                    "warning: {}: {e}; the --key check did not cover that host",
                    answer.host
                );
                continue;
            }
        };
        if runs.iter().any(|r| r.get("key").is_none()) {
            eprintln!(
                "warning: {} runs a spar that does not report run keys; the --key check cannot see its runs",
                answer.host
            );
        }
        if let Some(h) = remote_holder(&runs, key, &here) {
            return Ok(Some(Holder {
                host: Some(answer.host),
                ..h
            }));
        }
    }
    Ok(None)
}

/// What makes a remote checkout "this project": the normalized origin URL when both
/// sides have one, else the canonical directory name.
struct ProjectId {
    repo: Option<String>,
    name: String,
}

impl ProjectId {
    fn of(root: &std::path::Path) -> Self {
        let root = crate::registry::canonicalize_best_effort(root);
        Self {
            repo: crate::registry::repo_identity(&root),
            name: crate::registry::project_name(&root),
        }
    }

    fn matches(&self, row: &serde_json::Value) -> bool {
        match (&self.repo, row["project_repo"].as_str()) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => row["project_name"].as_str() == Some(self.name.as_str()),
        }
    }
}

/// A remote run matches on key and project identity. A phase this build does not know
/// (a newer remote) holds the key rather than letting a duplicate through.
fn remote_holder(runs: &[serde_json::Value], key: &str, here: &ProjectId) -> Option<Holder> {
    runs.iter()
        .find(|r| {
            r["key"].as_str() == Some(key)
                && here.matches(r)
                && !r["archived"].as_bool().unwrap_or(false)
                && !r["dry_run"].as_bool().unwrap_or(false)
                && serde_json::from_value::<Phase>(r["phase"].clone())
                    .map_or(true, |p| p.holds_key())
        })
        .map(|r| Holder {
            run_id: r["id"].as_str().unwrap_or("?").to_string(),
            phase: r["phase"].as_str().unwrap_or("?").to_string(),
            host: None,
        })
}

fn phase_name(phase: Phase) -> String {
    serde_json::to_value(phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{phase:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn named(name: &str) -> ProjectId {
        ProjectId {
            repo: None,
            name: name.into(),
        }
    }

    #[test]
    fn a_remote_run_holds_the_key_only_in_the_same_project_while_unfinished() {
        let runs = vec![
            json!({"id": "done0001", "key": "k", "project_name": "app", "phase": "done"}),
            json!({"id": "other001", "key": "k", "project_name": "lib", "phase": "dispatch"}),
            json!({"id": "arch0001", "key": "k", "project_name": "app", "phase": "dispatch", "archived": true}),
            json!({"id": "dry00001", "key": "k", "project_name": "app", "phase": "dispatch", "dry_run": true}),
            json!({"id": "live0001", "key": "k", "project_name": "app", "phase": "awaiting_plan_approval"}),
        ];
        let h = remote_holder(&runs, "k", &named("app")).expect("the gate run holds the key");
        assert_eq!(h.run_id, "live0001");
        assert!(remote_holder(&runs[..4], "k", &named("app")).is_none());
    }

    #[test]
    fn the_origin_url_outranks_the_directory_name() {
        let here = ProjectId {
            repo: Some("github.com/acme/app".into()),
            name: "app".into(),
        };
        let runs = vec![
            json!({"id": "fork0001", "key": "k", "project_name": "app", "project_repo": "github.com/other/app", "phase": "dispatch"}),
            json!({"id": "same0001", "key": "k", "project_name": "app-2", "project_repo": "github.com/acme/app", "phase": "dispatch"}),
        ];
        assert_eq!(remote_holder(&runs, "k", &here).unwrap().run_id, "same0001");
        let old =
            vec![json!({"id": "old00001", "key": "k", "project_name": "app", "phase": "dispatch"})];
        assert_eq!(
            remote_holder(&old, "k", &here).unwrap().run_id,
            "old00001",
            "a row without a repo identity falls back to the name"
        );
    }

    #[test]
    fn an_unknown_remote_phase_holds_the_key() {
        let runs = vec![
            json!({"id": "new00001", "key": "k", "project_name": "app", "phase": "some_future_phase"}),
        ];
        assert!(remote_holder(&runs, "k", &named("app")).is_some());
    }

    #[test]
    fn finished_and_dead_ended_phases_release_the_key_and_resumable_ones_hold_it() {
        for p in [
            Phase::Done,
            Phase::PlanRejected,
            Phase::Failed,
            Phase::Stuck,
            Phase::Escalated,
        ] {
            assert!(!p.holds_key(), "{p:?}");
        }
        for p in [
            Phase::Init,
            Phase::Dispatch,
            Phase::AwaitingPlanApproval,
            Phase::PlanApproved,
            Phase::AwaitingShipConfirm,
            Phase::Stopped,
            Phase::Quota,
        ] {
            assert!(p.holds_key(), "{p:?}");
        }
    }
}
