use std::collections::HashMap;

const PLANNER: &str = include_str!("../templates/planner.md");
const PLAN_CRITIC: &str = include_str!("../templates/plan_critic.md");
const IMPLEMENTER: &str = include_str!("../templates/implementer.md");
const REVIEWER: &str = include_str!("../templates/reviewer_adversarial.md");
const RANKER: &str = include_str!("../templates/ranker.md");
const PEER_HALF: &str = include_str!("../templates/peer_half.md");
const ROLE_FRONTEND: &str = include_str!("../templates/role_frontend.md");
const ROLE_BACKEND: &str = include_str!("../templates/role_backend.md");

pub fn get(name: &str) -> Option<&'static str> {
    match name {
        "planner" | "planner.md" => Some(PLANNER),
        "plan_critic" | "plan_critic.md" => Some(PLAN_CRITIC),
        "implementer" | "implementer.md" => Some(IMPLEMENTER),
        "reviewer" | "reviewer_adversarial" | "reviewer_adversarial.md" => Some(REVIEWER),
        "ranker" | "ranker.md" => Some(RANKER),
        "peer_half" | "peer_half.md" => Some(PEER_HALF),
        "role_frontend" | "role_frontend.md" => Some(ROLE_FRONTEND),
        "role_backend" | "role_backend.md" => Some(ROLE_BACKEND),
        _ => None,
    }
}

pub fn render(name: &str, vars: &HashMap<String, String>) -> anyhow::Result<String> {
    let tmpl = get(name).ok_or_else(|| anyhow::anyhow!("unknown template {name}"))?;
    Ok(render_str(tmpl, vars))
}

pub fn render_str(tmpl: &str, vars: &HashMap<String, String>) -> String {
    let mut out = tmpl.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

pub struct TemplateCtx<'a> {
    pub task: &'a str,
    pub project_root: &'a str,
    pub cwd: &'a str,
    pub run_id: &'a str,
    pub artifacts_dir: &'a str,
    pub markers_dir: &'a str,
    pub mailbox_dir: &'a str,
    pub slot_id: &'a str,
    pub provider: &'a str,
    pub branch: &'a str,
}

pub fn base_vars(ctx: &TemplateCtx<'_>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("task".into(), ctx.task.into());
    m.insert("project_root".into(), ctx.project_root.into());
    m.insert("cwd".into(), ctx.cwd.into());
    m.insert("run_id".into(), ctx.run_id.into());
    m.insert("artifacts_dir".into(), ctx.artifacts_dir.into());
    m.insert("markers_dir".into(), ctx.markers_dir.into());
    m.insert("mailbox_dir".into(), ctx.mailbox_dir.into());
    m.insert("slot_id".into(), ctx.slot_id.into());
    m.insert("provider".into(), ctx.provider.into());
    m.insert("branch".into(), ctx.branch.into());
    m.insert("plan_body".into(), String::new());
    m.insert("review_cwd".into(), ctx.cwd.into());
    m.insert("candidates".into(), String::new());
    m.insert("peer_role".into(), String::new());
    m.insert("partner_slot".into(), String::new());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_placeholders() {
        let mut v = HashMap::new();
        v.insert("task".into(), "fix login".into());
        let s = render("planner", &v).unwrap();
        assert!(s.contains("fix login"));
        assert!(!s.contains("{{task}}"));
    }
}
