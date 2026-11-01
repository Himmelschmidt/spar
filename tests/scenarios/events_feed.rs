//! `spar events` (O146): every run's `events.jsonl` as one feed, with a cursor that
//! resumes a disconnected client exactly. Runs here are `events.jsonl` fixtures plus one
//! real dry-run plan for the archive check; no provider ever spawns.

mod common;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    git(&proj, &["init", "-q"]);
    git(&proj, &["config", "user.email", "test@example.com"]);
    git(&proj, &["config", "user.name", "Test"]);
    std::fs::write(proj.join("README.md"), "test\n").unwrap();
    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"off\"\n").unwrap();
    git(&proj, &["add", "."]);
    git(&proj, &["commit", "-q", "-m", "init"]);
    (tmp, proj)
}

fn line(secs: u32, msg: &str) -> String {
    format!(
        "{{\"ts\":\"2027-01-01T00:{:02}:{:02}Z\",\"type\":\"info\",\"message\":\"{msg}\"}}\n",
        secs / 60,
        secs % 60
    )
}

fn append_raw(proj: &Path, run: &str, text: &str) {
    let dir = proj.join(".spar/runs").join(run);
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"))
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn events(proj: &Path, args: &[&str]) -> Vec<Value> {
    let out = common::spar()
        .current_dir(proj)
        .arg("events")
        .args(args)
        .arg("--json")
        .assert()
        .code(0);
    String::from_utf8_lossy(&out.get_output().stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("one json object per line"))
        .collect()
}

fn tags(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            format!(
                "{}/{}",
                l["run_id"].as_str().unwrap(),
                l["message"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

/// A dry-run plan, marked done and archived through the CLI.
fn archived_run(proj: &Path) -> String {
    let out = common::spar()
        .current_dir(proj)
        .args([
            "plan",
            "--task",
            "task",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let id = v["run_id"].as_str().unwrap().to_string();
    let state_path = proj.join(".spar/runs").join(&id).join("state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state["phase"] = Value::String("done".into());
    std::fs::write(&state_path, state.to_string()).unwrap();
    common::spar()
        .current_dir(proj)
        .args(["archive", &id, "--json"])
        .assert()
        .code(0);
    id
}

struct Follower {
    child: std::process::Child,
    rx: mpsc::Receiver<Value>,
}

impl Follower {
    fn start(proj: &Path, extra: &[&str]) -> Self {
        let mut child = common::spar_std()
            .current_dir(proj)
            .args(["events", "--follow", "--json"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines() {
                let Ok(l) = l else { return };
                if tx
                    .send(serde_json::from_str(&l).expect("json line"))
                    .is_err()
                {
                    return;
                }
            }
        });
        Self { child, rx }
    }

    fn next(&self) -> Value {
        self.rx
            .recv_timeout(Duration::from_secs(10))
            .expect("follow emitted a line")
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn backlog_merges_runs_by_time_and_skips_archived_runs() {
    let (_tmp, proj) = project();
    let hidden = archived_run(&proj);
    append_raw(&proj, "aaaa0001", &(line(1, "a1") + &line(4, "a2")));
    append_raw(&proj, "bbbb0002", &(line(2, "b1") + &line(3, "b2")));

    let lines = events(&proj, &[]);
    assert_eq!(
        tags(&lines),
        ["aaaa0001/a1", "bbbb0002/b1", "bbbb0002/b2", "aaaa0001/a2"]
    );
    assert_eq!(lines[0]["type"], "info");
    assert_eq!(lines[0]["ts"], "2027-01-01T00:00:01Z");
    assert!(lines.iter().all(|l| l["cursor"].is_string()));

    // Asked for by name, an archived run is shown.
    let by_name = events(&proj, &["--run", &hidden]);
    assert!(!by_name.is_empty());
    assert!(by_name.iter().all(|l| l["run_id"] == hidden.as_str()));

    let human = common::spar()
        .current_dir(&proj)
        .args(["events", "--run", "aaaa0001"])
        .assert()
        .code(0);
    let text = String::from_utf8_lossy(&human.get_output().stdout).into_owned();
    assert_eq!(text.lines().next().unwrap(), "aaaa0001 00:00:01 a1");

    common::spar()
        .current_dir(&proj)
        .args(["events", "--run", "nope0000"])
        .assert()
        .code(1);
    common::spar()
        .current_dir(&proj)
        .args(["events", "--since", "garbage"])
        .assert()
        .code(1);
}

#[test]
fn follow_starts_at_the_tail_and_picks_up_a_new_run() {
    let (_tmp, proj) = project();
    append_raw(&proj, "aaaa0001", &line(1, "old"));
    let follower = Follower::start(&proj, &[]);
    std::thread::sleep(Duration::from_millis(600));
    append_raw(&proj, "cccc0003", &line(2, "fresh"));
    let got = follower.next();
    assert_eq!(got["run_id"], "cccc0003");
    assert_eq!(got["message"], "fresh");
    append_raw(&proj, "aaaa0001", &line(3, "more"));
    assert_eq!(follower.next()["message"], "more");
}

#[test]
fn torn_last_line_is_emitted_once_whole() {
    let (_tmp, proj) = project();
    let full = line(2, "late");
    append_raw(&proj, "aaaa0001", &(line(1, "first") + &full[..12]));
    let lines = events(&proj, &[]);
    assert_eq!(tags(&lines), ["aaaa0001/first"]);
    append_raw(&proj, "aaaa0001", &full[12..]);
    let cursor = lines[0]["cursor"].as_str().unwrap();
    assert_eq!(
        tags(&events(&proj, &["--since", cursor])),
        ["aaaa0001/late"]
    );
}

/// A client drops mid-stream while one run is being appended to steadily and new runs
/// keep appearing. Resuming from the last cursor it saw must hand it exactly the rest.
#[test]
fn resume_from_cursor_has_no_loss_or_duplicates_under_concurrent_appends() {
    let (_tmp, proj) = project();
    append_raw(&proj, "aaaa0001", &line(1, "a0"));
    let follower = Follower::start(&proj, &["--since", "1"]);
    assert_eq!(follower.next()["message"], "a0");

    let writer_proj = proj.clone();
    let writer = std::thread::spawn(move || {
        let mut written = vec!["aaaa0001/a0".to_string()];
        for i in 0..60u32 {
            // Timestamps run backwards across runs on purpose.
            let msg = format!("b{i}");
            append_raw(&writer_proj, "bbbb0002", &line(59 - i, &msg));
            written.push(format!("bbbb0002/{msg}"));
            if i % 15 == 0 {
                let run = format!("new{i:05}");
                append_raw(&writer_proj, &run, &line(i, "n"));
                written.push(format!("{run}/n"));
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        written
    });

    let mut seen = vec!["aaaa0001/a0".to_string()];
    let mut cursor = String::new();
    for _ in 0..20 {
        let l = follower.next();
        seen.extend(tags(std::slice::from_ref(&l)));
        cursor = l["cursor"].as_str().unwrap().to_string();
    }
    drop(follower);
    let mut written = writer.join().unwrap();

    seen.extend(tags(&events(&proj, &["--since", &cursor])));
    let mut deduped = seen.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), seen.len(), "no duplicates");
    written.sort();
    assert_eq!(deduped, written, "no loss");
}
