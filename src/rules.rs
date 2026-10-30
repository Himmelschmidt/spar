//! Operator rulings (O121): settled points the operator records on a run or a project.
//! Every later reviewer and implementer prompt carries them, and a run ruling may waive
//! an acceptance criterion so the gate stops counting it.
//!
//! Stored outside `state.json` and outside any frozen config: a live orchestrator holds
//! its state in memory and saves over it, so a rule written there would be lost, and a
//! ruling is meant to reach the next dispatch of a run already in flight.

use crate::paths::SparPaths;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Run,
    Project,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ruling {
    /// `R<n>` on a run, `P<n>` on the project. Never reused after `rm`.
    pub id: String,
    pub scope: Scope,
    pub text: String,
    /// Acceptance criteria this ruling takes out of the gate. Run rulings only.
    #[serde(default)]
    pub waives: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    Run(&'a str),
    Project,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RuleFile {
    #[serde(default)]
    next_id: u32,
    #[serde(default)]
    rules: Vec<Ruling>,
}

fn file_for(paths: &SparPaths, target: Target<'_>) -> PathBuf {
    match target {
        Target::Run(id) => paths.run_dir(id).join("rules.json"),
        Target::Project => paths.root.join("rules").join("rules.json"),
    }
}

fn read(path: &Path) -> Result<RuleFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RuleFile::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Read-modify-write under an advisory lock on a sibling file, then a rename, so two
/// concurrent `rule add`s cannot drop one and a reader never sees half a file.
fn update<T>(path: &Path, f: impl FnOnce(&mut RuleFile) -> Result<T>) -> Result<T> {
    let dir = path.parent().context("rules file has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("lock {}", lock_path.display()))?;
    let mut file = read(path)?;
    let out = f(&mut file)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&file)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(out)
}

/// `AC-7` / `ac-7` -> `AC-7`; anything else is not a criterion id.
fn criterion_id(raw: &str) -> Option<String> {
    let up = raw.trim().to_ascii_uppercase();
    let digits = up.strip_prefix("AC-")?;
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(up)
}

/// Record a ruling. A waiver must name a criterion the run's contract declares, when
/// it has one: a typo would otherwise waive nothing and the gate would keep blocking.
pub fn add(paths: &SparPaths, target: Target<'_>, text: &str, waive: &[String]) -> Result<Ruling> {
    let text = text.trim();
    if text.is_empty() {
        bail!("ruling text is empty");
    }
    let mut waives = Vec::new();
    for raw in waive {
        let Some(id) = criterion_id(raw) else {
            bail!("--waive takes a criterion id like AC-7, not `{raw}`");
        };
        if !waives.contains(&id) {
            waives.push(id);
        }
    }
    let (scope, prefix) = match target {
        Target::Run(_) => (Scope::Run, "R"),
        Target::Project => (Scope::Project, "P"),
    };
    if let Target::Run(run_id) = target {
        if !paths.state_file(run_id).is_file() {
            bail!(
                "run {run_id} not found under {}",
                paths.runs_dir().display()
            );
        }
        let contract =
            std::fs::read_to_string(paths.artifact(run_id, "test-contract.md")).unwrap_or_default();
        let declared = crate::workflow::review_result::parse_contract_criteria(&contract);
        if let Some(id) = waives
            .iter()
            .find(|id| !declared.is_empty() && !declared.contains(id))
        {
            bail!(
                "{id} is not a criterion in run {run_id}'s contract (declared: {})",
                declared.join(", ")
            );
        }
    } else if !waives.is_empty() {
        bail!("--waive applies to one run's contract; use --run <id>");
    }
    update(&file_for(paths, target), |file| {
        file.next_id = file.next_id.max(1);
        let ruling = Ruling {
            id: format!("{prefix}{}", file.next_id),
            scope,
            text: text.to_string(),
            waives,
            created_at: Utc::now(),
        };
        file.next_id += 1;
        file.rules.push(ruling.clone());
        Ok(ruling)
    })
}

pub fn remove(paths: &SparPaths, target: Target<'_>, id: &str) -> Result<Ruling> {
    let path = file_for(paths, target);
    if !path.is_file() {
        bail!("no ruling {id}: none recorded here");
    }
    update(&path, |file| {
        let Some(pos) = file
            .rules
            .iter()
            .position(|r| r.id.eq_ignore_ascii_case(id))
        else {
            let hint = match target {
                Target::Run(_) if id.to_ascii_uppercase().starts_with('P') => {
                    " (a P rule is the project's: spar rule rm --project <id>)"
                }
                _ => "",
            };
            bail!("no ruling {id}{hint}");
        };
        Ok(file.rules.remove(pos))
    })
}

/// The rulings in force for `target`: a run sees the project's rulings, then its own.
pub fn list(paths: &SparPaths, target: Target<'_>) -> Result<Vec<Ruling>> {
    let mut out = read(&file_for(paths, Target::Project))?.rules;
    if let Target::Run(_) = target {
        out.extend(read(&file_for(paths, target))?.rules);
    }
    Ok(out)
}

/// `list` for a run, for prompt rendering and the gate. An unreadable rules file is
/// warned about and skipped rather than failing a dispatch; a waiver it held then does
/// not apply, which fails closed.
pub fn for_run(paths: &SparPaths, run_id: &str) -> Vec<Ruling> {
    list(paths, Target::Run(run_id)).unwrap_or_else(|e| {
        eprintln!("warning: operator rulings not read: {e:#}");
        Vec::new()
    })
}

/// Each waived criterion with the ruling that waived it, first ruling wins.
pub fn waived(rulings: &[Ruling]) -> Vec<(&str, &Ruling)> {
    let mut out: Vec<(&str, &Ruling)> = Vec::new();
    for r in rulings {
        for id in &r.waives {
            if !out.iter().any(|(w, _)| w == id) {
                out.push((id.as_str(), r));
            }
        }
    }
    out
}

/// The contract's criteria minus the waived ones: what the acceptance gate counts.
pub fn unwaived(criteria: &[String], rulings: &[Ruling]) -> Vec<String> {
    let w = waived(rulings);
    criteria
        .iter()
        .filter(|c| !w.iter().any(|(id, _)| id == c))
        .cloned()
        .collect()
}

pub fn line(r: &Ruling) -> String {
    let scope = match r.scope {
        Scope::Run => "",
        Scope::Project => " (project)",
    };
    let waives = if r.waives.is_empty() {
        String::new()
    } else {
        format!(" [waives {}]", r.waives.join(", "))
    };
    format!("- {}{scope}: {}{waives}", r.id, r.text.trim())
}

/// `{{operator_rulings}}`: empty when there are none.
pub fn prompt_section(rulings: &[Ruling]) -> String {
    if rulings.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Operator rulings\n\
         The operator has ruled on the points below. They override the task, the plan and the \
         acceptance contract where they conflict. Treat them as settled: do not relitigate \
         them, and do not block or rework on anything they rule out.\n\n",
    );
    for r in rulings {
        out.push_str(&line(r));
        out.push('\n');
    }
    let w = waived(rulings);
    if !w.is_empty() {
        let ids: Vec<&str> = w.iter().map(|(id, _)| *id).collect();
        out.push_str(&format!(
            "\nWaived criteria: {}. The acceptance gate no longer counts them. Reviewers still \
             list each in `## Acceptance`, as `{}: unverified — waived by {}`.\n",
            ids.join(", "),
            w[0].0,
            w[0].1.id
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn run(paths: &SparPaths, id: &str, contract: &str) {
        paths.ensure_run_dirs(id).unwrap();
        std::fs::write(paths.state_file(id), "{}").unwrap();
        std::fs::write(paths.artifact(id, "test-contract.md"), contract).unwrap();
    }

    const CONTRACT: &str = "## Scenarios\n- [ ] AC-1: one\n- [ ] AC-2: two\n";

    #[test]
    fn add_list_rm_round_trip_with_stable_ids() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        run(&paths, "r1", CONTRACT);
        let p = add(&paths, Target::Project, "dates are UTC", &[]).unwrap();
        assert_eq!(p.id, "P1");
        let r1 = add(&paths, Target::Run("r1"), "AC-2 is void", &["ac-2".into()]).unwrap();
        assert_eq!(
            (r1.id.as_str(), r1.waives.as_slice()),
            ("R1", &["AC-2".to_string()][..])
        );
        let r2 = add(&paths, Target::Run("r1"), "keep the old flag", &[]).unwrap();
        assert_eq!(r2.id, "R2");

        let all = list(&paths, Target::Run("r1")).unwrap();
        let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["P1", "R1", "R2"]);
        assert_eq!(list(&paths, Target::Project).unwrap().len(), 1);

        assert_eq!(remove(&paths, Target::Run("r1"), "r2").unwrap().id, "R2");
        let r3 = add(&paths, Target::Run("r1"), "again", &[]).unwrap();
        assert_eq!(r3.id, "R3", "a removed id is never reused");
        let err = remove(&paths, Target::Run("r1"), "P1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--project"), "{err}");
    }

    #[test]
    fn waivers_are_checked_against_the_contract() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        run(&paths, "r1", CONTRACT);
        let err = add(&paths, Target::Run("r1"), "x", &["AC-9".into()]).unwrap_err();
        assert!(err.to_string().contains("AC-9 is not a criterion"), "{err}");
        assert!(add(&paths, Target::Run("r1"), "x", &["seven".into()]).is_err());
        assert!(add(&paths, Target::Project, "x", &["AC-1".into()]).is_err());
        assert!(add(&paths, Target::Run("nope"), "x", &[]).is_err());
        assert!(add(&paths, Target::Run("r1"), "  ", &[]).is_err());
    }

    #[test]
    fn waived_criteria_leave_the_gate_and_reach_the_prompt() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        run(&paths, "r1", CONTRACT);
        add(
            &paths,
            Target::Run("r1"),
            "AC-2 is void: retired guard",
            &["AC-2".into()],
        )
        .unwrap();
        let rulings = for_run(&paths, "r1");
        let criteria = vec!["AC-1".to_string(), "AC-2".to_string()];
        assert_eq!(unwaived(&criteria, &rulings), ["AC-1"]);
        let s = prompt_section(&rulings);
        assert!(s.contains("## Operator rulings"), "{s}");
        assert!(
            s.contains("- R1: AC-2 is void: retired guard [waives AC-2]"),
            "{s}"
        );
        assert!(s.contains("AC-2: unverified — waived by R1"), "{s}");
        assert!(prompt_section(&[]).is_empty());
    }
}
