//! Recover `cli:opencode` subagent spend, which is structurally invisible on stdout.
//!
//! opencode's `--format json` emitter has a single `process.stdout.write` call site,
//! and it sits behind `if (A.sessionID !== e) continue` — `e` being the top-level
//! session id spar spawned. A `task` subagent runs as a **child session** with its own
//! id, so its `step_finish` events never reach that gate and never reach
//! [`crate::process`]'s `handle_opencode`. Unlike muse (`muse_telemetry`), which walks
//! `subagent/*/session.jsonl` after exit, opencode ships no equivalent recovery: the
//! spend is simply gone from `stats.json`.
//!
//! opencode keeps its own ledger in a sqlite database
//! (`${XDG_DATA_HOME:-~/.local/share}/opencode/opencode.db`, or wherever `OPENCODE_DB`
//! points), and the `session` table already carries **per-session totals**, not
//! per-step deltas: `tokens_input`, `tokens_output`, `tokens_reasoning`,
//! `tokens_cache_read`, `tokens_cache_write`, keyed by `id` with a `parent_id` pointing
//! at the session that spawned it. `parent_id` is a single edge, but the tree it forms
//! is not depth-bounded: a `task` subagent granted its own `task` permission can fan out
//! again, so [`collect`] walks the full descendant subtree (a recursive CTE), not just
//! direct children.
//!
//! The undercount can be large: a subagent that itself fans out (or several running in
//! one turn) can book several times the parent session's own token count. Verified
//! directly on this box with a one-subagent `opencode run` probe: parent session
//! `ses_f838e34f8ffeygS2gOji4elILL` and child `ses_f838e2054ffesdUqZ6DVAkUCaY` both carry
//! real, non-zero `tokens_*` rows in `opencode.db` with `parent_id` set correctly (child
//! input 11,097 against the parent's own 14,001) — confirming a `task` child's usage
//! really does land in `session`, not just in `message.data`, and that summing those
//! columns matches opencode's own `tokens.total` definition.
//!
//! **This recovery is additive, not a rewrite.** Unlike muse (whose stdout carries no
//! usage at all, so its session log is the *only* source), opencode's own stream
//! already reports the parent session's tokens correctly — only the children are
//! missing. [`apply`] therefore adds recovered child usage onto the stream-parsed
//! totals rather than replacing them.
//!
//! **Spend only, not tool counts.** The same session filter that drops a subagent's
//! `step_finish` also drops its `tool` events, so a fanned-out slot's `tools` /
//! `tool_errors` still reflect only the parent session. Unlike muse (which does recover
//! subagent tool counts from its session log), closing that gap here needs a second
//! query against `part`/`message`, not just `session`, and is not attempted by this
//! module.
//!
//! **Known undercount, not fixed by this module:** a `task` subagent launched with
//! `background=true` keeps running after the parent session's stream closes, so a
//! recovery read that lands right at exit can see a child session's row before
//! opencode has finished incrementing it. Same class of gap as the grandchild case
//! before this change: real spend that the ledger has not caught up to yet.

use crate::process::StreamStats;
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `${XDG_DATA_HOME:-$HOME/.local/share}/opencode`.
fn data_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("opencode"))
}

/// The sqlite ledger opencode is actually writing to.
///
/// Honours `OPENCODE_DB` the same way opencode's own binary resolves it: `:memory:`
/// means there is nothing on disk to read, an absolute path is used as-is, and a
/// relative one joins the data dir. Unset (or set to the empty string, which
/// opencode's own resolver treats as unset rather than a relative path to `.`), asks a
/// real `opencode` binary via its own `opencode db path` subcommand rather than
/// guessing: which channel maps to `opencode.db` versus `opencode-<channel>.db` is a
/// rule inside opencode's own bundle (`latest`/`beta`/`prod`/
/// `OPENCODE_DISABLE_CHANNEL_DB` all resolve to the former, everything else to the
/// latter), not something spar can read from outside without re-deriving it by hand and
/// drifting the day opencode changes it.
pub fn db_path() -> Option<PathBuf> {
    if let Some(over) = std::env::var_os("OPENCODE_DB") {
        if !over.is_empty() {
            if over == ":memory:" {
                return None;
            }
            let dir = data_dir()?;
            let p = PathBuf::from(over);
            let p = if p.is_absolute() { p } else { dir.join(p) };
            return p.is_file().then_some(p);
        }
    }
    let bin = crate::providers::adapter_named("opencode")?.resolve_binary()?;
    resolve_via_binary(&bin)
}

