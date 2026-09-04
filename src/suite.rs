//! Built-in suite channel: spar runs `[suite].command` itself and derives the gate from
//! exit codes, with no model between the suite and the verdict (O54).
//!
//! The agent `tester` slot exists to *discover* how a repo runs its tests. Once a project
//! has declared that, everything the slot did afterwards was mechanical — run it, tail the
//! log, format a report — and handing that to a model only buys ways for the verdict to be
//! wrong. This module is that path.

use crate::state::SuiteOutcome;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Lines of a failing command's own output carried into `suite.md`.
const EXCERPT_LINES: usize = 80;
/// Longest single excerpt line kept. A minified bundle or a base64 blob on one line
/// would otherwise be the whole excerpt.
const EXCERPT_LINE_CHARS: usize = 500;
/// Tail window read back per failing command.
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Exited(i32),
    /// Killed at the shared budget, or on the orchestrator's shutdown.
    TimedOut,
    /// Never started: an earlier command spent the budget, or the spawn failed.
    NotRun(String),
    /// `--dry-run`. Reported green with nothing executed.
    DryRun,
}

impl RunStatus {
    fn ok(&self) -> bool {
        matches!(self, RunStatus::Exited(0) | RunStatus::DryRun)
    }

    fn text(&self) -> String {
        match self {
            RunStatus::Exited(c) => format!("exit {c}"),
            RunStatus::TimedOut => "timed out".into(),
            RunStatus::NotRun(why) => format!("not run ({why})"),
            RunStatus::DryRun => "exit 0 (dry-run, not executed)".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandRun {
    pub command: String,
    pub status: RunStatus,
    pub excerpt: String,
}

#[derive(Debug, Clone)]
pub struct BuiltinReport {
    pub outcome: SuiteOutcome,
    /// `suite.md` content, in the same shape the `tester` template asks an agent for.
    pub body: String,
    pub runs: Vec<CommandRun>,
}

/// Run every configured command in `cwd`, in order, under one shared `budget`.
///
/// Every command runs even after one fails. A round is the expensive unit here, so a
/// gate that stops at the first red hands back one failure per round when it could have
/// handed back all of them.
pub fn run(cwd: &Path, commands: &[String], log_path: &Path, budget: Duration) -> BuiltinReport {
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(log_path, b"");

    let deadline = Instant::now() + budget;
    let mut runs = Vec::with_capacity(commands.len());
    for command in commands {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            runs.push(CommandRun {
                command: command.clone(),
                status: RunStatus::NotRun("suite budget already spent".into()),
                excerpt: String::new(),
            });
            continue;
        }
        runs.push(run_one(cwd, command, log_path, remaining));
    }
    report(runs)
}

/// Dry-run stand-in. `--dry-run` is spar's test backend, so it must never execute a
/// project's real suite; the report shape is identical so nothing downstream forks.
pub fn dry(commands: &[String]) -> BuiltinReport {
    report(
        commands
            .iter()
            .map(|c| CommandRun {
                command: c.clone(),
                status: RunStatus::DryRun,
                excerpt: String::new(),
            })
            .collect(),
    )
}

fn report(runs: Vec<CommandRun>) -> BuiltinReport {
    let outcome = derive(&runs);
    let body = render(&runs, outcome);
    BuiltinReport {
        outcome,
        body,
        runs,
    }
}

/// A definite non-zero exit beats an incomplete one. Both block the ship, but "this test
/// failed" is evidence the next round can act on, while `Inconclusive` tells reviewers the
/// suite never ran — a claim that would be false the moment one command really did fail.
fn derive(runs: &[CommandRun]) -> SuiteOutcome {
    if runs.is_empty() {
        return SuiteOutcome::Inconclusive;
    }
    if runs
        .iter()
        .any(|r| matches!(r.status, RunStatus::Exited(c) if c != 0))
    {
        return SuiteOutcome::Fail;
    }
    if runs.iter().any(|r| !r.status.ok()) {
        return SuiteOutcome::Inconclusive;
    }
    SuiteOutcome::Pass
}

fn run_one(cwd: &Path, command: &str, log_path: &Path, timeout: Duration) -> CommandRun {
    let start = append_header(log_path, command);
    let not_run = |why: String| CommandRun {
        command: command.into(),
        status: RunStatus::NotRun(why),
        excerpt: String::new(),
    };

    let log = match std::fs::OpenOptions::new().append(true).open(log_path) {
        Ok(f) => f,
        Err(e) => return not_run(format!("suite log unwritable: {e}")),
    };
    let err_log = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return not_run(format!("suite log unwritable: {e}")),
    };

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));
    // Own process group: a suite spawns nested test binaries, and the budget has to be
    // able to reap those too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return not_run(format!("spawn failed: {e}")),
    };
    let pid = child.id();
    #[cfg(unix)]
    crate::process::shutdown::track(pid);

    let began = Instant::now();
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if began.elapsed() >= timeout || crate::process::shutdown_requested() {
                    killed = true;
                    crate::process::terminate_tree(pid, true);
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break None,
        }
    };
    #[cfg(unix)]
    crate::process::shutdown::untrack(pid);

    let status = match status.and_then(|s| s.code()).filter(|_| !killed) {
        Some(code) => RunStatus::Exited(code),
        None => RunStatus::TimedOut,
    };
    let excerpt = if status.ok() {
        String::new()
    } else {
        tail_from(log_path, start)
    };
    CommandRun {
        command: command.into(),
        status,
        excerpt,
    }
}

