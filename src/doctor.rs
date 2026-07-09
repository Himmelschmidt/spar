use crate::config::Config;
use crate::exit_codes::ExitCode;
use crate::paths::{self, SparPaths};
use crate::providers;
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    project_root: Option<PathBuf>,
    spar_dir: Option<PathBuf>,
    max_agents: u32,
    default_backend: String,
    git: ToolCheck,
    tmux: ToolCheck,
    bwrap: ToolCheck,
    providers: Vec<providers::ProviderReport>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ToolCheck {
    name: String,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    required: bool,
}

pub fn run(json: bool) -> Result<ExitCode> {
    let mut notes = Vec::new();
    let project_root = match paths::find_project_root() {
        Ok(p) => Some(p),
        Err(e) => {
            notes.push(format!("project root: {e}"));
            None
        }
    };
    let spar_dir = project_root.as_ref().map(|p| SparPaths::new(p).root);

    let git = check_tool("git", &["git"], true, &["--version"]);
    let tmux = check_tool("tmux", &["tmux"], false, &["-V"]);
    let bwrap = check_tool("bwrap", &["bwrap"], false, &["--version"]);

    if !tmux.available {
        notes.push("tmux not found — interactive backend unavailable; headless still works".into());
    }
    if !bwrap.available {
        notes.push("bwrap not found — optional sandbox backend unavailable".into());
    }

    let providers = providers::detect_all();
    let any_provider = providers.iter().any(|p| p.available);
    if !any_provider {
        notes.push("no first-class providers found on PATH (claude, grok, agy)".into());
    }

    let cfg = project_root
        .as_ref()
        .and_then(|p| Config::load(p).ok())
        .unwrap_or_default();

    let ok = git.available && any_provider;
    let report = DoctorReport {
        ok,
        project_root,
        spar_dir,
        max_agents: cfg.max_agents,
        default_backend: format!("{:?}", cfg.default_backend).to_ascii_lowercase(),
        git,
        tmux,
        bwrap,
        providers,
        notes,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }

    Ok(if report.ok {
        ExitCode::Success
    } else {
        ExitCode::Failure
    })
}

fn check_tool(name: &str, bins: &[&str], required: bool, version_args: &[&str]) -> ToolCheck {
    let path = bins.iter().find_map(|b| which::which(b).ok());
    match path {
        Some(p) => {
            let version = std::process::Command::new(&p)
                .args(version_args)
                .output()
                .ok()
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout);
                    let e = String::from_utf8_lossy(&o.stderr);
                    let text = if s.trim().is_empty() { e } else { s };
                    text.lines().next().map(|l| l.trim().to_string())
                });
            ToolCheck {
                name: name.into(),
                available: true,
                path: Some(p.display().to_string()),
                version,
                required,
            }
        }
        None => ToolCheck {
            name: name.into(),
            available: false,
            path: None,
            version: None,
            required,
        },
    }
}

fn print_human(r: &DoctorReport) {
    println!("spar doctor");
    println!(
        "  status:        {}",
        if r.ok { "ok" } else { "problems found" }
    );
    match &r.project_root {
        Some(p) => println!("  project_root:  {}", p.display()),
        None => println!("  project_root:  (not found)"),
    }
    if let Some(p) = &r.spar_dir {
        let exists = p.is_dir();
        println!(
            "  spar_dir:     {}{}",
            p.display(),
            if exists { "" } else { " (not created yet)" }
        );
    }
    println!("  max_agents:    {}", r.max_agents);
    println!("  backend:       {}", r.default_backend);
    print_tool(&r.git);
    print_tool(&r.tmux);
    print_tool(&r.bwrap);
    println!("  providers:");
    for p in &r.providers {
        let mark = if p.available { "ok" } else { "--" };
        println!(
            "    [{mark}] {:<8} {}",
            p.name,
            p.path.as_deref().unwrap_or("not on PATH")
        );
        if p.available {
            println!(
                "           headless={} skip_perms={} sandbox={} version={}",
                p.capabilities.headless,
                p.capabilities.skip_permissions,
                p.capabilities.native_sandbox,
                p.version.as_deref().unwrap_or("?")
            );
        }
    }
    if !r.notes.is_empty() {
        println!("  notes:");
        for n in &r.notes {
            println!("    - {n}");
        }
    }
}

fn print_tool(t: &ToolCheck) {
    let req = if t.required { "required" } else { "optional" };
    if t.available {
        println!(
            "  {:<12}  ok ({req}) {} {}",
            t.name,
            t.path.as_deref().unwrap_or(""),
            t.version.as_deref().unwrap_or("")
        );
    } else {
        println!("  {:<12}  missing ({req})", t.name);
    }
}