/// Ask a real `opencode` binary where its own ledger lives, via `opencode db path`.
fn resolve_via_binary(bin: &Path) -> Option<PathBuf> {
    let out = run_with_timeout(bin, &["db", "path"], Duration::from_secs(5))?;
    let out = out.trim();
    (!out.is_empty() && out != ":memory:").then(|| PathBuf::from(out))
}

/// Run `bin args..` to completion and return its stdout, killing it and giving up past
/// `timeout` so a wedged child can never hang slot dispatch — this recovery is
/// best-effort telemetry, not something worth blocking on.
fn run_with_timeout(bin: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Number of descendant sessions found (children and their own descendants, not
    /// just direct children); zero means nothing was recovered.
    pub descendants: u32,
}

/// Why [`collect`] could not read the ledger, distinct from "read it fine and found no
/// descendants" so a caller can tell real breakage from the common no-fan-out case.
#[derive(Debug, PartialEq, Eq)]
pub enum CollectError {
    Open,
    Prepare,
    Query,
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CollectError::Open => "could not open the ledger",
            CollectError::Prepare => "the descendant-subtree query failed to prepare",
            CollectError::Query => "the descendant-subtree query failed",
        })
    }
}

impl Usage {
    /// Reasoning is billed at output rates, matching the fold `handle_opencode` already
    /// does for the parent session's own step deltas.
    pub fn billed_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.reasoning_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}

/// Sum token usage across every descendant of `session_id` in opencode's own ledger —
/// direct children and any subagent they themselves fanned out to, transitively.
///
/// The recursive CTE's `UNION` (not `UNION ALL`) dedupes visited ids, so a cycle in
/// `parent_id` (which should never happen, but the ledger is an external file spar does
/// not control) terminates instead of looping.
///
/// Returns `Err` when the ledger could not be read at all — a real anomaly, since
/// [`db_path`] only returns a path it believes is opencode's own — as opposed to `Ok`
/// with zero descendants, which just means this session never fanned out.
pub fn collect(db: &Path, session_id: &str) -> Result<Usage, CollectError> {
    let mut usage = Usage::default();
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| CollectError::Open)?;
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE descendants(id) AS ( \
                 SELECT id FROM session WHERE parent_id = ?1 \
                 UNION \
                 SELECT s.id FROM session s JOIN descendants d ON s.parent_id = d.id \
             ) \
             SELECT tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, \
             tokens_cache_write FROM session WHERE id IN (SELECT id FROM descendants)",
        )
        .map_err(|_| CollectError::Prepare)?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|_| CollectError::Query)?;
    for row in rows {
        let (input, output, reasoning, cache_read, cache_write) =
            row.map_err(|_| CollectError::Query)?;
        usage.input_tokens = usage.input_tokens.saturating_add(input.max(0) as u64);
        usage.output_tokens = usage.output_tokens.saturating_add(output.max(0) as u64);
        usage.reasoning_tokens = usage
            .reasoning_tokens
            .saturating_add(reasoning.max(0) as u64);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(cache_read.max(0) as u64);
        usage.cache_write_tokens = usage
            .cache_write_tokens
            .saturating_add(cache_write.max(0) as u64);
        usage.descendants += 1;
    }
    Ok(usage)
}