/// Append a banner and return the byte offset the command's own output starts at.
fn append_header(log_path: &Path, command: &str) -> u64 {
    let mut f = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let _ = writeln!(f, "\n===== $ {command} =====");
    let _ = f.flush();
    f.stream_position().unwrap_or(0)
}

/// Last `EXCERPT_LINES` lines of the log from `start` on, read through a bounded window.
fn tail_from(log_path: &Path, start: u64) -> String {
    let Ok(mut f) = std::fs::File::open(log_path) else {
        return String::new();
    };
    let end = f.metadata().map(|m| m.len()).unwrap_or(0);
    if end <= start {
        return String::new();
    }
    let from = start.max(end.saturating_sub(TAIL_BYTES));
    if f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.take(TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(EXCERPT_LINES)..]
        .iter()
        .map(|l| truncate(l, EXCERPT_LINE_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let head: String = line.chars().take(max).collect();
    format!("{head} …[truncated]")
}

/// `suite.md`, in the same shape `templates/tester.md` asks an agent for, so every
/// downstream reader (reviewer prompt, coverage check, operator) is unchanged.
fn render(runs: &[CommandRun], outcome: SuiteOutcome) -> String {
    let result = match outcome {
        SuiteOutcome::Pass => "pass",
        SuiteOutcome::Fail => "fail",
        SuiteOutcome::Inconclusive => "inconclusive",
    };
    let mut s = format!("## Result\n{result}\n\n## Commands\n");
    if runs.is_empty() {
        s.push_str("- (none configured)\n");
    }
    for r in runs {
        s.push_str(&format!("- `{}` → {}\n", r.command, r.status.text()));
    }

    let failed: Vec<&CommandRun> = runs.iter().filter(|r| !r.status.ok()).collect();
    s.push_str(&format!(
        "\n## Summary\nspar ran the {} command(s) in `[suite].command` itself; the verdict is their exit codes, not a model's reading of them. ",
        runs.len()
    ));
    match outcome {
        SuiteOutcome::Pass => s.push_str("All exited 0.\n"),
        SuiteOutcome::Fail => s.push_str(&format!("{} did not exit 0.\n", failed.len())),
        SuiteOutcome::Inconclusive => s.push_str(
            "The suite did not reach a clean verdict inside `[suite].timeout_secs`; treat it as unknown, not as a code failure.\n",
        ),
    }

    s.push_str("\n## Failures\n");
    if failed.is_empty() {
        s.push_str("none\n");
        return s;
    }
    for r in failed {
        s.push_str(&format!("\n### `{}` → {}\n", r.command, r.status.text()));
        if r.excerpt.is_empty() {
            s.push_str("(no output captured)\n");
        } else {
            s.push_str(&format!("```\n{}\n```\n", r.excerpt));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("spar-suite-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn all_green_is_pass() {
        let d = tmp("green");
        let rep = run(
            &d,
            &["true".into(), "echo hi".into()],
            &d.join("suite.log"),
            Duration::from_secs(30),
        );
        assert_eq!(rep.outcome, SuiteOutcome::Pass);
        assert!(rep.body.contains("## Failures\nnone"), "{}", rep.body);
    }

    #[test]
    fn nonzero_exit_is_fail_and_carries_its_own_output() {
        let d = tmp("red");
        let rep = run(
            &d,
            &["echo first-ok".into(), "echo boom-marker; exit 3".into()],
            &d.join("suite.log"),
            Duration::from_secs(30),
        );
        assert_eq!(rep.outcome, SuiteOutcome::Fail);
        assert!(rep.body.contains("→ exit 3"), "{}", rep.body);
        assert!(rep.body.contains("boom-marker"), "{}", rep.body);
        // The excerpt is scoped to the failing command's own slice of the log, not the
        // whole log: a green command's output never lands in a failure report.
        let excerpt = &rep.runs[1].excerpt;
        assert!(excerpt.contains("boom-marker"), "{excerpt}");
        assert!(!excerpt.contains("first-ok"), "{excerpt}");
    }

    /// Every command runs even after one fails: a round costs far more than a second
    /// command, so the fix round should see all of the red at once.
    #[test]
    fn a_failure_does_not_stop_the_rest() {
        let d = tmp("continue");
        let rep = run(
            &d,
            &["exit 1".into(), "exit 2".into()],
            &d.join("suite.log"),
            Duration::from_secs(30),
        );
        assert_eq!(rep.runs.len(), 2);
        assert_eq!(rep.runs[1].status, RunStatus::Exited(2));
    }

    #[test]
    fn timeout_is_inconclusive_not_fail() {
        let d = tmp("slow");
        let rep = run(
            &d,
            &["sleep 30".into()],
            &d.join("suite.log"),
            Duration::from_millis(300),
        );
        assert_eq!(rep.outcome, SuiteOutcome::Inconclusive);
        assert_eq!(rep.runs[0].status, RunStatus::TimedOut);
    }

    /// A real failure outranks an incomplete command: the fix round gets actionable
    /// evidence instead of "the runner fell over".
    #[test]
    fn definite_failure_outranks_incomplete() {
        let runs = vec![
            CommandRun {
                command: "a".into(),
                status: RunStatus::Exited(1),
                excerpt: String::new(),
            },
            CommandRun {
                command: "b".into(),
                status: RunStatus::TimedOut,
                excerpt: String::new(),
            },
        ];
        assert_eq!(derive(&runs), SuiteOutcome::Fail);
    }

    #[test]
    fn budget_exhausted_marks_later_commands_not_run() {
        let d = tmp("budget");
        let rep = run(
            &d,
            &["sleep 30".into(), "true".into()],
            &d.join("suite.log"),
            Duration::from_millis(300),
        );
        assert!(matches!(rep.runs[1].status, RunStatus::NotRun(_)));
        assert!(rep.body.contains("not run (suite budget already spent)"));
        assert_eq!(rep.outcome, SuiteOutcome::Inconclusive);
    }

    #[test]
    fn dry_run_executes_nothing_and_reports_pass() {
        let rep = dry(&["rm -rf /".into()]);
        assert_eq!(rep.outcome, SuiteOutcome::Pass);
        assert!(rep.body.contains("dry-run, not executed"), "{}", rep.body);
    }

    #[test]
    fn excerpt_lines_are_truncated() {
        let long = "x".repeat(EXCERPT_LINE_CHARS + 50);
        let out = truncate(&long, EXCERPT_LINE_CHARS);
        assert!(out.ends_with("…[truncated]"));
        assert!(out.chars().count() < long.chars().count());
    }
}
