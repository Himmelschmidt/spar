use crate::exit_codes::ExitCode;
use anyhow::{bail, Result};
use serde::Serialize;

const CORE_SKILL: &str = include_str!("../skills/core.md");

#[derive(Debug, Serialize)]
struct SkillMeta {
    name: String,
    description: String,
}

pub fn run(action: SkillsAction) -> Result<ExitCode> {
    match action {
        SkillsAction::List { json } => list(json),
        SkillsAction::Get { name } => get(&name),
    }
}

#[derive(Debug)]
pub enum SkillsAction {
    List { json: bool },
    Get { name: String },
}

fn catalog() -> Vec<SkillMeta> {
    vec![SkillMeta {
        name: "core".into(),
        description: "How outer agents drive spar via CLI, exit codes, and discovery".into(),
    }]
}

fn list(json: bool) -> Result<ExitCode> {
    let items = catalog();
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for s in &items {
            println!("{:<12} {}", s.name, s.description);
        }
    }
    Ok(ExitCode::Success)
}

fn get(name: &str) -> Result<ExitCode> {
    match name {
        "core" => {
            print!("{CORE_SKILL}");
            if !CORE_SKILL.ends_with('\n') {
                println!();
            }
            Ok(ExitCode::Success)
        }
        other => bail!("unknown skill '{other}' (try: spar skills list)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_skill_nonempty() {
        assert!(CORE_SKILL.contains("spar"));
        assert!(CORE_SKILL.contains("exit"));
    }

    #[test]
    fn catalog_has_core() {
        assert!(catalog().iter().any(|s| s.name == "core"));
    }
}
