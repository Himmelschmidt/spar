//! `spar provider list --usage` (O132) against fixtures: a fake `muse` on an otherwise
//! empty PATH answers `usage --json`, and HOME holds a codex session log. No real
//! provider CLI can run.
use assert_cmd::cargo::cargo_bin_cmd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::tempdir;

const MUSE_USAGE: &str = r#"{"allowance":{"asOfMs":1793397310429,"source":"mint","tier":"Muse Code Max Usage","weekly":{"resetsAtMs":1793577600000,"usedPercent":27},"window":{"resetsAtMs":1793404987000,"usedPercent":4}},"hint":null,"reason":null,"schemaVersion":1}"#;

const CODEX_RECORD: &str = r#"{"timestamp":"2026-10-30T21:47:05.485Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1793774154},"secondary":null,"credits":{"has_credits":true,"unlimited":false,"balance":"20047.5535505000"},"individual_limit":null,"spend_control_reached":null,"plan_type":"pro","rate_limit_reached_type":null}}}"#;

fn fake_muse(bin: &Path) {
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = usage ]; then echo '{MUSE_USAGE}'; exit 0; fi\necho 'Muse Code 1.4.14'\n"
    );
    let path = bin.join("muse");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn usage_reads_muse_and_codex_and_says_why_claude_has_none() {
    let home = tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    fake_muse(&bin);
    let day = home.path().join(".codex/sessions/2026/10/30");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(
        day.join("rollout-2026-10-30T17-45-13-x.jsonl"),
        format!("{{\"timestamp\":\"2026-10-30T21:00:00Z\",\"type\":\"session_meta\"}}\n{CODEX_RECORD}\n"),
    )
    .unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".spar")).unwrap();

    let out = cargo_bin_cmd!("spar")
        .current_dir(project.path())
        .env("PATH", &bin)
        .env("HOME", home.path())
        .env("SPAR_HOME", home.path().join(".spar-home"))
        .env_remove("CODEX_HOME")
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .args(["provider", "list", "--usage", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let by_name = |n: &str| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == n)
            .unwrap_or_else(|| panic!("{n} in {v}"))
            .clone()
    };

    let muse = by_name("muse");
    assert_eq!(muse["available"], true, "{muse}");
    let u = &muse["usage"];
    assert_eq!(u["schema_version"], 1);
    assert_eq!(u["status"], "ok", "{u}");
    assert_eq!(u["source"], "muse usage --json");
    assert_eq!(u["windows"][0]["name"], "5h");
    assert_eq!(u["windows"][0]["used_percent"], 4.0);
    assert_eq!(u["windows"][1]["name"], "weekly");
    assert_eq!(u["windows"][1]["used_percent"], 27.0);
    assert_eq!(u["plan"], "Muse Code Max Usage");

    let codex = by_name("codex");
    let u = &codex["usage"];
    assert_eq!(u["status"], "ok", "{u}");
    assert_eq!(u["windows"][0]["name"], "weekly");
    assert_eq!(u["windows"][0]["used_percent"], 100.0);
    assert_eq!(u["credits"], "20047.5535505000");
    assert_eq!(u["as_of"], "2026-10-30T21:47:05.485Z");

    let claude = by_name("claude");
    assert_eq!(claude["usage"]["status"], "unsupported");
    assert!(claude["usage"]["message"]
        .as_str()
        .unwrap()
        .contains("non-billed"));

    let plain = cargo_bin_cmd!("spar")
        .current_dir(project.path())
        .env("PATH", &bin)
        .env("HOME", home.path())
        .env("SPAR_HOME", home.path().join(".spar-home"))
        .env_remove("SPAR_PROJECT_ROOT")
        .args(["provider", "list", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&plain.get_output().stdout).unwrap();
    assert!(
        v.as_array()
            .unwrap()
            .iter()
            .all(|p| p.get("usage").is_none()),
        "no probe without --usage: {v}"
    );

    let human = cargo_bin_cmd!("spar")
        .current_dir(project.path())
        .env("PATH", &bin)
        .env("HOME", home.path())
        .env("SPAR_HOME", home.path().join(".spar-home"))
        .env_remove("CODEX_HOME")
        .env_remove("SPAR_PROJECT_ROOT")
        .args(["provider", "list", "--usage"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&human.get_output().stdout).to_string();
    assert!(text.contains("usage: 5h 4%"), "{text}");
    assert!(text.contains("weekly 100%"), "{text}");
}