/// Add recovered child-session usage onto a slot's stream-parsed stats. The parent
/// session's own tokens already reached `stats` via stdout; this only fills in what the
/// stream's session filter dropped.
pub fn apply(stats: &mut StreamStats, usage: &Usage) {
    if usage.descendants == 0 {
        return;
    }
    stats.input_tokens = stats.input_tokens.saturating_add(usage.input_tokens);
    stats.output_tokens = stats
        .output_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.reasoning_tokens);
    stats.cache_read_tokens = stats
        .cache_read_tokens
        .saturating_add(usage.cache_read_tokens);
    stats.cache_write_tokens = stats
        .cache_write_tokens
        .saturating_add(usage.cache_write_tokens);
    stats.billed_tokens = stats.billed_tokens.saturating_add(usage.billed_tokens());
    // `context_tokens` is the peak single-call prompt footprint, a gauge on the
    // parent's own window; a child session runs in its own context and does not
    // extend it.
}

/// Rewrite an opencode slot's stats with recovered subagent spend, keyed off the
/// session id spar already recorded from the stream. A no-op when opencode never fanned
/// out (the common case today, since no shipped role prompt tells a slot to), when the
/// db is missing, or when the slot's session id was never captured. Returns a note
/// worth logging when a ledger was found but could not actually be read — that case is
/// indistinguishable from "nothing to recover" in the stats alone.
pub fn enrich(stats: &mut StreamStats) -> Option<String> {
    let session_id = stats.session_id.clone()?;
    let db = db_path()?;
    match collect(&db, &session_id) {
        Ok(usage) => {
            apply(stats, &usage);
            None
        }
        Err(err) => Some(format!(
            "opencode subagent-spend recovery: {err} ({})",
            db.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                cost REAL NOT NULL DEFAULT 0,
                tokens_input INTEGER NOT NULL DEFAULT 0,
                tokens_output INTEGER NOT NULL DEFAULT 0,
                tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    /// `tokens` is `(input, output, reasoning, cache_read, cache_write)`.
    fn insert(
        conn: &rusqlite::Connection,
        id: &str,
        parent: Option<&str>,
        tokens: (i64, i64, i64, i64, i64),
    ) {
        let (input, output, reasoning, cache_read, cache_write) = tokens;
        conn.execute(
            "INSERT INTO session (id, parent_id, tokens_input, tokens_output, \
             tokens_reasoning, tokens_cache_read, tokens_cache_write) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                id,
                parent,
                input,
                output,
                reasoning,
                cache_read,
                cache_write
            ],
        )
        .unwrap();
    }

    #[test]
    fn sums_the_whole_descendant_subtree() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        let conn = make_db(&db);
        insert(&conn, "parent", None, (1_384_565, 0, 0, 0, 0));
        insert(
            &conn,
            "child-a",
            Some("parent"),
            (2_000_000, 100_000, 5_000, 500_000, 10_000),
        );
        insert(
            &conn,
            "child-b",
            Some("parent"),
            (3_000_000, 90_838, 0, 0, 0),
        );
        // A grandchild (a subagent that itself fanned out) and a great-grandchild must
        // be swept in; an unrelated sibling tree must not.
        insert(&conn, "grandchild", Some("child-a"), (999_999, 0, 0, 0, 0));
        insert(
            &conn,
            "great-grandchild",
            Some("grandchild"),
            (1, 2, 3, 4, 5),
        );
        insert(&conn, "unrelated", None, (42, 0, 0, 0, 0));
        drop(conn);

        let usage = collect(&db, "parent").unwrap();
        assert_eq!(usage.descendants, 4);
        assert_eq!(usage.input_tokens, 2_000_000 + 3_000_000 + 999_999 + 1);
        assert_eq!(usage.output_tokens, 100_000 + 90_838 + 2);
        assert_eq!(usage.reasoning_tokens, 5_000 + 3);
        assert_eq!(usage.cache_read_tokens, 500_000 + 4);
        assert_eq!(usage.cache_write_tokens, 10_000 + 5);
    }

    #[test]
    fn no_descendants_yields_no_records() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        let conn = make_db(&db);
        insert(&conn, "lonely", None, (10, 0, 0, 0, 0));
        drop(conn);
        assert_eq!(collect(&db, "lonely").unwrap().descendants, 0);
    }

    #[test]
    fn missing_db_is_an_open_error() {
        let tmp = tempdir().unwrap();
        assert_eq!(
            collect(&tmp.path().join("nope.db"), "x"),
            Err(CollectError::Open)
        );
    }

    #[test]
    fn missing_session_table_is_a_prepare_error() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE not_session (id TEXT);")
            .unwrap();
        assert_eq!(collect(&db, "x"), Err(CollectError::Prepare));
    }

    #[test]
    fn apply_adds_onto_the_streams_own_parent_totals() {
        // The stream already reported the parent's own step deltas; recovery must add
        // to that, never replace it, since opencode (unlike muse) does carry live usage
        // on stdout for the session it names.
        let mut stats = StreamStats {
            input_tokens: 1_384_565,
            output_tokens: 200_000,
            cache_read_tokens: 50_000,
            cache_write_tokens: 5_000,
            billed_tokens: 1_384_565 + 200_000 + 50_000 + 5_000,
            ..Default::default()
        };
        let usage = Usage {
            input_tokens: 5_000_000,
            output_tokens: 190_838,
            reasoning_tokens: 10_000,
            cache_read_tokens: 500_000,
            cache_write_tokens: 15_000,
            descendants: 4,
        };
        apply(&mut stats, &usage);
        assert_eq!(stats.input_tokens, 1_384_565 + 5_000_000);
        assert_eq!(stats.output_tokens, 200_000 + 190_838 + 10_000);
        assert_eq!(stats.cache_read_tokens, 50_000 + 500_000);
        assert_eq!(stats.cache_write_tokens, 5_000 + 15_000);
        assert_eq!(
            stats.billed_tokens,
            stats.input_tokens
                + stats.output_tokens
                + stats.cache_read_tokens
                + stats.cache_write_tokens,
            "the published identity must hold after recovery, not just before it"
        );
    }

    #[test]
    fn apply_is_a_no_op_when_nothing_fanned_out() {
        let mut stats = StreamStats {
            input_tokens: 42,
            billed_tokens: 42,
            ..Default::default()
        };
        apply(&mut stats, &Usage::default());
        assert_eq!(stats.input_tokens, 42);
        assert_eq!(stats.billed_tokens, 42);
    }

    #[test]
    fn enrich_is_a_no_op_without_a_session_id() {
        let mut stats = StreamStats::default();
        assert_eq!(enrich(&mut stats), None);
        assert_eq!(stats.billed_tokens, 0);
    }

    #[test]
    fn enrich_reports_a_note_when_the_resolved_ledger_cannot_be_read() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let db = tmp.path().join("broken.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE not_session (id TEXT);")
            .unwrap();
        std::env::set_var("OPENCODE_DB", &db);

        let mut stats = StreamStats {
            session_id: Some("parent".to_string()),
            ..Default::default()
        };
        let note = enrich(&mut stats).expect("a broken ledger must surface a note");
        assert!(
            note.contains("descendant-subtree query failed to prepare"),
            "note was: {note}"
        );
    }

    // The remaining tests mutate process env (`XDG_DATA_HOME`, `OPENCODE_DB`), which
    // only needs to be serialized against other tests in *this* module: nothing else in
    // the binary reads those two vars from a test today, muse_telemetry's
    // `sessions_root()` included (it reads `XDG_DATA_HOME` in production code, but no
    // muse test calls it).
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            let guard = ENV_LOCK.lock().unwrap();
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("OPENCODE_DB");
            EnvGuard { _guard: guard }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("OPENCODE_DB");
        }
    }

    #[test]
    fn db_path_honours_an_absolute_opencode_db_override() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let elsewhere = tmp.path().join("elsewhere.db");
        std::fs::write(&elsewhere, b"x").unwrap();
        std::env::set_var("OPENCODE_DB", &elsewhere);
        assert_eq!(db_path(), Some(elsewhere));
    }

    #[test]
    fn db_path_honours_a_relative_opencode_db_override() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let dir = tmp.path().join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode-prod.db"), b"x").unwrap();
        std::env::set_var("OPENCODE_DB", "opencode-prod.db");
        assert_eq!(db_path(), Some(dir.join("opencode-prod.db")));
    }

    #[test]
    fn db_path_is_none_when_opencode_db_is_memory() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        std::env::set_var("OPENCODE_DB", ":memory:");
        assert_eq!(db_path(), None);
    }

    #[test]
    fn enrich_recovers_child_spend_via_an_explicit_db_override_end_to_end() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let db = tmp.path().join("opencode.db");
        let conn = make_db(&db);
        insert(&conn, "parent", None, (100, 0, 0, 0, 0));
        insert(&conn, "child", Some("parent"), (10, 20, 0, 5, 1));
        drop(conn);
        std::env::set_var("OPENCODE_DB", &db);

        let mut stats = StreamStats {
            session_id: Some("parent".to_string()),
            input_tokens: 100,
            billed_tokens: 100,
            ..Default::default()
        };
        assert_eq!(enrich(&mut stats), None);
        assert_eq!(stats.input_tokens, 110);
        assert_eq!(stats.output_tokens, 20);
        assert_eq!(stats.cache_read_tokens, 5);
        assert_eq!(stats.cache_write_tokens, 1);
        assert_eq!(stats.billed_tokens, 100 + 10 + 20 + 5 + 1);
    }

    /// Writes an executable shell script standing in for the `opencode` binary, so
    /// `db_path()`'s "ask the binary" fallback can be exercised without a real install.
    #[cfg(unix)]
    fn write_fake_opencode(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("opencode");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    struct PathGuard {
        old: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl PathGuard {
        fn prepend(dir: &Path) -> Self {
            let old = std::env::var_os("PATH");
            let mut new = std::ffi::OsString::from(dir);
            if let Some(old) = &old {
                new.push(":");
                new.push(old);
            }
            std::env::set_var("PATH", new);
            PathGuard { old }
        }
    }

    #[cfg(unix)]
    impl Drop for PathGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn db_path_asks_a_real_opencode_binary_when_unset() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let real_db = tmp.path().join("resolved-by-opencode.db");
        std::fs::write(&real_db, b"x").unwrap();
        let bindir = tmp.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        write_fake_opencode(&bindir, &format!("echo '{}'", real_db.display()));
        let _path = PathGuard::prepend(&bindir);

        assert_eq!(db_path(), Some(real_db));
    }

    #[cfg(unix)]
    #[test]
    fn db_path_asks_a_real_opencode_binary_when_opencode_db_is_empty() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let real_db = tmp.path().join("resolved-by-opencode.db");
        std::fs::write(&real_db, b"x").unwrap();
        let bindir = tmp.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        write_fake_opencode(&bindir, &format!("echo '{}'", real_db.display()));
        let _path = PathGuard::prepend(&bindir);
        std::env::set_var("OPENCODE_DB", "");

        assert_eq!(db_path(), Some(real_db));
    }

    #[cfg(unix)]
    #[test]
    fn db_path_ignores_a_binary_that_reports_memory() {
        let _env = EnvGuard::acquire();
        let tmp = tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let bindir = tmp.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        write_fake_opencode(&bindir, "echo ':memory:'");
        let _path = PathGuard::prepend(&bindir);

        assert_eq!(db_path(), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_returns_stdout_on_success() {
        let tmp = tempdir().unwrap();
        let script = write_fake_opencode(tmp.path(), "echo hello");
        let out = run_with_timeout(&script, &[], Duration::from_secs(5)).unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_returns_none_on_a_nonzero_exit() {
        let tmp = tempdir().unwrap();
        let script = write_fake_opencode(tmp.path(), "exit 1");
        assert_eq!(run_with_timeout(&script, &[], Duration::from_secs(5)), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_a_wedged_process() {
        let tmp = tempdir().unwrap();
        let script = write_fake_opencode(tmp.path(), "sleep 30");
        let start = Instant::now();
        assert_eq!(
            run_with_timeout(&script, &[], Duration::from_millis(200)),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the wedged child must be killed, not waited out"
        );
    }
}
