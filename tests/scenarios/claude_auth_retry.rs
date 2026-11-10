//! #241: the credential-refresh retry must not reuse the first attempt's
//! session id. A fake `claude` records every `--session-id` it receives and
//! refuses an id it has already created (the real vendor's `Session ID <id>
//! is already in use`); the first dispatch fails with a revoked-token error,
//! so the retry only succeeds when spar names it a fresh session.
#[path = "common/mod.rs"]
mod common;

use std::process::Stdio;

/// Install a fake `claude` that models the vendor's session registry: every
/// `--session-id` it receives is recorded, and an id it already created is
/// refused exactly the way the real CLI refuses it. The first real dispatch
/// fails with a revoked-token error so the credential-refresh retry fires.
fn install_fake_claude(bin: &std::path::Path, state: &std::path::Path) {
    let fake = &bin.join("claude");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-claude 1.0"; exit 0; fi
if [ "$1" = "auth" ]; then exit 0; fi
sid=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ]; then sid="$a"; fi
  prev="$a"
done
echo "$sid" >> "{state}/ids"
touch "{state}/sessions"
if grep -qxF "$sid" "{state}/sessions" 2>/dev/null; then
  echo "Error: Session ID $sid is already in use." >&2
  exit 1
fi
echo "$sid" >> "{state}/sessions"
if [ ! -f "{state}/authed" ]; then
  touch "{state}/authed"
  echo 'API Error: 401 {{"type":"error","error":{{"type":"authentication_error","message":"OAuth access token has been revoked. Please obtain a new token."}}}}' >&2
  exit 1
fi
run="${{SPAR_AGENT_ID%%:*}}"
slot="${{SPAR_AGENT_ID#*:}}"
art="$FAKE_PROJECT_ROOT/.spar/runs/$run/artifacts"
mkdir -p "$art"
printf '# Summary\nall good\n' > "$art/summary-$slot.md"
printf '# Carry-forward\nnothing pending\n' > "$art/carry-forward-$slot.md"
case "$slot" in
  review-*)
    sha=$(git rev-parse HEAD 2>/dev/null || echo none)
    printf '## Verdict\napprove\n\nReviewed-Commit: %s\n' "$sha" > "$art/review-$slot.md"
    ;;
esac
exit 0
"#,
        state = state.display()
    );
    std::fs::write(fake, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(fake).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(fake, perms).unwrap();
    }
}

#[test]
fn credential_refresh_retry_uses_a_fresh_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "# proj\n").unwrap();
    std::fs::write(
        proj.join("spar.toml"),
        "[suite]\nenabled = false\n[rounds]\nmax = 1\n",
    )
    .unwrap();
    let bin = tmp.path().join("bin");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    install_fake_claude(&bin, &state);

    let mut cmd = common::spar_std();
    cmd.current_dir(&proj)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("FAKE_PROJECT_ROOT", &proj)
        .env("SPAR_AUTH_RETRY_WAIT_SECS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .args([
            "implement",
            "-t",
            "add a summary file",
            "--fleet",
            "small",
            "--providers",
            "cli:claude",
        ])
        .spawn()
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        [0, 2].contains(&out.status.code().unwrap_or(99)),
        "run completes: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let ids: Vec<String> = std::fs::read_to_string(state.join("ids"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        ids.len() >= 2,
        "the first attempt must fail auth and retry: {ids:?}"
    );
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        assert!(!id.is_empty(), "every dispatch carries a session id");
        assert!(
            seen.insert(id.clone()),
            "no session id is ever reused: {ids:?}"
        );
    }
    assert_ne!(
        ids[0], ids[1],
        "the credential retry names a fresh session, not the refused one"
    );

    let run = std::fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .find_map(|e| e.ok())
        .expect("a run exists")
        .file_name();
    let run = run.to_string_lossy().into_owned();
    let events =
        std::fs::read_to_string(proj.join(".spar/runs").join(&run).join("events.jsonl")).unwrap();
    assert!(
        events.contains("rejected the credential"),
        "the auth retry fired: {events}"
    );
    let logs: Vec<String> = walk(&proj.join(".spar/runs").join(&run))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    assert!(
        !logs.iter().any(|l| l.contains("already in use")),
        "no dispatch ever hit the in-use refusal"
    );
    let artifacts: Vec<String> =
        std::fs::read_dir(proj.join(".spar/runs").join(&run).join("artifacts"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
    assert!(
        artifacts.iter().any(|n| n.starts_with("summary-impl")),
        "the implementer wrote its summary through the retry: {artifacts:?}"
    );
    assert!(
        artifacts
            .iter()
            .any(|n| n.starts_with("carry-forward-impl")),
        "the implementer finished through the retry: {artifacts:?}"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
