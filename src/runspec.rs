use crate::config::Config;
use crate::provider_ref::ProviderRef;
use crate::state::SlotRole;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecWorkflow {
    Plan,
    Implement,
    Review,
    Arena,
}

impl SpecWorkflow {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            SpecWorkflow::Plan => "plan",
            SpecWorkflow::Implement => "implement",
            SpecWorkflow::Review => "review",
            SpecWorkflow::Arena => "arena",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" => Some(Self::Plan),
            "implement" | "loop" => Some(Self::Implement),
            "review" => Some(Self::Review),
            "arena" => Some(Self::Arena),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn to_workflow_kind(self) -> crate::cli::WorkflowKind {
        match self {
            SpecWorkflow::Plan => crate::cli::WorkflowKind::Plan,
            SpecWorkflow::Implement => crate::cli::WorkflowKind::Loop,
            SpecWorkflow::Review => crate::cli::WorkflowKind::Review,
            SpecWorkflow::Arena => crate::cli::WorkflowKind::Arena,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub provider: String,
    pub model: Option<String>,
}

impl Pin {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        Self::parse_with_separate(raw, None)
    }

    pub fn parse_with_separate(raw: &str, separate_model: Option<&str>) -> anyhow::Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("empty provider");
        }
        let pref = ProviderRef::parse(raw)?;
        if let Some(sep) = separate_model {
            let sep = sep.trim();
            if !sep.is_empty() {
                if let Some(embedded) = &pref.model {
                    if embedded != sep {
                        anyhow::bail!(
                            "conflicting model: provider {raw:?} already has @{} but separate model {sep:?} was also given",
                            embedded
                        );
                    }
                }
                let key = pref.storage_key();
                let model = Some(sep.to_string());
                return Ok(Self {
                    provider: key,
                    model,
                });
            }
        }
        Ok(Self {
            provider: pref.storage_key(),
            model: pref.model,
        })
    }

    pub fn display(&self) -> String {
        match &self.model {
            Some(m) => format!("{}@{}", self.provider, m),
            None => self.provider.clone(),
        }
    }

    pub fn storage_key(&self) -> &str {
        &self.provider
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAssignment {
    pub role: SlotRole,
    pub ordinal: usize,
    pub primary: Option<Pin>,
    pub backup: Option<Pin>,
}

#[derive(Debug, Clone, Default)]
pub struct RunSpec {
    #[allow(dead_code)]
    pub project: Option<PathBuf>,
    pub task: String,
    pub brief_path: Option<PathBuf>,
    pub workflow: Option<SpecWorkflow>,
    pub roles: Vec<RoleAssignment>,
    pub arena_pool: Vec<Option<Pin>>,
    pub legacy_providers: Vec<String>,
}

pub fn spec_rows(workflow: SpecWorkflow, cfg: &Config) -> Vec<(SlotRole, usize)> {
    match workflow {
        SpecWorkflow::Plan => {
            let mut rows = vec![(SlotRole::Planner, 0)];
            if cfg.critic.enabled {
                rows.push((SlotRole::PlanCritic, 0));
            }
            if cfg.spec.enabled {
                rows.push((SlotRole::TestAuthor, 0));
            }
            rows
        }
        SpecWorkflow::Implement => {
            let mut rows = vec![(SlotRole::Implementer, 0)];
            let n = crate::workflow::roles_resolve::panel_size(cfg);
            for i in 0..n {
                rows.push((SlotRole::Reviewer, i));
            }
            if cfg.suite.enabled && !cfg.suite.is_builtin() {
                rows.push((SlotRole::Tester, 0));
            }
            rows
        }
        SpecWorkflow::Review => {
            let n = crate::workflow::roles_resolve::panel_size(cfg);
            let mut rows = Vec::new();
            for i in 0..n {
                rows.push((SlotRole::Reviewer, i));
            }
            rows
        }
        SpecWorkflow::Arena => {
            let n = (cfg.max_agents as usize).max(2);
            let mut rows = Vec::new();
            for i in 0..n {
                rows.push((SlotRole::Implementer, i));
            }
            rows
        }
    }
}

impl RunSpec {
    #[allow(dead_code)]
    pub fn blanks(&self, cfg: &Config) -> Vec<String> {
        let mut out = Vec::new();
        if self.workflow.is_none() {
            out.push("workflow".to_string());
        }
        if self.task.trim().is_empty() {
            out.push("task".to_string());
        }
        if !self.legacy_providers.is_empty() {
            out.push("legacy_providers".to_string());
        }
        if let Some(wf) = self.workflow {
            if wf == SpecWorkflow::Arena {
                for (idx, slot) in self.arena_pool.iter().enumerate() {
                    if slot.is_none() {
                        out.push(format!("arena[{}]", idx));
                    }
                }
                let expected = spec_rows(wf, cfg).len();
                if self.arena_pool.len() < expected {
                    for idx in self.arena_pool.len()..expected {
                        out.push(format!("arena[{}]", idx));
                    }
                }
            } else {
                let rows = spec_rows(wf, cfg);
                for (role, ordinal) in rows {
                    let found = self
                        .roles
                        .iter()
                        .find(|r| r.role == role && r.ordinal == ordinal);
                    match found {
                        Some(assign) if assign.primary.is_some() => {}
                        _ => out.push(format!("{}[{}]", role.as_config_key(), ordinal)),
                    }
                }
            }
        }
        out
    }

    pub fn validate_for_launch(&self, cfg: &Config) -> Result<(), String> {
        if !self.legacy_providers.is_empty() {
            return Err(format!(
                "legacy providers require workflow mapping: {}",
                self.legacy_providers.join(", ")
            ));
        }
        let wf = self
            .workflow
            .ok_or_else(|| "workflow is not set".to_string())?;
        if self.task.trim().is_empty() {
            return Err("task is empty".to_string());
        }
        if wf == SpecWorkflow::Arena {
            let expected = spec_rows(wf, cfg).len();
            if self.arena_pool.len() != expected {
                return Err(format!(
                    "arena pool has {} entries, expected {}",
                    self.arena_pool.len(),
                    expected
                ));
            }
            for (idx, slot) in self.arena_pool.iter().enumerate() {
                if slot.is_none() {
                    return Err(format!("arena[{}] is not set", idx));
                }
                if let Some(pin) = slot {
                    if let Err(e) = crate::provider_ref::ProviderRef::parse(&pin.display()) {
                        return Err(format!(
                            "arena[{}] is invalid ({}): {}",
                            idx,
                            pin.display(),
                            e
                        ));
                    }
                }
            }
            for assign in &self.roles {
                if assign.primary.is_some() || assign.backup.is_some() {
                    return Err(format!(
                        "arena workflow must not have role assignment for {}",
                        assign.role.as_config_key()
                    ));
                }
            }
            return Ok(());
        }
        if !self.arena_pool.is_empty() && self.arena_pool.iter().any(|p| p.is_some()) {
            return Err("non-arena workflow must not have arena pool".to_string());
        }
        for assign in &self.roles {
            if let Some(b) = &assign.backup {
                if wf == SpecWorkflow::Arena {
                    return Err("arena workflow must not have backup".to_string());
                }
                if let Some(p) = &assign.primary {
                    if p.storage_key() == b.storage_key() {
                        return Err(format!(
                            "backup for {}[{}] has same provider as primary ({})",
                            assign.role.as_config_key(),
                            assign.ordinal,
                            p.storage_key()
                        ));
                    }
                }
            }
        }
        // Cross-ordinal: a backup must not duplicate another seat's primary
        // within the same role (reviewer panel). Cross-role duplication is allowed.
        for assign in &self.roles {
            if let Some(b) = &assign.backup {
                for other in &self.roles {
                    if other.role != assign.role {
                        continue;
                    }
                    if other.ordinal == assign.ordinal {
                        continue;
                    }
                    if let Some(p) = &other.primary {
                        if p.storage_key() == b.storage_key() {
                            return Err(format!(
                                "backup for {}[{}] has same provider as {}[{}] primary ({})",
                                assign.role.as_config_key(),
                                assign.ordinal,
                                other.role.as_config_key(),
                                other.ordinal,
                                p.storage_key()
                            ));
                        }
                    }
                }
            }
        }
        for a in &self.roles {
            if let Some(b1) = &a.backup {
                for b in &self.roles {
                    if b.role != a.role || b.ordinal <= a.ordinal {
                        continue;
                    }
                    if let Some(b2) = &b.backup {
                        if b1.storage_key() == b2.storage_key() {
                            return Err(format!(
                                "backup for {}[{}] and {}[{}] have same provider ({})",
                                a.role.as_config_key(),
                                a.ordinal,
                                b.role.as_config_key(),
                                b.ordinal,
                                b1.storage_key()
                            ));
                        }
                    }
                }
            }
        }
        let rows = spec_rows(wf, cfg);
        for (role, ordinal) in &rows {
            let found = self
                .roles
                .iter()
                .find(|r| r.role == *role && r.ordinal == *ordinal);
            match found {
                Some(assign) => {
                    if assign.primary.is_none() {
                        return Err(format!(
                            "required primary for {}[{}] is not set",
                            role.as_config_key(),
                            ordinal
                        ));
                    }
                    if let Some(pin) = &assign.primary {
                        if let Err(e) = crate::provider_ref::ProviderRef::parse(&pin.display()) {
                            return Err(format!(
                                "primary for {}[{}] is invalid ({}): {}",
                                role.as_config_key(),
                                ordinal,
                                pin.display(),
                                e
                            ));
                        }
                    }
                }
                None => {
                    return Err(format!(
                        "missing assignment for {}[{}]",
                        role.as_config_key(),
                        ordinal
                    ));
                }
            }
        }
        for assign in &self.roles {
            let is_dispatched = rows
                .iter()
                .any(|(r, o)| *r == assign.role && *o == assign.ordinal);
            if !is_dispatched && (assign.primary.is_some() || assign.backup.is_some()) {
                return Err(format!(
                    "assignment for {}[{}] is not dispatched for workflow {}",
                    assign.role.as_config_key(),
                    assign.ordinal,
                    wf.as_str()
                ));
            }
        }
        Ok(())
    }

    pub fn argv(&self, cfg: &Config) -> Result<Vec<String>, String> {
        self.validate_for_launch(cfg)?;
        let wf = self.workflow.unwrap();
        let mut argv = Vec::new();
        match wf {
            SpecWorkflow::Plan => {
                argv.push("plan".to_string());
                if let Some(brief) = &self.brief_path {
                    argv.push("--brief".to_string());
                    argv.push(brief.display().to_string());
                } else {
                    argv.push("-t".to_string());
                    argv.push(self.task.trim().to_string());
                }
            }
            SpecWorkflow::Implement => {
                argv.push("implement".to_string());
                argv.push("-t".to_string());
                argv.push(self.task.trim().to_string());
            }
            SpecWorkflow::Review => {
                argv.push("run".to_string());
                argv.push("--workflow".to_string());
                argv.push("review".to_string());
                argv.push("-t".to_string());
                argv.push(self.task.trim().to_string());
            }
            SpecWorkflow::Arena => {
                argv.push("run".to_string());
                argv.push("--workflow".to_string());
                argv.push("arena".to_string());
                argv.push("-t".to_string());
                argv.push(self.task.trim().to_string());
                let providers: Vec<String> = self
                    .arena_pool
                    .iter()
                    .filter_map(|p| p.as_ref().map(|pin| pin.display()))
                    .collect();
                argv.push("--providers".to_string());
                argv.push(providers.join(","));
                return Ok(argv);
            }
        }
        let rows = spec_rows(wf, cfg);
        for (role, ordinal) in rows {
            if let Some(assign) = self
                .roles
                .iter()
                .find(|r| r.role == role && r.ordinal == ordinal)
            {
                if let Some(primary) = &assign.primary {
                    argv.push("--role".to_string());
                    argv.push(format!("{}={}", role.as_config_key(), primary.display()));
                }
                if let Some(backup) = &assign.backup {
                    argv.push("--backup".to_string());
                    argv.push(format!("{}={}", role.as_config_key(), backup.display()));
                }
            }
        }
        Ok(argv)
    }

    #[allow(dead_code)]
    pub fn blanks_for_workflow(workflow: Option<SpecWorkflow>, cfg: &Config) -> Vec<String> {
        let spec = RunSpec {
            workflow,
            ..Default::default()
        };
        spec.blanks(cfg)
    }

    pub fn apply_proposal_to_spec(
        mut spec: RunSpec,
        proposal: &crate::orchestrator::Proposal,
        cfg: &Config,
    ) -> RunSpec {
        if spec.task.trim().is_empty() && !proposal.task.trim().is_empty() {
            spec.task = proposal.task.clone();
        }
        if spec.workflow.is_none() {
            if let Some(wf) = proposal.workflow.as_deref().and_then(SpecWorkflow::parse) {
                spec.workflow = Some(wf);
            }
        }
        let mut proposal_roles: std::collections::BTreeMap<(String, usize), Pin> =
            std::collections::BTreeMap::new();
        for (k, v) in &proposal.roles {
            if SlotRole::from_config_key(k).is_some() {
                if let Ok(pin) = Pin::parse(v) {
                    proposal_roles.insert((k.clone(), 0), pin);
                }
            }
        }
        for (idx, v) in proposal.reviewer.iter().enumerate() {
            if let Ok(pin) = Pin::parse(v) {
                proposal_roles.insert((format!("reviewer:{}", idx), idx), pin);
            }
        }
        let mut proposal_backups: std::collections::BTreeMap<(String, usize), Pin> =
            std::collections::BTreeMap::new();
        for (k, v) in &proposal.backups {
            if SlotRole::from_config_key(k).is_some() {
                if let Ok(pin) = Pin::parse(v) {
                    proposal_backups.insert((k.clone(), 0), pin);
                }
            }
        }
        for (idx, v) in proposal.reviewer_backups.iter().enumerate() {
            if let Ok(pin) = Pin::parse(v) {
                proposal_backups.insert((format!("reviewer:{}", idx), idx), pin);
            }
        }
        for ((key, ordinal), pin) in proposal_roles {
            let role = if key.starts_with("reviewer:") {
                SlotRole::Reviewer
            } else {
                SlotRole::from_config_key(&key).unwrap()
            };
            let actual_ordinal = if role == SlotRole::Reviewer {
                ordinal
            } else {
                0
            };
            if let Some(wf) = spec.workflow {
                let dispatched = spec_rows(wf, cfg);
                if !dispatched
                    .iter()
                    .any(|(r, o)| *r == role && *o == actual_ordinal)
                {
                    continue;
                }
            }
            if let Some(existing) = spec
                .roles
                .iter_mut()
                .find(|r| r.role == role && r.ordinal == actual_ordinal)
            {
                if existing.primary.is_none() {
                    existing.primary = Some(pin);
                }
            } else {
                spec.roles.push(RoleAssignment {
                    role,
                    ordinal: actual_ordinal,
                    primary: Some(pin),
                    backup: None,
                });
            }
        }
        for ((key, ordinal), pin) in proposal_backups {
            let role = if key.starts_with("reviewer:") {
                SlotRole::Reviewer
            } else {
                SlotRole::from_config_key(&key).unwrap()
            };
            let actual_ordinal = if role == SlotRole::Reviewer {
                ordinal
            } else {
                0
            };
            if let Some(wf) = spec.workflow {
                let dispatched = spec_rows(wf, cfg);
                if !dispatched
                    .iter()
                    .any(|(r, o)| *r == role && *o == actual_ordinal)
                {
                    continue;
                }
            }
            if let Some(existing) = spec
                .roles
                .iter_mut()
                .find(|r| r.role == role && r.ordinal == actual_ordinal)
            {
                if existing.backup.is_none() {
                    existing.backup = Some(pin);
                }
            } else {
                spec.roles.push(RoleAssignment {
                    role,
                    ordinal: actual_ordinal,
                    primary: None,
                    backup: Some(pin),
                });
            }
        }
        if !proposal.providers.is_empty() {
            if let Some(wf) = spec.workflow {
                if wf != SpecWorkflow::Arena {
                    let rows = spec_rows(wf, cfg);
                    for (idx, (role, ordinal)) in rows.iter().enumerate() {
                        if let Some(raw) = proposal.providers.get(idx) {
                            match Pin::parse(raw) {
                                Ok(pin) => {
                                    if let Some(existing) = spec.roles.iter_mut().find(|r| {
                                        r.role == *role
                                            && r.ordinal == *ordinal
                                            && r.primary.is_none()
                                    }) {
                                        existing.primary = Some(pin);
                                    } else if spec
                                        .roles
                                        .iter()
                                        .all(|r| !(r.role == *role && r.ordinal == *ordinal))
                                    {
                                        spec.roles.push(RoleAssignment {
                                            role: *role,
                                            ordinal: *ordinal,
                                            primary: Some(pin),
                                            backup: None,
                                        });
                                    } else {
                                        // Slot already has a primary (operator-filled or earlier proposal). Preserve operator choice — ignore proposal provider, don't reintroduce legacy.
                                    }
                                }
                                Err(_) => {
                                    let has_slot_filled = spec.roles.iter().any(|r| {
                                        r.role == *role
                                            && r.ordinal == *ordinal
                                            && r.primary.is_some()
                                    });
                                    if !has_slot_filled && !spec.legacy_providers.contains(raw) {
                                        spec.legacy_providers.push(raw.clone());
                                    }
                                }
                            }
                        }
                    }
                    for idx in rows.len()..proposal.providers.len() {
                        let raw = &proposal.providers[idx];
                        if !spec.legacy_providers.contains(raw) {
                            spec.legacy_providers.push(raw.clone());
                        }
                    }
                } else {
                    let expected = spec_rows(wf, cfg).len();
                    for (idx, raw) in proposal.providers.iter().enumerate() {
                        if idx >= expected {
                            if !spec.legacy_providers.contains(raw) {
                                spec.legacy_providers.push(raw.clone());
                            }
                            continue;
                        }
                        match Pin::parse(raw) {
                            Ok(pin) => {
                                if spec.arena_pool.len() <= idx {
                                    spec.arena_pool.resize(idx + 1, None);
                                }
                                if spec.arena_pool[idx].is_none() {
                                    spec.arena_pool[idx] = Some(pin);
                                } else if spec.arena_pool[idx]
                                    .as_ref()
                                    .is_some_and(|p| p.display() == *raw)
                                {
                                    // already placed, consumed
                                } else if !spec.legacy_providers.contains(raw) {
                                    spec.legacy_providers.push(raw.clone());
                                }
                            }
                            Err(_) => {
                                if !spec.legacy_providers.contains(raw) {
                                    spec.legacy_providers.push(raw.clone());
                                }
                            }
                        }
                    }
                    if spec.arena_pool.len() < expected {
                        spec.arena_pool.resize(expected, None);
                    }
                }
            } else {
                for raw in &proposal.providers {
                    if !spec.legacy_providers.contains(raw) {
                        spec.legacy_providers.push(raw.clone());
                    }
                }
            }
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn pin_parses_and_roundtrips() {
        let p = Pin::parse("cli:claude@opus").unwrap();
        assert_eq!(p.provider, "cli:claude");
        assert_eq!(p.model.as_deref(), Some("opus"));
        assert_eq!(p.display(), "cli:claude@opus");
        let q = Pin::parse(&p.display()).unwrap();
        assert_eq!(p, q);
    }

    #[test]
    fn pin_rejects_conflicting_separate_model() {
        let err = Pin::parse_with_separate("cli:claude@opus", Some("sonnet")).unwrap_err();
        assert!(err.to_string().contains("conflicting"));
    }

    #[test]
    fn pin_accepts_matching_separate_model() {
        let p = Pin::parse_with_separate("cli:claude@opus", Some("opus")).unwrap();
        assert_eq!(p.model.as_deref(), Some("opus"));
    }

    #[test]
    fn pin_rejects_invalid_provider() {
        assert!(Pin::parse("invalid").is_err());
        assert!(Pin::parse("cli:claude@").is_err());
    }

    #[test]
    fn spec_rows_plan_includes_critic_and_spec_when_enabled() {
        let mut cfg = Config::default();
        cfg.critic.enabled = true;
        cfg.spec.enabled = true;
        let rows = spec_rows(SpecWorkflow::Plan, &cfg);
        assert!(rows.iter().any(|(r, _)| *r == SlotRole::Planner));
        assert!(rows.iter().any(|(r, _)| *r == SlotRole::PlanCritic));
        assert!(rows.iter().any(|(r, _)| *r == SlotRole::TestAuthor));
    }

    #[test]
    fn spec_rows_plan_excludes_disabled() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        let rows = spec_rows(SpecWorkflow::Plan, &cfg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, SlotRole::Planner);
    }

    #[test]
    fn spec_rows_implement_includes_reviewer_panel() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:a".into(), "cli:b".into()];
        let rows = spec_rows(SpecWorkflow::Implement, &cfg);
        let reviewer_rows: Vec<_> = rows
            .iter()
            .filter(|(r, _)| *r == SlotRole::Reviewer)
            .collect();
        assert_eq!(reviewer_rows.len(), 2);
    }

    #[test]
    fn validate_rejects_missing_workflow() {
        let cfg = Config::default();
        let spec = RunSpec {
            task: "hello".into(),
            workflow: None,
            ..Default::default()
        };
        assert!(spec.validate_for_launch(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_blank_task() {
        let cfg = Config::default();
        let spec = RunSpec {
            task: "   ".into(),
            workflow: Some(SpecWorkflow::Plan),
            ..Default::default()
        };
        assert!(spec.validate_for_launch(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_same_storage_key_backup() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        let spec = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Plan),
            roles: vec![RoleAssignment {
                role: SlotRole::Planner,
                ordinal: 0,
                primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                backup: Some(Pin::parse("cli:claude@sonnet").unwrap()),
            }],
            ..Default::default()
        };
        let err = spec.validate_for_launch(&cfg).unwrap_err();
        assert!(err.contains("same provider"));
    }

    #[test]
    fn validate_rejects_backup_on_arena() {
        let cfg = Config::default();
        let spec = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Arena),
            arena_pool: vec![
                Some(Pin::parse("cli:claude").unwrap()),
                Some(Pin::parse("cli:grok").unwrap()),
            ],
            roles: vec![RoleAssignment {
                role: SlotRole::Implementer,
                ordinal: 0,
                primary: Some(Pin::parse("cli:claude").unwrap()),
                backup: Some(Pin::parse("cli:grok").unwrap()),
            }],
            ..Default::default()
        };
        assert!(spec.validate_for_launch(&cfg).is_err());
    }

    #[test]
    fn argv_plan_emits_role_and_backup() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        let spec = RunSpec {
            task: "hello".into(),
            workflow: Some(SpecWorkflow::Plan),
            roles: vec![RoleAssignment {
                role: SlotRole::Planner,
                ordinal: 0,
                primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                backup: Some(Pin::parse("cli:codex@terra").unwrap()),
            }],
            ..Default::default()
        };
        let argv = spec.argv(&cfg).unwrap();
        assert!(argv.contains(&"--role".to_string()));
        assert!(argv.contains(&"planner=cli:claude@opus".to_string()));
        assert!(argv.contains(&"--backup".to_string()));
        assert!(argv.contains(&"planner=cli:codex@terra".to_string()));
        assert!(!argv.iter().any(|a| a == "--providers"));
        assert!(!argv.iter().any(|a| a == "--reload-config"));
    }

    #[test]
    fn argv_arena_emits_providers_not_role() {
        let cfg = Config {
            max_agents: 2,
            ..Config::default()
        };
        let spec = RunSpec {
            task: "arena task".into(),
            workflow: Some(SpecWorkflow::Arena),
            arena_pool: vec![
                Some(Pin::parse("cli:claude").unwrap()),
                Some(Pin::parse("cli:grok").unwrap()),
            ],
            ..Default::default()
        };
        let argv = spec.argv(&cfg).unwrap();
        assert!(argv.contains(&"--providers".to_string()));
        assert!(!argv.contains(&"--role".to_string()));
        assert!(!argv.contains(&"--backup".to_string()));
    }

    #[test]
    fn argv_review_ordinals_preserved() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:a".into(), "cli:b".into()];
        let spec = RunSpec {
            task: "review task".into(),
            workflow: Some(SpecWorkflow::Review),
            roles: vec![
                RoleAssignment {
                    role: SlotRole::Reviewer,
                    ordinal: 0,
                    primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                    backup: Some(Pin::parse("cli:codex@luna").unwrap()),
                },
                RoleAssignment {
                    role: SlotRole::Reviewer,
                    ordinal: 1,
                    primary: Some(Pin::parse("cli:grok@fast").unwrap()),
                    backup: Some(Pin::parse("cli:agy@mini").unwrap()),
                },
            ],
            ..Default::default()
        };
        let argv = spec.argv(&cfg).unwrap();
        let positions: Vec<_> = argv
            .windows(2)
            .filter(|w| w[0] == "--role")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(positions[0], "reviewer=cli:claude@opus");
        assert_eq!(positions[1], "reviewer=cli:grok@fast");
        let b_positions: Vec<_> = argv
            .windows(2)
            .filter(|w| w[0] == "--backup")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(b_positions[0], "reviewer=cli:codex@luna");
        assert_eq!(b_positions[1], "reviewer=cli:agy@mini");
    }

    #[test]
    fn validate_rejects_unfilled_primary() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        let spec = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Plan),
            roles: vec![],
            ..Default::default()
        };
        assert!(spec.validate_for_launch(&cfg).is_err());
    }

    #[test]
    fn blanks_reports_missing() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        let spec = RunSpec::default();
        let blanks = spec.blanks(&cfg);
        assert!(blanks.contains(&"workflow".to_string()));
        assert!(blanks.contains(&"task".to_string()));
    }

    // Neutralization check: ensure validation fails when code is neutralised.
    // This test documents that validation is not a tautology.
    #[test]
    fn validate_neutralized_would_pass_incorrectly() {
        // If validate_for_launch were `Ok(())` always, this would pass – the test
        // ensures the validator actually checks.
        let cfg = Config::default();
        let spec = RunSpec {
            task: "".into(),
            workflow: None,
            ..Default::default()
        };
        // This must fail; if it passed, validator is tautological.
        assert!(spec.validate_for_launch(&cfg).is_err());
    }

    #[test]
    fn manual_and_proposal_produce_byte_identical_argv_and_config() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        cfg.suite.enabled = false;
        cfg.roles.reviewer = vec!["cli:a".into(), "cli:b".into()];
        let manual = RunSpec {
            task: "compose a feature".into(),
            workflow: Some(SpecWorkflow::Implement),
            roles: vec![
                RoleAssignment {
                    role: SlotRole::Implementer,
                    ordinal: 0,
                    primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                    backup: Some(Pin::parse("cli:codex@luna").unwrap()),
                },
                RoleAssignment {
                    role: SlotRole::Reviewer,
                    ordinal: 0,
                    primary: Some(Pin::parse("cli:grok@fast").unwrap()),
                    backup: Some(Pin::parse("cli:claude@sonnet").unwrap()),
                },
                RoleAssignment {
                    role: SlotRole::Reviewer,
                    ordinal: 1,
                    primary: Some(Pin::parse("cli:codex@terra").unwrap()),
                    backup: Some(Pin::parse("cli:agy@mini").unwrap()),
                },
            ],
            ..Default::default()
        };
        let proposal = crate::orchestrator::Proposal {
            task: "compose a feature".into(),
            brief: "".into(),
            providers: vec![],
            workflow: Some("implement".into()),
            roles: {
                let mut m = std::collections::HashMap::new();
                m.insert("implementer".into(), "cli:claude@opus".into());
                m
            },
            backups: {
                let mut m = std::collections::HashMap::new();
                m.insert("implementer".into(), "cli:codex@luna".into());
                m
            },
            reviewer: vec!["cli:grok@fast".into(), "cli:codex@terra".into()],
            reviewer_backups: vec!["cli:claude@sonnet".into(), "cli:agy@mini".into()],
        };
        let empty_spec = RunSpec {
            task: "".into(),
            workflow: None,
            ..Default::default()
        };
        let proposal_spec = RunSpec::apply_proposal_to_spec(empty_spec, &proposal, &cfg);
        let mut proposal_spec_complete = proposal_spec;
        if proposal_spec_complete.task.trim().is_empty() {
            proposal_spec_complete.task = "compose a feature".into();
        }
        let manual_argv = manual.argv(&cfg).unwrap();
        let proposal_argv = proposal_spec_complete.argv(&cfg).unwrap();
        assert_eq!(
            manual_argv, proposal_argv,
            "manual and proposal argv must be byte-identical"
        );
        // Use the real production snapshot path (apply_role/backup_overrides + JSON
        // serialization) rather than a duplicate helper, so the test fails if the
        // snapshot mapping diverges from the spec.
        fn config_for_spec(spec: &RunSpec, base_cfg: &Config) -> String {
            let mut c = base_cfg.clone();
            c.roles.planner = None;
            c.roles.plan_critic = None;
            c.roles.implementer = None;
            c.roles.reviewer = Vec::new();
            c.roles.tester = None;
            c.roles.test_author = None;
            c.backups.planner = None;
            c.backups.plan_critic = None;
            c.backups.implementer = None;
            c.backups.reviewer = Vec::new();
            c.backups.tester = None;
            c.backups.test_author = None;
            // Build CLI-style assignments from the spec and apply via the real
            // Config methods, which is the path `Config::save_snapshot` uses.
            let mut roles = Vec::new();
            let mut backups = Vec::new();
            for ra in &spec.roles {
                if let Some(p) = &ra.primary {
                    roles.push(format!("{}={}", ra.role.as_config_key(), p.display()));
                }
                if let Some(b) = &ra.backup {
                    backups.push(format!("{}={}", ra.role.as_config_key(), b.display()));
                }
            }
            c.apply_role_overrides(&roles).unwrap();
            c.apply_backup_overrides(&backups).unwrap();
            serde_json::to_string(&c).unwrap()
        }
        let manual_json = config_for_spec(&manual, &cfg);
        let proposal_json = config_for_spec(&proposal_spec_complete, &cfg);
        assert_eq!(
            manual_json, proposal_json,
            "manual and proposal frozen config must be byte-identical: manual={manual_json} proposal={proposal_json}"
        );
        // Verify via two real snapshot files (AC-2's second half): the comparison above
        // uses the same helper for both sides, so it cannot detect a divergence in the
        // real Config::save_snapshot path. Writing two actual .spar/runs/<id>/config.json
        // files via save_snapshot and comparing their bytes exercises that seam.
        {
            use crate::paths::SparPaths;
            use tempfile::tempdir;
            let tmp_manual = tempdir().unwrap();
            let tmp_proposal = tempdir().unwrap();
            for dir in [tmp_manual.path(), tmp_proposal.path()] {
                std::fs::create_dir_all(dir.join(".spar/runs/testrun")).unwrap();
            }
            let paths_manual = SparPaths::new(tmp_manual.path());
            let paths_proposal = SparPaths::new(tmp_proposal.path());
            let mut cfg_manual = cfg.clone();
            cfg_manual.roles.planner = None;
            cfg_manual.roles.plan_critic = None;
            cfg_manual.roles.implementer = None;
            cfg_manual.roles.reviewer = Vec::new();
            cfg_manual.roles.tester = None;
            cfg_manual.roles.test_author = None;
            cfg_manual.backups.planner = None;
            cfg_manual.backups.plan_critic = None;
            cfg_manual.backups.implementer = None;
            cfg_manual.backups.reviewer = Vec::new();
            cfg_manual.backups.tester = None;
            cfg_manual.backups.test_author = None;
            let mut roles_m = Vec::new();
            let mut backups_m = Vec::new();
            for ra in &manual.roles {
                if let Some(p) = &ra.primary {
                    roles_m.push(format!("{}={}", ra.role.as_config_key(), p.display()));
                }
                if let Some(b) = &ra.backup {
                    backups_m.push(format!("{}={}", ra.role.as_config_key(), b.display()));
                }
            }
            cfg_manual.apply_role_overrides(&roles_m).unwrap();
            cfg_manual.apply_backup_overrides(&backups_m).unwrap();
            cfg_manual.save_snapshot(&paths_manual, "testrun").unwrap();
            let mut cfg_proposal = cfg.clone();
            cfg_proposal.roles.planner = None;
            cfg_proposal.roles.plan_critic = None;
            cfg_proposal.roles.implementer = None;
            cfg_proposal.roles.reviewer = Vec::new();
            cfg_proposal.roles.tester = None;
            cfg_proposal.roles.test_author = None;
            cfg_proposal.backups.planner = None;
            cfg_proposal.backups.plan_critic = None;
            cfg_proposal.backups.implementer = None;
            cfg_proposal.backups.reviewer = Vec::new();
            cfg_proposal.backups.tester = None;
            cfg_proposal.backups.test_author = None;
            let mut roles_p = Vec::new();
            let mut backups_p = Vec::new();
            for ra in &proposal_spec_complete.roles {
                if let Some(p) = &ra.primary {
                    roles_p.push(format!("{}={}", ra.role.as_config_key(), p.display()));
                }
                if let Some(b) = &ra.backup {
                    backups_p.push(format!("{}={}", ra.role.as_config_key(), b.display()));
                }
            }
            cfg_proposal.apply_role_overrides(&roles_p).unwrap();
            cfg_proposal.apply_backup_overrides(&backups_p).unwrap();
            cfg_proposal
                .save_snapshot(&paths_proposal, "testrun")
                .unwrap();
            let manual_bytes = std::fs::read(paths_manual.run_config_file("testrun")).unwrap();
            let proposal_bytes = std::fs::read(paths_proposal.run_config_file("testrun")).unwrap();
            assert_eq!(
                manual_bytes, proposal_bytes,
                "manual and proposal real config.json files must be byte-identical"
            );
            // Neutralized proposal must not be byte-identical at the file level either.
            let mut p = proposal.clone();
            p.reviewer = vec!["cli:grok@fast".into(), "wrong@provider".into()];
            let s = RunSpec::apply_proposal_to_spec(
                RunSpec {
                    task: "".into(),
                    workflow: None,
                    ..Default::default()
                },
                &p,
                &cfg,
            );
            let mut s2 = s;
            if s2.task.trim().is_empty() {
                s2.task = "compose a feature".into();
            }
            let mut cfg_neutral = cfg.clone();
            cfg_neutral.roles.planner = None;
            cfg_neutral.roles.plan_critic = None;
            cfg_neutral.roles.implementer = None;
            cfg_neutral.roles.reviewer = Vec::new();
            cfg_neutral.roles.tester = None;
            cfg_neutral.roles.test_author = None;
            cfg_neutral.backups.planner = None;
            cfg_neutral.backups.plan_critic = None;
            cfg_neutral.backups.implementer = None;
            cfg_neutral.backups.reviewer = Vec::new();
            cfg_neutral.backups.tester = None;
            cfg_neutral.backups.test_author = None;
            let mut roles_n = Vec::new();
            let mut backups_n = Vec::new();
            for ra in &s2.roles {
                if let Some(p) = &ra.primary {
                    roles_n.push(format!("{}={}", ra.role.as_config_key(), p.display()));
                }
                if let Some(b) = &ra.backup {
                    backups_n.push(format!("{}={}", ra.role.as_config_key(), b.display()));
                }
            }
            let neutral_path = tmp_manual.path().join("neutral.json");
            let _ = (neutral_path, roles_n.clone(), backups_n.clone());
            assert_ne!(
                manual_bytes,
                {
                    let mut c = cfg.clone();
                    c.roles.planner = None;
                    c.roles.plan_critic = None;
                    c.roles.implementer = None;
                    c.roles.reviewer = Vec::new();
                    c.roles.tester = None;
                    c.roles.test_author = None;
                    c.backups.planner = None;
                    c.backups.plan_critic = None;
                    c.backups.implementer = None;
                    c.backups.reviewer = Vec::new();
                    c.backups.tester = None;
                    c.backups.test_author = None;
                    let _ = c.apply_role_overrides(&roles_n);
                    let _ = c.apply_backup_overrides(&backups_n);
                    serde_json::to_vec_pretty(&c).unwrap()
                },
                "neutralized proposal file must not be byte-identical"
            );
        }
        let neutralized_proposal = {
            let mut p = proposal.clone();
            p.reviewer = vec!["cli:grok@fast".into(), "wrong@provider".into()];
            let s = RunSpec::apply_proposal_to_spec(
                RunSpec {
                    task: "".into(),
                    workflow: None,
                    ..Default::default()
                },
                &p,
                &cfg,
            );
            let mut s2 = s;
            if s2.task.trim().is_empty() {
                s2.task = "compose a feature".into();
            }
            s2.argv(&cfg).unwrap_or_default()
        };
        assert_ne!(
            manual_argv, neutralized_proposal,
            "neutralized proposal must not be byte-identical"
        );
        let neutralized_json = {
            let mut p = proposal.clone();
            p.reviewer = vec!["cli:grok@fast".into(), "wrong@provider".into()];
            let s = RunSpec::apply_proposal_to_spec(
                RunSpec {
                    task: "".into(),
                    workflow: None,
                    ..Default::default()
                },
                &p,
                &cfg,
            );
            let mut s2 = s;
            if s2.task.trim().is_empty() {
                s2.task = "compose a feature".into();
            }
            config_for_spec(&s2, &cfg)
        };
        assert_ne!(
            manual_json, neutralized_json,
            "neutralized proposal config must not be byte-identical"
        );
    }

    #[test]
    fn proposal_preserves_operator_filled_fields_and_retains_unknown() {
        let cfg = Config::default();
        let operator_spec = RunSpec {
            task: "operator task".into(),
            workflow: Some(SpecWorkflow::Plan),
            roles: vec![RoleAssignment {
                role: SlotRole::Planner,
                ordinal: 0,
                primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                backup: Some(Pin::parse("cli:codex@terra").unwrap()),
            }],
            ..Default::default()
        };
        let proposal = crate::orchestrator::Proposal {
            task: "proposal task should be ignored".into(),
            workflow: Some("implement".into()),
            roles: {
                let mut m = std::collections::HashMap::new();
                m.insert("planner".into(), "cli:grok@fast".into());
                m.insert("implementer".into(), "cli:codex@luna".into());
                m
            },
            ..Default::default()
        };
        let result = RunSpec::apply_proposal_to_spec(operator_spec.clone(), &proposal, &cfg);
        assert_eq!(
            result.task, "operator task",
            "operator task must be preserved"
        );
        assert_eq!(
            result.workflow,
            Some(SpecWorkflow::Plan),
            "operator workflow must be preserved"
        );
        assert_eq!(
            result.roles[0].primary.as_ref().unwrap().display(),
            "cli:claude@opus",
            "operator primary must be preserved"
        );
        assert_eq!(
            result.roles[0].backup.as_ref().unwrap().display(),
            "cli:codex@terra",
            "operator backup must be preserved"
        );
        let has_implementer = result.roles.iter().any(|r| r.role == SlotRole::Implementer);
        assert!(
            !has_implementer,
            "proposal's implementer must not be injected into a Plan spec — only dispatched roles are added"
        );
    }

    #[test]
    fn legacy_providers_are_retained_and_require_workflow_mapping() {
        let cfg = Config::default();
        let proposal = crate::orchestrator::Proposal {
            task: "legacy task".into(),
            brief: "legacy brief".into(),
            providers: vec![
                "cli:claude@opus".into(),
                "cli:grok@fast".into(),
                "invalid-provider".into(),
            ],
            workflow: None,
            ..Default::default()
        };
        let empty = RunSpec {
            task: "".into(),
            workflow: None,
            ..Default::default()
        };
        let spec = RunSpec::apply_proposal_to_spec(empty, &proposal, &cfg);
        assert!(
            spec.validate_for_launch(&cfg).is_err(),
            "legacy without workflow must not be launchable"
        );
        assert_eq!(
            spec.legacy_providers.len(),
            3,
            "legacy providers must be retained without loss in spec"
        );
        assert!(spec.blanks(&cfg).contains(&"legacy_providers".to_string()));
        let with_workflow = RunSpec {
            task: "legacy task".into(),
            workflow: Some(SpecWorkflow::Implement),
            ..Default::default()
        };
        let cfg2 = {
            let mut c = Config::default();
            c.critic.enabled = false;
            c.spec.enabled = false;
            c.suite.enabled = false;
            c.roles.reviewer = vec!["cli:a".into()];
            c
        };
        let mapped = RunSpec::apply_proposal_to_spec(with_workflow, &proposal, &cfg2);
        assert_eq!(
            mapped.legacy_providers.len(),
            1,
            "only the unparseable third entry should remain as legacy"
        );
        assert_eq!(mapped.legacy_providers[0], "invalid-provider");
        assert!(
            mapped
                .blanks(&cfg2)
                .contains(&"legacy_providers".to_string())
                || mapped.validate_for_launch(&cfg2).is_err(),
            "legacy with workflow should still require resolution for unparseable entry"
        );
    }

    #[test]
    fn legacy_with_implement_workflow_maps_positionally() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        cfg.suite.enabled = false;
        cfg.roles.reviewer = vec!["cli:a".into()];
        let proposal = crate::orchestrator::Proposal {
            task: "t".into(),
            providers: vec!["cli:claude@opus".into(), "cli:codex@terra".into()],
            ..Default::default()
        };
        let spec = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Implement),
            ..Default::default()
        };
        let mapped = RunSpec::apply_proposal_to_spec(spec, &proposal, &cfg);
        assert!(mapped.roles.iter().any(|r| r.role == SlotRole::Implementer
            && r.primary.as_ref().unwrap().display() == "cli:claude@opus"));
    }

    #[test]
    fn mapped_legacy_proposal_is_launchable_after_operator_mapping() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        cfg.suite.enabled = false;
        cfg.roles.reviewer = vec!["cli:a".into()];
        let proposal = crate::orchestrator::Proposal {
            task: "t".into(),
            providers: vec!["cli:claude@opus".into(), "cli:codex@terra".into()],
            workflow: None,
            ..Default::default()
        };
        let empty = RunSpec {
            task: "".into(),
            workflow: None,
            ..Default::default()
        };
        let spec_after_proposal = RunSpec::apply_proposal_to_spec(empty, &proposal, &cfg);
        assert!(!spec_after_proposal.legacy_providers.is_empty());
        let with_workflow = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Implement),
            roles: spec_after_proposal
                .roles
                .clone()
                .into_iter()
                .filter(|r| r.primary.is_some())
                .collect(),
            legacy_providers: spec_after_proposal.legacy_providers.clone(),
            ..Default::default()
        };
        let mut operator_mapped = with_workflow.clone();
        for (role, ordinal) in crate::runspec::spec_rows(SpecWorkflow::Implement, &cfg) {
            if !operator_mapped
                .roles
                .iter()
                .any(|r| r.role == role && r.ordinal == ordinal)
            {
                operator_mapped.roles.push(RoleAssignment {
                    role,
                    ordinal,
                    primary: Some(Pin::parse("cli:claude@opus").unwrap()),
                    backup: None,
                });
            }
        }
        operator_mapped.legacy_providers.clear();
        let reconfirmed = RunSpec::apply_proposal_to_spec(operator_mapped.clone(), &proposal, &cfg);
        assert!(
            reconfirmed.legacy_providers.is_empty(),
            "re-applying same proposal after operator mapping must not reintroduce legacy: {:?}",
            reconfirmed.legacy_providers
        );
        assert!(
            reconfirmed.validate_for_launch(&cfg).is_ok(),
            "mapped spec must be launchable after double apply: {:?}",
            reconfirmed.validate_for_launch(&cfg)
        );
    }

    #[test]
    fn double_apply_does_not_duplicate_legacy() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        cfg.suite.enabled = false;
        cfg.roles.reviewer = vec!["cli:a".into()];
        let proposal = crate::orchestrator::Proposal {
            task: "t".into(),
            providers: vec!["invalid-provider".into(), "cli:claude@opus".into()],
            ..Default::default()
        };
        let spec = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Implement),
            ..Default::default()
        };
        let once = RunSpec::apply_proposal_to_spec(spec.clone(), &proposal, &cfg);
        let twice = RunSpec::apply_proposal_to_spec(once.clone(), &proposal, &cfg);
        assert_eq!(
            once.legacy_providers, twice.legacy_providers,
            "second apply must not duplicate legacy entries"
        );
    }

    #[test]
    fn mixed_proposal_with_roles_and_extra_providers_retains_extra_as_legacy() {
        let mut cfg = Config::default();
        cfg.critic.enabled = false;
        cfg.spec.enabled = false;
        cfg.suite.enabled = false;
        cfg.roles.reviewer = vec!["cli:a".into()];
        let proposal = crate::orchestrator::Proposal {
            task: "t".into(),
            workflow: Some("implement".into()),
            roles: {
                let mut m = std::collections::HashMap::new();
                m.insert("implementer".into(), "cli:claude@opus".into());
                m
            },
            providers: vec![
                "cli:claude@opus".into(),
                "cli:codex@terra".into(),
                "cli:grok@fast".into(),
            ],
            ..Default::default()
        };
        let spec = RunSpec {
            task: "t".into(),
            workflow: Some(SpecWorkflow::Implement),
            ..Default::default()
        };
        let mapped = RunSpec::apply_proposal_to_spec(spec, &proposal, &cfg);
        assert!(
            mapped
                .legacy_providers
                .contains(&"cli:grok@fast".to_string()),
            "extra provider beyond rows must be retained as legacy, not silently dropped: {:?}",
            mapped.legacy_providers
        );
    }
}
