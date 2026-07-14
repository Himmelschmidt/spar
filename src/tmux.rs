use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// spar owns a dedicated tmux server socket so it never touches the user's default
/// socket or their personal sessions. Every tmux invocation goes through [`tmux`].
const SOCKET: &str = "spar";

/// A `tmux` command pinned to spar's private socket (`-L spar`). The socket flag
/// must precede the tmux subcommand, so this is the only place `Command::new("tmux")`
/// is constructed.
fn tmux() -> Command {
    let mut cmd = Command::new("tmux");
    cmd.args(["-L", SOCKET]);
    cmd
}

pub fn available() -> bool {
    which::which("tmux").is_ok()
}

pub fn session_name(run_id: &str) -> String {
    format!("spar-{run_id}")
}

pub fn has_session(name: &str) -> bool {
    tmux()
        .args(["has-session", "-t", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Deterministic per-project session name for the embedded workspace shell.
///
/// Hashing the canonicalized root keeps the name stable across TUI runs —
/// `DefaultHasher::new()` is seeded to a fixed constant (unlike `HashMap`'s
/// randomized `RandomState`), so the same root always yields the same name while
/// distinct roots map to distinct names.
pub fn workspace_shell_session(project_root: &Path) -> String {
    let canon = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canon.hash(&mut hasher);
    format!("spar-shell-{:x}", hasher.finish())
}

/// Ensure the project's workspace shell exists (idempotent), returning its session
/// name. The default window runs the user's `$SHELL` rooted at `project_root`.
///
/// This session is intentionally persistent: it is detached and OUTLIVES the TUI,
/// so a dev server (`vite`, `cargo run`, …) started in it keeps running across TUI
/// restarts and disconnects. It is never killed on exit.
pub fn ensure_workspace_shell(project_root: &Path) -> Result<String> {
    let name = workspace_shell_session(project_root);
    new_session(&name, project_root)?;
    Ok(name)
}

/// All session names on the spar socket, one per line. Empty when no server is
/// running or the query fails.
pub fn list_sessions() -> Vec<String> {
    let out = tmux()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match out {
        Ok(o) if o.status.success() => parse_session_list(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// Parse `list-sessions` stdout into trimmed, non-empty names. Pure, for testing.
fn parse_session_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Create a detached session with a shell in `cwd`.
pub fn new_session(name: &str, cwd: &Path) -> Result<()> {
    if has_session(name) {
        return Ok(());
    }
    let status = tmux()
        .args([
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            cwd.to_str().unwrap_or("."),
        ])
        .status()
        .context("tmux new-session")?;
    if !status.success() {
        bail!("tmux new-session failed for {name}");
    }
    Ok(())
}

/// Create a window for a slot and run `command` inside it. `env` entries are set on
/// the new pane (`-e KEY=VAL`), so a spawned agent and any hooks it runs inherit
/// its `SPAR_AGENT_ID` / `SPAR_RUN_ID` / `SPAR_PROJECT_ROOT` identity.
pub fn spawn_window(
    session: &str,
    window: &str,
    cwd: &Path,
    shell_cmd: &str,
    env: &[(String, String)],
) -> Result<()> {
    let mut cmd = tmux();
    cmd.args([
        "new-window",
        "-t",
        session,
        "-n",
        window,
        "-c",
        cwd.to_str().unwrap_or("."),
    ]);
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(shell_cmd);
    let status = cmd.status().context("tmux new-window")?;
    if !status.success() {
        bail!("tmux new-window failed: {window}");
    }
    Ok(())
}

#[allow(dead_code)]
pub fn send_keys(session: &str, window: &str, keys: &str) -> Result<()> {
    let target = format!("{session}:{window}");
    let status = tmux()
        .args(["send-keys", "-t", &target, keys, "Enter"])
        .status()
        .context("tmux send-keys")?;
    if !status.success() {
        bail!("tmux send-keys failed for {target}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Key forwarding (Stage 10)
//
// When the TUI's Terminal panel is focused, crossterm key events are translated
// into `tmux send-keys` invocations against the focused pane on the spar socket.
// The translation is a pure function so it can be unit-tested independently of a
// live tmux server.
// ---------------------------------------------------------------------------

/// A crossterm key translated for `tmux send-keys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendKey {
    /// Printable text sent verbatim with `send-keys -l` (literal), so a glyph is
    /// never mistaken for a tmux key name (e.g. the letters in "Enter").
    Literal(String),
    /// A named tmux key or modifier combo sent by name — `Enter`, `Tab`, `BSpace`,
    /// `Up`, `C-c`, `M-b`, `F5`, … These carry semantics (Enter submits a prompt),
    /// which is why they must stay distinct from [`SendKey::Literal`].
    Named(String),
}

impl SendKey {
    /// The `send-keys` arguments that follow the `-t <target>`, e.g. `["-l", "--", "a"]`
    /// or `["Enter"]`. Pure; used both for spawning and for tests.
    pub fn args(&self) -> Vec<String> {
        match self {
            // `--` terminates option parsing so a literal starting with `-`
            // (e.g. a prompt like `-x fix this`) is typed verbatim, not read as flags.
            SendKey::Literal(s) => vec!["-l".to_string(), "--".to_string(), s.clone()],
            SendKey::Named(k) => vec![k.clone()],
        }
    }
}

/// Translate a crossterm key event into a single `tmux send-keys` payload, or
/// `None` for keys we don't forward.
///
/// The write-text-then-separate-Enter convention (a prompt's text is typed, then
/// Enter is a distinct submit) falls out naturally: each printable key maps to a
/// [`SendKey::Literal`] and Enter maps to its own [`SendKey::Named`] `Enter`, so a
/// per-keystroke forwarder never fuses text and the submit into one send.
pub fn map_key(code: KeyCode, mods: KeyModifiers) -> Option<SendKey> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let named = |k: &str| Some(SendKey::Named(k.to_string()));
    match code {
        KeyCode::Char(c) => {
            if ctrl || alt {
                // tmux spells modifiers `M-`/`C-`; Ctrl folds case (C-c == C-C).
                let base = if ctrl { c.to_ascii_lowercase() } else { c };
                let mut name = String::new();
                if alt {
                    name.push_str("M-");
                }
                if ctrl {
                    name.push_str("C-");
                }
                name.push(base);
                Some(SendKey::Named(name))
            } else {
                Some(SendKey::Literal(c.to_string()))
            }
        }
        KeyCode::Enter => named("Enter"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => named("BTab"),
        KeyCode::Backspace => named("BSpace"),
        KeyCode::Esc => named("Escape"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::Delete => named("DC"),
        KeyCode::Insert => named("IC"),
        KeyCode::F(n) => Some(SendKey::Named(format!("F{n}"))),
        _ => None,
    }
}

/// Send one translated key to `target` (a pane id `%N` or a session name) on the
/// spar socket via `tmux -L spar send-keys`. Human keystroke rate makes the per-key
/// spawn negligible, and targeting the exact pane keeps it off any shared socket.
pub fn send_key(target: &str, key: &SendKey) -> Result<()> {
    let mut cmd = tmux();
    cmd.args(["send-keys", "-t", target]);
    for a in key.args() {
        cmd.arg(a);
    }
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("tmux send-keys")?;
    if !status.success() {
        bail!("tmux send-keys failed for {target}");
    }
    Ok(())
}

#[allow(dead_code)]
pub fn capture_pane(session: &str, window: &str) -> Result<String> {
    let target = format!("{session}:{window}");
    let out = tmux()
        .args(["capture-pane", "-p", "-t", &target, "-S", "-200"])
        .output()
        .context("tmux capture-pane")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pid of the process running in a slot's window (the pane's shell), if the pane exists.
pub fn pane_pid(session: &str, window: &str) -> Option<u32> {
    let target = format!("{session}:{window}");
    let out = tmux()
        .args(["list-panes", "-t", &target, "-F", "#{pane_pid}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

pub fn kill_session(name: &str) -> Result<()> {
    if !has_session(name) {
        return Ok(());
    }
    let _ = tmux().args(["kill-session", "-t", name]).status();
    Ok(())
}

pub fn attach_command(session: &str) -> Result<()> {
    if !has_session(session) {
        bail!("no tmux session {session}");
    }
    let status = tmux()
        .args(["attach-session", "-t", session])
        .status()
        .context("tmux attach")?;
    if !status.success() {
        bail!("tmux attach failed");
    }
    Ok(())
}

/// Build a shell command string that runs `program` with `args` and no logging
/// wrapper. Interactive agent panes need a real tty on stdout for their TUI to
/// render, so the `| tee` of [`shell_wrap`] can't be used for them.
pub fn shell_command(program: &Path, args: &[String]) -> String {
    let prog = shell_escape(&program.display().to_string());
    if args.is_empty() {
        return prog;
    }
    let args_s: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    format!("{prog} {}", args_s.join(" "))
}

/// Build a shell command string that runs program with args and logs.
pub fn shell_wrap(program: &Path, args: &[String], log_path: &Path) -> String {
    let prog = shell_escape(&program.display().to_string());
    let args_s: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    let log = shell_escape(&log_path.display().to_string());
    format!(
        "{prog} {} 2>&1 | tee {log}; echo EXIT:$? >> {log}",
        args_s.join(" ")
    )
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Control-mode client (W2)
//
// `tmux -L spar -C attach -t <session>` runs tmux as a control client: it reads
// commands on stdin and emits an event stream on stdout using the control-mode
// line protocol. We parse that protocol into a typed [`ControlEvent`] stream so
// the TUI can render live pane output without polling `capture-pane`.
// ---------------------------------------------------------------------------

/// A typed event decoded from tmux's control-mode output stream.
///
/// Consumed by the embedded-terminal widget (Stage 9); wired here so the parser
/// and client land and are unit-tested independently.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// `%output %<pane> <data>` — raw (octal-unescaped) bytes written to a pane.
    PaneOutput { pane_id: String, bytes: Vec<u8> },
    /// `%window-add @<window>`
    WindowAdd { window_id: String },
    /// `%window-close @<window>` — the window (and its panes) went away.
    WindowClose { window_id: String },
    /// `%pane-mode-changed %<pane>`
    PaneModeChanged { pane_id: String },
    /// A completed `%begin … %end`/`%error` command-reply block.
    Reply { error: bool, lines: Vec<String> },
    /// `%exit [reason]` — the control client itself is terminating.
    Exit { reason: Option<String> },
    /// Any other `%`-notification we don't model, kept verbatim (without the `%`).
    Other(String),
}

/// Incremental parser for the control-mode line protocol. Feed it one line at a
/// time (newline stripped); it emits at most one [`ControlEvent`] per line,
/// buffering `%begin … %end` blocks until they close.
#[allow(dead_code)]
#[derive(Default)]
pub struct ControlParser {
    block: Option<ReplyBlock>,
}

struct ReplyBlock {
    lines: Vec<String>,
}

#[allow(dead_code)]
impl ControlParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line (without its trailing newline). Returns an event if the line
    /// completed one; block-content lines and `%begin` return `None`.
    pub fn push_line(&mut self, line: &str) -> Option<ControlEvent> {
        // Inside a %begin block, everything is literal command output until the
        // matching %end / %error terminator.
        if let Some(block) = self.block.as_mut() {
            if line == "%end" || line.starts_with("%end ") {
                let block = self.block.take().unwrap();
                return Some(ControlEvent::Reply {
                    error: false,
                    lines: block.lines,
                });
            }
            if line == "%error" || line.starts_with("%error ") {
                let block = self.block.take().unwrap();
                return Some(ControlEvent::Reply {
                    error: true,
                    lines: block.lines,
                });
            }
            block.lines.push(line.to_string());
            return None;
        }

        // Non-notification lines (no leading `%`) outside a block are ignored;
        // they aren't part of the protocol.
        let rest = line.strip_prefix('%')?;

        if let Some(payload) = rest.strip_prefix("output ") {
            return parse_output(payload);
        }
        if let Some(id) = rest.strip_prefix("window-add ") {
            return Some(ControlEvent::WindowAdd {
                window_id: id.trim().to_string(),
            });
        }
        if let Some(id) = rest.strip_prefix("window-close ") {
            return Some(ControlEvent::WindowClose {
                window_id: id.trim().to_string(),
            });
        }
        if let Some(id) = rest.strip_prefix("pane-mode-changed ") {
            return Some(ControlEvent::PaneModeChanged {
                pane_id: id.trim().to_string(),
            });
        }
        if rest == "begin" || rest.starts_with("begin ") {
            self.block = Some(ReplyBlock { lines: Vec::new() });
            return None;
        }
        if rest == "exit" {
            return Some(ControlEvent::Exit { reason: None });
        }
        if let Some(reason) = rest.strip_prefix("exit ") {
            return Some(ControlEvent::Exit {
                reason: Some(reason.trim().to_string()),
            });
        }
        Some(ControlEvent::Other(rest.to_string()))
    }
}

/// Parse the payload after `%output ` — `%<pane-id> <octal-escaped-data>`.
#[allow(dead_code)]
fn parse_output(payload: &str) -> Option<ControlEvent> {
    let (pane_id, data) = payload.split_once(' ')?;
    Some(ControlEvent::PaneOutput {
        pane_id: pane_id.to_string(),
        bytes: unescape_output(data),
    })
}

/// Decode tmux's `%output` escaping: bytes it can't emit literally are written as
/// `\ooo` (a backslash followed by exactly three octal digits). Everything else —
/// including literal UTF-8 — passes through untouched.
#[allow(dead_code)]
fn unescape_output(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && is_octal(b[i + 1])
            && is_octal(b[i + 2])
            && is_octal(b[i + 3])
        {
            let val = ((b[i + 1] - b'0') as u16) * 64
                + ((b[i + 2] - b'0') as u16) * 8
                + (b[i + 3] - b'0') as u16;
            out.push(val as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn is_octal(c: u8) -> bool {
    (b'0'..=b'7').contains(&c)
}

/// A running control-mode client: a `tmux -L spar -C` child plus a background
/// reader thread that decodes its stdout into a [`ControlEvent`] channel.
#[allow(dead_code)]
pub struct ControlClient {
    child: Child,
    stdin: std::process::ChildStdin,
    events: Receiver<ControlEvent>,
}

#[allow(dead_code)]
impl ControlClient {
    /// Attach a control client to an existing session on the spar socket.
    pub fn attach(session: &str) -> Result<Self> {
        Self::spawn(&["attach-session", "-t", session])
    }

    fn spawn(args: &[&str]) -> Result<Self> {
        let mut child = tmux()
            .arg("-C")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn tmux control mode")?;
        let stdin = child.stdin.take().context("control-mode stdin")?;
        let stdout = child.stdout.take().context("control-mode stdout")?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut parser = ControlParser::new();
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if let Some(ev) = parser.push_line(trimmed) {
                            if tx.send(ev).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            events: rx,
        })
    }

    /// Send a tmux command to the control client (newline-terminated on stdin).
    #[allow(dead_code)]
    pub fn send_command(&mut self, cmd: &str) -> Result<()> {
        writeln!(self.stdin, "{cmd}").context("write control-mode command")?;
        self.stdin.flush().context("flush control-mode command")?;
        Ok(())
    }

    /// The decoded event stream. Recv/try_recv as needed.
    #[allow(dead_code)]
    pub fn events(&self) -> &Receiver<ControlEvent> {
        &self.events
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_shell_name_is_deterministic_and_root_specific() {
        let a1 = workspace_shell_session(Path::new("/tmp/does-not-exist/proj-a"));
        let a2 = workspace_shell_session(Path::new("/tmp/does-not-exist/proj-a"));
        let b = workspace_shell_session(Path::new("/tmp/does-not-exist/proj-b"));
        assert_eq!(a1, a2, "same root must yield the same name");
        assert_ne!(a1, b, "distinct roots must yield distinct names");
        assert!(a1.starts_with("spar-shell-"));
    }

    #[test]
    fn parse_session_list_trims_and_drops_blanks() {
        assert_eq!(
            parse_session_list("spar-shell-1\n  spar-abc  \n\nspar-def\n"),
            vec![
                "spar-shell-1".to_string(),
                "spar-abc".to_string(),
                "spar-def".to_string(),
            ]
        );
        assert!(parse_session_list("").is_empty());
        assert!(parse_session_list("\n  \n").is_empty());
    }

    #[test]
    fn unescapes_octal_bytes() {
        // \015\012 == CR LF; literal text passes through.
        assert_eq!(unescape_output("hi\\015\\012"), b"hi\r\n");
    }

    #[test]
    fn unescapes_escaped_backslash() {
        // tmux emits a literal backslash as \134.
        assert_eq!(unescape_output("a\\134b"), b"a\\b");
    }

    #[test]
    fn leaves_lone_backslash_untouched() {
        // A backslash not followed by three octal digits is literal.
        assert_eq!(unescape_output("a\\b"), b"a\\b");
        assert_eq!(unescape_output("tail\\12"), b"tail\\12");
    }

    #[test]
    fn passes_high_bytes_through() {
        // UTF-8 multibyte content is emitted literally by tmux.
        assert_eq!(unescape_output("é"), "é".as_bytes());
    }

    #[test]
    fn parses_output_frame() {
        let mut p = ControlParser::new();
        let ev = p.push_line("%output %3 echo\\040done\\015\\012").unwrap();
        assert_eq!(
            ev,
            ControlEvent::PaneOutput {
                pane_id: "%3".to_string(),
                bytes: b"echo done\r\n".to_vec(),
            }
        );
    }

    #[test]
    fn parses_lifecycle_lines() {
        let mut p = ControlParser::new();
        assert_eq!(
            p.push_line("%window-add @2"),
            Some(ControlEvent::WindowAdd {
                window_id: "@2".to_string()
            })
        );
        assert_eq!(
            p.push_line("%window-close @2"),
            Some(ControlEvent::WindowClose {
                window_id: "@2".to_string()
            })
        );
        assert_eq!(
            p.push_line("%pane-mode-changed %5"),
            Some(ControlEvent::PaneModeChanged {
                pane_id: "%5".to_string()
            })
        );
    }

    #[test]
    fn parses_exit_with_and_without_reason() {
        let mut p = ControlParser::new();
        assert_eq!(
            p.push_line("%exit"),
            Some(ControlEvent::Exit { reason: None })
        );
        assert_eq!(
            p.push_line("%exit server exited"),
            Some(ControlEvent::Exit {
                reason: Some("server exited".to_string())
            })
        );
    }

    #[test]
    fn buffers_begin_end_block() {
        let mut p = ControlParser::new();
        assert_eq!(p.push_line("%begin 1700000000 12 1"), None);
        assert_eq!(p.push_line("session-a"), None);
        assert_eq!(p.push_line("session-b"), None);
        let ev = p.push_line("%end 1700000000 12 1").unwrap();
        assert_eq!(
            ev,
            ControlEvent::Reply {
                error: false,
                lines: vec!["session-a".to_string(), "session-b".to_string()],
            }
        );
    }

    #[test]
    fn error_block_flags_error() {
        let mut p = ControlParser::new();
        assert_eq!(p.push_line("%begin 1 2 1"), None);
        assert_eq!(p.push_line("no such session"), None);
        let ev = p.push_line("%error 1 2 1").unwrap();
        assert_eq!(
            ev,
            ControlEvent::Reply {
                error: true,
                lines: vec!["no such session".to_string()],
            }
        );
    }

    fn map(code: KeyCode, mods: KeyModifiers) -> SendKey {
        super::map_key(code, mods).expect("key should map")
    }

    #[test]
    fn printable_chars_map_to_literal() {
        assert_eq!(
            map(KeyCode::Char('a'), KeyModifiers::NONE),
            SendKey::Literal("a".to_string())
        );
        // A shifted char already arrives upper-cased; still literal.
        assert_eq!(
            map(KeyCode::Char('A'), KeyModifiers::SHIFT),
            SendKey::Literal("A".to_string())
        );
        assert_eq!(
            map(KeyCode::Char(' '), KeyModifiers::NONE),
            SendKey::Literal(" ".to_string())
        );
    }

    #[test]
    fn literal_args_use_dash_l() {
        assert_eq!(
            SendKey::Literal("a".to_string()).args(),
            vec!["-l".to_string(), "--".to_string(), "a".to_string()]
        );
        assert_eq!(
            SendKey::Named("Enter".to_string()).args(),
            vec!["Enter".to_string()]
        );
    }

    #[test]
    fn enter_is_its_own_named_key() {
        // Enter must stay a distinct submit, never fused into typed text.
        assert_eq!(
            map(KeyCode::Enter, KeyModifiers::NONE),
            SendKey::Named("Enter".to_string())
        );
    }

    #[test]
    fn ctrl_combos_fold_case_and_prefix() {
        assert_eq!(
            map(KeyCode::Char('c'), KeyModifiers::CONTROL),
            SendKey::Named("C-c".to_string())
        );
        // Ctrl folds case: Ctrl+Shift+D still yields C-d.
        assert_eq!(
            map(
                KeyCode::Char('D'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            SendKey::Named("C-d".to_string())
        );
    }

    #[test]
    fn alt_and_ctrl_alt_combos() {
        assert_eq!(
            map(KeyCode::Char('b'), KeyModifiers::ALT),
            SendKey::Named("M-b".to_string())
        );
        assert_eq!(
            map(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            SendKey::Named("M-C-x".to_string())
        );
    }

    #[test]
    fn editing_and_navigation_keys_map_to_names() {
        for (code, name) in [
            (KeyCode::Tab, "Tab"),
            (KeyCode::BackTab, "BTab"),
            (KeyCode::Backspace, "BSpace"),
            (KeyCode::Esc, "Escape"),
            (KeyCode::Left, "Left"),
            (KeyCode::Right, "Right"),
            (KeyCode::Up, "Up"),
            (KeyCode::Down, "Down"),
            (KeyCode::Home, "Home"),
            (KeyCode::End, "End"),
            (KeyCode::PageUp, "PageUp"),
            (KeyCode::PageDown, "PageDown"),
            (KeyCode::Delete, "DC"),
            (KeyCode::Insert, "IC"),
            (KeyCode::F(5), "F5"),
        ] {
            assert_eq!(
                map(code, KeyModifiers::NONE),
                SendKey::Named(name.to_string())
            );
        }
    }

    #[test]
    fn unmapped_keys_return_none() {
        assert_eq!(super::map_key(KeyCode::Null, KeyModifiers::NONE), None);
    }

    #[test]
    fn output_inside_block_is_literal_content() {
        // A `%`-looking line inside a block is content, not a notification.
        let mut p = ControlParser::new();
        assert_eq!(p.push_line("%begin 1 1 1"), None);
        assert_eq!(p.push_line("%output-ish data"), None);
        let ev = p.push_line("%end 1 1 1").unwrap();
        assert_eq!(
            ev,
            ControlEvent::Reply {
                error: false,
                lines: vec!["%output-ish data".to_string()],
            }
        );
    }
}
