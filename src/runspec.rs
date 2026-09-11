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
                    if pin.provider.trim().is_empty() {
                        return Err(format!("arena[{}] has empty provider", idx));
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
                        if pin.provider.trim().is_empty() {
                            return Err(format!(
                                "primary for {}[{}] is empty",
                                role.as_config_key(),
                                ordinal
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

    #[allow(dead_code)]
    pub fn apply_proposal_to_spec(
        mut spec: RunSpec,
        proposal: &crate::orchestrator::Proposal,
        _cfg: &Config,
    ) -> RunSpec {
        if spec.task.trim().is_empty() && !proposal.task.trim().is_empty() {
            spec.task = proposal.task.clone();
        }
        if spec.workflow.is_none() {
            if let Some(wf) = proposal.workflow.as_deref().and_then(SpecWorkflow::parse) {
                spec.workflow = Some(wf);
            }
        }
        let mut proposal_roles: std::collections::HashMap<(SlotRole, usize), Pin> =
            std::collections::HashMap::new();
        for (k, v) in &proposal.roles {
            if let Some(role) = SlotRole::from_config_key(k) {
                if let Ok(pin) = Pin::parse(v) {
                    proposal_roles.insert((role, 0), pin);
                }
            }
        }
        for (idx, v) in proposal.reviewer.iter().enumerate() {
            if let Ok(pin) = Pin::parse(v) {
                proposal_roles.insert((SlotRole::Reviewer, idx), pin);
            }
        }
        let mut proposal_backups: std::collections::HashMap<(SlotRole, usize), Pin> =
            std::collections::HashMap::new();
        for (k, v) in &proposal.backups {
            if let Some(role) = SlotRole::from_config_key(k) {
                if let Ok(pin) = Pin::parse(v) {
                    proposal_backups.insert((role, 0), pin);
                }
            }
        }
        for (idx, v) in proposal.reviewer_backups.iter().enumerate() {
            if let Ok(pin) = Pin::parse(v) {
                proposal_backups.insert((SlotRole::Reviewer, idx), pin);
            }
        }
        for ((role, ordinal), pin) in proposal_roles {
            if let Some(existing) = spec
                .roles
                .iter_mut()
                .find(|r| r.role == role && r.ordinal == ordinal)
            {
                if existing.primary.is_none() {
                    existing.primary = Some(pin);
                }
            } else {
                spec.roles.push(RoleAssignment {
                    role,
                    ordinal,
                    primary: Some(pin),
                    backup: None,
                });
            }
        }
        for ((role, ordinal), pin) in proposal_backups {
            if let Some(existing) = spec
                .roles
                .iter_mut()
                .find(|r| r.role == role && r.ordinal == ordinal)
            {
                if existing.backup.is_none() {
                    existing.backup = Some(pin);
                }
            } else {
                spec.roles.push(RoleAssignment {
                    role,
                    ordinal,
                    primary: None,
                    backup: Some(pin),
                });
            }
        }
        if spec.workflow.is_none() && !proposal.providers.is_empty() {
            for prov in &proposal.providers {
                if let Ok(pin) = Pin::parse(prov) {
                    let _ = pin;
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
                    backup: Some(Pin::parse("cli:claude@sonnet").unwrap()),
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
        assert_eq!(b_positions[1], "reviewer=cli:claude@sonnet");
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
}
