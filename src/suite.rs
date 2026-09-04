//! Built-in suite channel: spar runs `[suite].command` itself and derives the gate from
//! exit codes, with no model between the suite and the verdict (O54).
//!
//! The agent `tester` slot exists to *discover* how a repo runs its tests. Once a project
//! has declared that, everything the slot did afterwards was mechanical — run it, tail the
//! log, format a report — and handing that to a model only buys ways for the verdict to be
//! wrong. This module is that path.

use crate::config::IsolationMode;
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
/// Cap across *all* excerpts in one report. `suite.md` is interpolated whole into every
/// reviewer's prompt every round, and a three-command list all red would otherwise put
/// ~120KB of build output there — the cost O47 exists to keep out of transcripts.
const MAX_TOTAL_EXCERPT_CHARS: usize = 24_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Exited(i32),
    /// Died on a signal. A crash, an abort, the OOM killer: a definite failure of that
    /// command, and reported as one — never as "the suite did not run".
    Signaled(i32),
    /// Killed at the shared budget, or on the orchestrator's shutdown / `spar stop`.
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

    /// A command that ran and definitively did not pass, as opposed to one that never
    /// reached a verdict. Drives `Fail` over `Inconclusive`.
    fn definite_failure(&self) -> bool {
        matches!(self, RunStatus::Exited(c) if *c != 0) || matches!(self, RunStatus::Signaled(_))
    }

    fn text(&self) -> String {
        match self {
            RunStatus::Exited(c) => format!("exit {c}"),
            RunStatus::Signaled(s) => format!("killed by signal {s}"),
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

pub struct Options<'a> {
    pub cwd: &'a Path,
    pub commands: &'a [String],
    pub log_path: &'a Path,
    /// Wall clock for the whole list, already the hard ceiling.
    pub budget: Duration,
    /// The run's isolation. The commands compile and execute code a model just wrote, so
    /// they get the same confinement every other spawn in the tree gets.
    pub isolation: IsolationMode,
    /// Live pid on spawn, `None` on exit. The orchestrator owns this child but no slot
    /// does, so the caller keeps the reap marker that `live_slot_pids` reads (O28).
    pub on_pid: &'a dyn Fn(Option<u32>),
    /// Polled while waiting: `spar stop` must be able to interrupt a two-hour suite.
    pub stop: &'a dyn Fn() -> bool,
}

/// Run every configured command in `cwd`, in order, under one shared budget.
///
/// Every command runs even after one fails. A round is the expensive unit here, so a
/// gate that stops at the first red hands back one failure per round when it could have
/// handed back all of them.
pub fn run(opts: &Options) -> BuiltinReport {
    if let Some(parent) = opts.log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(opts.log_path, b"");

    // `checked_add`: `timeout_secs` is operator input, and a huge one panics `Instant`
    // addition, taking the orchestrator down mid-round.
    let deadline = Instant::now().checked_add(opts.budget);
    let mut runs = Vec::with_capacity(opts.commands.len());
    for command in opts.commands {
        let remaining = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => Duration::MAX,
        };
        if remaining.is_zero() || (opts.stop)() {
            let why = if remaining.is_zero() {
                "suite budget already spent"
            } else {
                "run stopped"
            };
            runs.push(CommandRun {
                command: command.clone(),
                status: RunStatus::NotRun(why.into()),
                excerpt: String::new(),
            });
            continue;
        }
        runs.push(run_one(opts, command, remaining));
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

/// A definite failure beats an incomplete command. Both block the ship, but `Inconclusive`
/// tells reviewers the suite never ran — a claim that would be false the moment one
/// command really did fail — while `Fail` is evidence the next round can act on.
fn derive(runs: &[CommandRun]) -> SuiteOutcome {
    if runs.is_empty() {
        return SuiteOutcome::Inconclusive;
    }
    if runs.iter().any(|r| r.status.definite_failure()) {
        return SuiteOutcome::Fail;
    }
    if runs.iter().any(|r| !r.status.ok()) {
        return SuiteOutcome::Inconclusive;
    }
    SuiteOutcome::Pass
}

/// `pipefail` wherever the shell has it. `cargo test 2>&1 | tail -200` exits with
/// `tail`'s status, so without it a red suite behind a pipe is a green gate — and the
/// implementer template trains agents to write exactly that pipeline. dash has no
/// `pipefail`, and passing it there fails every command, so bash is preferred and plain
/// `sh` is the fallback rather than the default.
fn shell() -> (&'static str, &'static [&'static str]) {
    use std::sync::OnceLock;
    static PIPEFAIL: OnceLock<bool> = OnceLock::new();
    // Probed, not assumed: a `bash` on PATH that is a busybox applet or a restricted
    // build rejects `-o pipefail` and would fail *every* command, wedging the run at a
    // gate no round can clear.
    let ok = *PIPEFAIL.get_or_init(|| {
        Command::new("bash")
            .args(["-o", "pipefail", "-c", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    if ok {
        ("bash", &["-o", "pipefail", "-c"])
    } else {
        ("sh", &["-c"])
    }
}

/// What actually gets spawned for one command: the shell, plus the run's isolation.
///
/// Split out because it is the only place the sandbox is applied, and a silent
/// regression here is a `worktree+bwrap` run compiling model-authored code unconfined.
fn spawn_argv(
    isolation: IsolationMode,
    cwd: &Path,
    command: &str,
) -> (std::path::PathBuf, Vec<String>) {
    let (program, flags) = shell();
    let mut argv: Vec<String> = flags.iter().map(|f| (*f).to_string()).collect();
    argv.push(command.to_string());
    crate::sandbox::maybe_wrap(isolation, cwd, &std::path::PathBuf::from(program), &argv)
}

fn run_one(opts: &Options, command: &str, timeout: Duration) -> CommandRun {
    let start = append_header(opts.log_path, command);
    let not_run = |why: String| CommandRun {
        command: command.into(),
        status: RunStatus::NotRun(why),
        excerpt: String::new(),
    };

    let log = match std::fs::OpenOptions::new().append(true).open(opts.log_path) {
        Ok(f) => f,
        Err(e) => return not_run(format!("suite log unwritable: {e}")),
    };
    let err_log = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return not_run(format!("suite log unwritable: {e}")),
    };

    let (program, argv) = spawn_argv(opts.isolation, opts.cwd, command);
    let mut cmd = Command::new(&program);
    cmd.args(&argv)
        .current_dir(opts.cwd)
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
    (opts.on_pid)(Some(pid));
    #[cfg(unix)]
    crate::process::shutdown::track(pid);

    let began = Instant::now();
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if began.elapsed() >= timeout
                    || crate::process::shutdown_requested()
                    || (opts.stop)()
                {
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
    (opts.on_pid)(None);

    let status = classify(status, killed);
    // Bound the excerpt at the log's length *now*. Reading to EOF instead would pick up
    // whatever a descendant of this command kept writing after it exited, which is output
    // the report would attribute to a command that had already finished.
    let end = std::fs::metadata(opts.log_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let excerpt = if status.ok() {
        String::new()
    } else {
        tail_range(opts.log_path, start, end)
    };
    CommandRun {
        command: command.into(),
        status,
        excerpt,
    }
}

/// A budget kill is `TimedOut`; a signal we did not send is the command dying, which is a
/// failure and not an unknown. Without the split, `cargo test` OOM-killed in five seconds
/// reports "the suite did not reach a clean verdict inside `[suite].timeout_secs`".
fn classify(status: Option<std::process::ExitStatus>, killed: bool) -> RunStatus {
    if killed {
        return RunStatus::TimedOut;
    }
    let Some(status) = status else {
        return RunStatus::TimedOut;
    };
    if let Some(code) = status.code() {
        return RunStatus::Exited(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return RunStatus::Signaled(sig);
        }
    }
    RunStatus::TimedOut
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

/// Last `EXCERPT_LINES` lines of `[start, end)`, read through a bounded window.
fn tail_range(log_path: &Path, start: u64, end: u64) -> String {
    let Ok(mut f) = std::fs::File::open(log_path) else {
        return String::new();
    };
    if end <= start {
        return String::new();
    }
    let from = start.max(end.saturating_sub(TAIL_BYTES));
    if f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    // `read_to_end` over a bounded `take`, not a single `read`: a bare `read` neither
    // retries on EINTR (spar installs signal handlers) nor loops on a short read, and
    // either one silently empties the excerpt a reviewer is meant to diagnose from.
    let mut buf = Vec::new();
    if f.take(end - from).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // The window can start mid-line (and mid-UTF-8 sequence), so the first line is a
    // fragment rather than evidence.
    if from > start && lines.len() > 1 {
        lines.remove(0);
    }
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

    let dry = runs.iter().all(|r| r.status == RunStatus::DryRun) && !runs.is_empty();
    let failed: Vec<&CommandRun> = runs.iter().filter(|r| !r.status.ok()).collect();
    s.push_str("\n## Summary\n");
    if dry {
        // Never claim a suite ran. This body goes verbatim into reviewer prompts, and a
        // report that says "all exited 0" about commands nobody executed is the exact
        // class of false evidence O54 exists to remove.
        s.push_str(&format!(
            "`--dry-run`: spar executed none of the {} command(s) in `[suite].command`. This is a stub, not a suite result.\n",
            runs.len()
        ));
        s.push_str("\n## Failures\nnone\n");
        return s;
    }
    s.push_str(&format!(
        "spar ran the {} command(s) in `[suite].command` itself; the verdict is their exit codes, not a model's reading of them. ",
        runs.len()
    ));
    match outcome {
        SuiteOutcome::Pass => s.push_str("All exited 0.\n"),
        SuiteOutcome::Fail => s.push_str(&format!("{} did not exit 0.\n", failed.len())),
        SuiteOutcome::Inconclusive => {
            // Name the actual cause. "Ran out of wall clock" over a run the operator
            // stopped, or a command that could not spawn, sends the next round chasing a
            // timeout that never happened.
            let why = runs
                .iter()
                .find(|r| !r.status.ok())
                .map(|r| r.status.text())
                .unwrap_or_else(|| "no commands configured".into());
            s.push_str(&format!(
                "The suite did not reach a clean verdict ({why}); treat it as unknown, not as a code failure.\n"
            ));
        }
    }

    s.push_str("\n## Failures\n");
    if failed.is_empty() {
        s.push_str("none\n");
        return s;
    }
    let mut budget = MAX_TOTAL_EXCERPT_CHARS;
    for r in failed {
        s.push_str(&format!("\n### `{}` → {}\n", r.command, r.status.text()));
        if r.excerpt.is_empty() {
            s.push_str("(no output captured)\n");
            continue;
        }
        let (excerpt, clipped) = clip(&r.excerpt, budget);
        budget = budget.saturating_sub(excerpt.chars().count());
        s.push_str(&format!("```\n{excerpt}\n```\n"));
        if clipped {
            s.push_str(&format!(
                "_Excerpt clipped: this report's excerpts are capped at {MAX_TOTAL_EXCERPT_CHARS} characters. Full output: `suite.log`._\n"
            ));
        }
    }
    s
}

fn clip(excerpt: &str, budget: usize) -> (String, bool) {
    if excerpt.chars().count() <= budget {
        return (excerpt.to_string(), false);
    }
    (excerpt.chars().take(budget).collect(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn opts<'a>(
        dir: &'a Path,
        commands: &'a [String],
        log: &'a Path,
        budget: Duration,
    ) -> Options<'a> {
        Options {
            cwd: dir,
            commands,
            log_path: log,
            budget,
            isolation: IsolationMode::Worktree,
            on_pid: &|_| {},
            stop: &|| false,
        }
    }

    fn go(dir: &TempDir, commands: &[String], budget: Duration) -> BuiltinReport {
        let log = dir.path().join("suite.log");
        run(&opts(dir.path(), commands, &log, budget))
    }

    #[test]
    fn all_green_is_pass() {
        let d = TempDir::new().unwrap();
        let rep = go(
            &d,
            &["true".into(), "echo hi".into()],
            Duration::from_secs(30),
        );
        assert_eq!(rep.outcome, SuiteOutcome::Pass);
        assert!(rep.body.contains("## Failures\nnone"), "{}", rep.body);
    }

    #[test]
    fn nonzero_exit_is_fail_and_carries_its_own_output() {
        let d = TempDir::new().unwrap();
        let rep = go(
            &d,
            &["echo first-ok".into(), "echo boom-marker; exit 3".into()],
            Duration::from_secs(30),
        );
        assert_eq!(rep.outcome, SuiteOutcome::Fail);
        assert!(rep.body.contains("→ exit 3"), "{}", rep.body);
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
        let d = TempDir::new().unwrap();
        let rep = go(
            &d,
            &["exit 1".into(), "exit 2".into()],
            Duration::from_secs(30),
        );
        assert_eq!(rep.runs.len(), 2);
        assert_eq!(rep.runs[1].status, RunStatus::Exited(2));
    }

    #[test]
    fn timeout_is_inconclusive_not_fail() {
        let d = TempDir::new().unwrap();
        let rep = go(&d, &["sleep 30".into()], Duration::from_millis(300));
        assert_eq!(rep.outcome, SuiteOutcome::Inconclusive);
        assert_eq!(rep.runs[0].status, RunStatus::TimedOut);
    }

    /// A crash is a failure, not an unknown. Before this split, `cargo test` OOM-killed
    /// in five seconds reported "the suite did not reach a clean verdict inside its wall
    /// clock" — a false statement in the artifact reviewers read.
    #[test]
    #[cfg(unix)]
    fn a_signal_death_is_a_failure_not_a_timeout() {
        let d = TempDir::new().unwrap();
        let rep = go(
            &d,
            &["exec sh -c 'kill -SEGV $$'".into()],
            Duration::from_secs(30),
        );
        assert_eq!(
            rep.runs[0].status,
            RunStatus::Signaled(11),
            "{:?}",
            rep.runs
        );
        assert_eq!(rep.outcome, SuiteOutcome::Fail);
        assert!(rep.body.contains("killed by signal 11"), "{}", rep.body);
    }

    /// The nested-binary case `process_group(0)` + `terminate_tree(pid, true)` exist for:
    /// a bare child kill would leave the grandchild running and holding the log fd.
    #[test]
    #[cfg(unix)]
    fn the_budget_reaps_nested_children() {
        let d = TempDir::new().unwrap();
        let marker = d.path().join("grandchild.pid");
        let cmd = format!("sh -c 'echo $$ > {}; sleep 60' & wait", marker.display());
        let rep = go(&d, &[cmd], Duration::from_millis(500));
        assert_eq!(rep.runs[0].status, RunStatus::TimedOut);
        let pid: u32 = std::fs::read_to_string(&marker)
            .expect("grandchild pid")
            .trim()
            .parse()
            .unwrap();
        // terminate_tree SIGKILLs the group; give the kernel a moment to reap.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !crate::process::pid_alive(pid),
            "nested child {pid} survived the budget kill"
        );
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
        let d = TempDir::new().unwrap();
        let rep = go(
            &d,
            &["sleep 30".into(), "true".into()],
            Duration::from_millis(300),
        );
        assert!(matches!(rep.runs[1].status, RunStatus::NotRun(_)));
        assert!(rep.body.contains("not run (suite budget already spent)"));
        assert_eq!(rep.outcome, SuiteOutcome::Inconclusive);
    }

    /// `spar stop` must be able to interrupt a two-hour suite, not just SIGINT: both the
    /// command already running and the ones not started yet.
    #[test]
    fn a_stop_request_interrupts_the_run() {
        let d = TempDir::new().unwrap();
        let log = d.path().join("suite.log");
        let cmds = vec!["sleep 30".to_string(), "true".to_string()];
        // False once so the first command starts, then true: the stop lands mid-command.
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let stop = || polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
        let mut o = opts(d.path(), &cmds, &log, Duration::from_secs(60));
        o.stop = &stop;
        let rep = run(&o);
        assert_eq!(rep.runs[0].status, RunStatus::TimedOut, "{:?}", rep.runs);
        assert!(matches!(&rep.runs[1].status, RunStatus::NotRun(w) if w == "run stopped"));
        assert_eq!(rep.outcome, SuiteOutcome::Inconclusive);
    }

    /// The caller keeps the reap marker for this child, so it has to see both edges.
    #[test]
    fn the_live_pid_is_reported_on_both_edges() {
        let d = TempDir::new().unwrap();
        let log = d.path().join("suite.log");
        let cmds = vec!["true".to_string()];
        let seen = std::sync::Mutex::new(Vec::new());
        let record = |p: Option<u32>| seen.lock().unwrap().push(p);
        let mut o = opts(d.path(), &cmds, &log, Duration::from_secs(30));
        o.on_pid = &record;
        run(&o);
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].is_some());
        assert!(seen[1].is_none());
    }

    /// A red suite behind `| tail` must not report the pipe's exit code. Only meaningful
    /// where bash exists; `sh` has no `pipefail` and the fallback is documented.
    #[test]
    fn a_failure_behind_a_pipe_is_still_a_failure() {
        if which::which("bash").is_err() {
            return;
        }
        let d = TempDir::new().unwrap();
        let rep = go(&d, &["exit 7 | tail -1".into()], Duration::from_secs(30));
        assert_eq!(rep.outcome, SuiteOutcome::Fail, "{}", rep.body);
    }

    #[test]
    fn dry_run_executes_nothing() {
        let d = TempDir::new().unwrap();
        let canary = d.path().join("EXECUTED");
        let rep = dry(&[format!("touch {}", canary.display())]);
        assert!(!canary.exists(), "dry() executed its command list");
        assert_eq!(rep.outcome, SuiteOutcome::Pass);
        // And it must not claim a suite ran: this body reaches reviewer prompts verbatim.
        assert!(rep.body.contains("executed none of"), "{}", rep.body);
        assert!(!rep.body.contains("All exited 0"), "{}", rep.body);
    }

    #[test]
    fn an_absurd_budget_does_not_panic() {
        let d = TempDir::new().unwrap();
        let rep = go(&d, &["true".into()], Duration::from_secs(u64::MAX));
        assert_eq!(rep.outcome, SuiteOutcome::Pass);
    }

    #[test]
    fn excerpts_are_capped_per_line_and_in_total() {
        let d = TempDir::new().unwrap();
        let noisy = format!(
            "for i in $(seq 1 400); do printf 'line-%s-{}\\n' \"$i\"; done; exit 1",
            "x".repeat(600)
        );
        let rep = go(&d, &[noisy.clone(), noisy], Duration::from_secs(60));
        assert_eq!(rep.outcome, SuiteOutcome::Fail);
        assert_eq!(
            rep.runs[0].excerpt.lines().count(),
            EXCERPT_LINES,
            "per-command line cap"
        );
        assert!(rep.runs[0]
            .excerpt
            .lines()
            .all(|l| l.chars().count() <= EXCERPT_LINE_CHARS + 20));
        let fences: usize = rep.body.matches("```").count();
        assert_eq!(fences, 4, "both failures rendered");
        assert!(
            rep.body.contains("Excerpt clipped"),
            "total cap must announce itself"
        );
    }

    /// The sandbox is the one thing here with no visible failure mode: drop the
    /// `maybe_wrap` call and every other test still passes while a `worktree+bwrap` run
    /// compiles model-authored code with the operator's full ambient privileges.
    #[test]
    fn isolation_is_applied_to_the_spawn() {
        let d = TempDir::new().unwrap();
        let (plain, args) = spawn_argv(IsolationMode::Worktree, d.path(), "cargo test");
        let plain = plain.to_string_lossy().to_string();
        assert!(plain.ends_with("bash") || plain.ends_with("sh"), "{plain}");
        assert_eq!(args.last().unwrap(), "cargo test");

        if !crate::sandbox::bwrap_available() {
            return;
        }
        let (wrapped, args) = spawn_argv(IsolationMode::WorktreeBwrap, d.path(), "cargo test");
        assert!(
            wrapped.to_string_lossy().ends_with("bwrap"),
            "{}",
            wrapped.display()
        );
        assert!(args.iter().any(|a| a == "--ro-bind"), "{args:?}");
        assert_eq!(args.last().unwrap(), "cargo test");
    }

    /// An inconclusive report has to name its own cause: "ran out of wall clock" over a
    /// run the operator stopped points the next round at a timeout that never happened.
    #[test]
    fn an_inconclusive_summary_names_the_real_cause() {
        let d = TempDir::new().unwrap();
        let log = d.path().join("suite.log");
        let cmds = vec!["sleep 30".to_string()];
        let stop = || true;
        let mut o = opts(d.path(), &cmds, &log, Duration::from_secs(60));
        o.stop = &stop;
        let rep = run(&o);
        assert_eq!(rep.outcome, SuiteOutcome::Inconclusive);
        assert!(rep.body.contains("run stopped"), "{}", rep.body);
    }

    #[test]
    fn excerpt_lines_are_truncated() {
        let long = "x".repeat(EXCERPT_LINE_CHARS + 50);
        let out = truncate(&long, EXCERPT_LINE_CHARS);
        assert!(out.ends_with("…[truncated]"));
        assert!(out.chars().count() < long.chars().count());
    }
}
