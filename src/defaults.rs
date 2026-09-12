use crate::registry::spar_home;
use crate::runspec::{Pin, RoleAssignment, RunSpec, SpecWorkflow};
use crate::state::SlotRole;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::RefCell;
use std::path::PathBuf;

#[cfg(test)]
thread_local! {
    static TEST_HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub fn set_test_home(path: Option<PathBuf>) {
    TEST_HOME.with(|h| *h.borrow_mut() = path);
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DefaultsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    roles: Vec<DefaultsRole>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    arena_pool: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DefaultsRole {
    role: String,
    ordinal: usize,
    primary: Option<String>,
    backup: Option<String>,
}

fn defaults_path() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = TEST_HOME.with(|h| h.borrow().clone()) {
            return p.join("defaults.json");
        }
    }
    spar_home().join("defaults.json")
}

pub fn load() -> RunSpec {
    let path = defaults_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return RunSpec::default(),
    };
    if text.trim().is_empty() {
        return RunSpec::default();
    }
    let file: DefaultsFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(_) => return RunSpec::default(),
    };
    let workflow = file.workflow.as_deref().and_then(SpecWorkflow::parse);
    let task = file.task.clone().unwrap_or_default();
    let mut roles = Vec::new();
    for dr in file.roles {
        let Some(role) = SlotRole::from_config_key(&dr.role) else {
            continue;
        };
        let primary = dr.primary.as_deref().and_then(|s| Pin::parse(s).ok());
        let backup = dr.backup.as_deref().and_then(|s| Pin::parse(s).ok());
        roles.push(RoleAssignment {
            role,
            ordinal: dr.ordinal,
            primary,
            backup,
        });
    }
    let arena_pool = file
        .arena_pool
        .into_iter()
        .map(|s| Pin::parse(&s).ok())
        .collect();
    RunSpec {
        workflow,
        task,
        roles,
        arena_pool,
        ..Default::default()
    }
}

pub fn save(spec: &RunSpec) -> Result<()> {
    let path = defaults_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let workflow = spec.workflow.map(|w| w.as_str().to_string());
    let task = if spec.task.trim().is_empty() {
        None
    } else {
        Some(spec.task.clone())
    };
    let roles = spec
        .roles
        .iter()
        .map(|r| DefaultsRole {
            role: r.role.as_config_key().to_string(),
            ordinal: r.ordinal,
            primary: r.primary.as_ref().map(|p| p.display()),
            backup: r.backup.as_ref().map(|p| p.display()),
        })
        .collect();
    let arena_pool = spec
        .arena_pool
        .iter()
        .filter_map(|p| p.as_ref().map(|pin| pin.display()))
        .collect();
    let file = DefaultsFile {
        workflow,
        task,
        roles,
        arena_pool,
    };
    let text = serde_json::to_string_pretty(&file)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn with_home<F: FnOnce()>(f: F) {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("spar-home");
        std::fs::create_dir_all(&home).unwrap();
        set_test_home(Some(home));
        f();
        set_test_home(None);
    }

    #[test]
    fn saves_and_loads_roundtrip() {
        with_home(|| {
            let spec = RunSpec {
                workflow: Some(SpecWorkflow::Plan),
                task: "hello".into(),
                roles: vec![RoleAssignment {
                    role: SlotRole::Planner,
                    ordinal: 0,
                    primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                    backup: Some(Pin::parse("cli:codex@terra").unwrap()),
                }],
                ..Default::default()
            };
            save(&spec).unwrap();
            let loaded = load();
            assert_eq!(loaded.workflow, Some(SpecWorkflow::Plan));
            assert_eq!(loaded.roles.len(), 1);
            assert_eq!(
                loaded.roles[0].primary.as_ref().unwrap().display(),
                "cli:claude@opus"
            );
            assert_eq!(
                loaded.roles[0].backup.as_ref().unwrap().display(),
                "cli:codex@terra"
            );
        });
    }

    #[test]
    fn missing_file_returns_default() {
        with_home(|| {
            let spec = load();
            assert!(spec.workflow.is_none());
            assert!(spec.roles.is_empty());
        });
    }

    #[test]
    fn corrupt_file_returns_default() {
        with_home(|| {
            let path = defaults_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, "not json").unwrap();
            let spec = load();
            assert!(spec.workflow.is_none());
        });
    }

    #[test]
    fn tainting_defaults_does_not_affect_frozen_run() {
        with_home(|| {
            let spec = RunSpec {
                workflow: Some(SpecWorkflow::Implement),
                roles: vec![RoleAssignment {
                    role: SlotRole::Implementer,
                    ordinal: 0,
                    primary: Some(Pin::parse("cli:claude").unwrap()),
                    backup: None,
                }],
                ..Default::default()
            };
            save(&spec).unwrap();
            let path = defaults_path();
            std::fs::write(&path, "corrupt").unwrap();
            let loaded = load();
            assert!(
                loaded.workflow.is_none(),
                "corrupt defaults must return None, not previous value {:?}",
                loaded.workflow
            );
            assert!(
                loaded.roles.is_empty(),
                "corrupt defaults must return empty roles"
            );
        });
    }

    #[test]
    fn deleting_defaults_after_save_returns_default() {
        with_home(|| {
            let spec = RunSpec {
                workflow: Some(SpecWorkflow::Plan),
                roles: vec![RoleAssignment {
                    role: SlotRole::Planner,
                    ordinal: 0,
                    primary: Some(Pin::parse("cli:claude").unwrap()),
                    backup: None,
                }],
                ..Default::default()
            };
            save(&spec).unwrap();
            let path = defaults_path();
            std::fs::remove_file(&path).unwrap();
            let loaded = load();
            assert!(loaded.workflow.is_none());
            assert!(loaded.roles.is_empty());
        });
    }
}
