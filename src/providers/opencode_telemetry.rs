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
//! (`${XDG_DATA_HOME:-~/.local/share}/opencode/opencode.db`), and the `session` table
//! already carries **per-session totals**, not per-step deltas: `tokens_input`,
//! `tokens_output`, `tokens_reasoning`, `tokens_cache_read`, `tokens_cache_write`, keyed
//! by `id` with a `parent_id` pointing at the session that spawned it. So recovering a
//! child's spend needs no event replay, just `SELECT ... WHERE parent_id = ?`.
//!
//! Measured on a real corpus: the top parent by child spend books 1,384,565 tokens on
//! its own stream against 6,605,838 across 4 children (4.8x what the stream alone
//! would report), the next 1,234,061 against 6,106,099 (4.9x), and one case is 197,553
//! against 3,226,622 (16.3x) — the stream undercounts the true spend by more than an
//! order of magnitude when a slot fans out.
//!
//! **This recovery is additive, not a rewrite.** Unlike muse (whose stdout carries no
//! usage at all, so its session log is the *only* source), opencode's own stream
//! already reports the parent session's tokens correctly — only the children are
//! missing. [`apply`] therefore adds recovered child usage onto the stream-parsed
//! totals rather than replacing them.

use crate::process::StreamStats;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// `${XDG_DATA_HOME:-$HOME/.local/share}/opencode/opencode.db`, if it exists.
pub fn db_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    let p = base.join("opencode/opencode.db");
    p.is_file().then_some(p)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Number of child sessions found; zero means nothing was recovered.
    pub children: u32,
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

/// Sum token usage across every direct child of `session_id` in opencode's own ledger.
///
/// Grandchildren (a subagent that itself fans out) are out of scope: `task` subagents
/// observed on this box are one level deep, and `parent_id` is a single hop by design
/// in opencode's schema, so a second pass would need to walk the tree explicitly if
/// that ever changes.
pub fn collect(db: &Path, session_id: &str) -> Usage {
    let mut usage = Usage::default();
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return usage;
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, \
         tokens_cache_write FROM session WHERE parent_id = ?1",
    ) else {
        return usage;
    };
    let Ok(rows) = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    }) else {
        return usage;
    };
    for (input, output, reasoning, cache_read, cache_write) in rows.flatten() {
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
        usage.children += 1;
    }
    usage
}

/// Add recovered child-session usage onto a slot's stream-parsed stats. The parent
/// session's own tokens already reached `stats` via stdout; this only fills in what the
/// stream's session filter dropped.
pub fn apply(stats: &mut StreamStats, usage: &Usage) {
    if usage.children == 0 {
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
/// db is missing, or when the slot's session id was never captured.
pub fn enrich(stats: &mut StreamStats) {
    let Some(session_id) = stats.session_id.clone() else {
        return;
    };
    let Some(db) = db_path() else { return };
    apply(stats, &collect(&db, &session_id));
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
    fn sums_direct_children_only() {
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
        // A grandchild, and an unrelated sibling tree, must not be swept in.
        insert(&conn, "grandchild", Some("child-a"), (999_999, 0, 0, 0, 0));
        insert(&conn, "unrelated", None, (42, 0, 0, 0, 0));
        drop(conn);

        let usage = collect(&db, "parent");
        assert_eq!(usage.children, 2);
        assert_eq!(usage.input_tokens, 2_000_000 + 3_000_000);
        assert_eq!(usage.output_tokens, 100_000 + 90_838);
        assert_eq!(usage.reasoning_tokens, 5_000);
        assert_eq!(usage.cache_read_tokens, 500_000);
        assert_eq!(usage.cache_write_tokens, 10_000);
    }

    #[test]
    fn no_children_yields_no_records() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        let conn = make_db(&db);
        insert(&conn, "lonely", None, (10, 0, 0, 0, 0));
        drop(conn);
        assert_eq!(collect(&db, "lonely").children, 0);
    }

    #[test]
    fn missing_db_yields_no_records() {
        let tmp = tempdir().unwrap();
        assert_eq!(collect(&tmp.path().join("nope.db"), "x").children, 0);
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
            children: 4,
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
        enrich(&mut stats);
        assert_eq!(stats.billed_tokens, 0);
    }
}
