//! Per-slot log activity: last write time + stall detection.
use crate::config::Config;
use crate::process::StreamStats;
use crate::state::{SlotState, SlotStatus};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

/// Live observation of a slot's log output (not persisted on SlotState).
#[derive(Debug, Clone, Serialize)]
pub struct SlotActivity {
    /// RFC3339 timestamp of last log write (stats or log mtime).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_log_at: Option<String>,
    /// Seconds since last_log_at (None if unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent_for_secs: Option<u64>,
    /// RFC3339 timestamp of last heartbeat (process-liveness, independent of log output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
    /// Running and silent — in both log output *and* heartbeat — longer than
    /// `timeouts.stall_warn_secs` (0 disables). A quiet-but-heartbeating slot is healthy.
    pub stalled: bool,
}

impl SlotActivity {
    /// `last_heartbeat` is the freshest process-liveness beat for the slot (see
    /// [`crate::bus::heartbeat_map`]); `None` when the slot never heartbeat.
    /// `hard_stall_secs` is the slot's role timeout — the point past which continued log
    /// silence is a stall even while the process still heartbeats (0 disables that arm).
    pub fn observe(
        slot: &SlotState,
        stall_warn_secs: u64,
        hard_stall_secs: u64,
        last_heartbeat: Option<DateTime<Utc>>,
    ) -> Self {
        let now = Utc::now();
        let last = slot.log_path.as_ref().and_then(|p| last_log_time(p));
        let silent_for_secs = last.map(|t| (now - t).num_seconds().max(0) as u64);
        // The heartbeat is process-liveness, not progress: a live child beats every ~30s
        // regardless of whether it is working. So a quiet-but-heartbeating slot is treated
        // as working — UNTIL it has been log-silent for its entire role budget
        // (`hard_stall_secs`), at which point an alive-but-hung slot is stalled. A slot that
        // has also stopped heartbeating (likely dead/gone) stalls at the warn threshold.
        let heartbeat_silent = last_heartbeat
            .map(|t| (now - t).num_seconds().max(0) as u64)
            .unwrap_or(u64::MAX);
        let log_silent = silent_for_secs.unwrap_or(0);
        let stalled = slot.status == SlotStatus::Running
            && stall_warn_secs > 0
            && log_silent >= stall_warn_secs
            && (heartbeat_silent >= stall_warn_secs
                || (hard_stall_secs > 0 && log_silent >= hard_stall_secs));
        Self {
            last_log_at: last.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            silent_for_secs,
            last_heartbeat_at: last_heartbeat
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            stalled,
        }
    }

    pub fn human_silent(&self) -> String {
        match self.silent_for_secs {
            Some(s) => format_duration_short(s),
            None => "—".into(),
        }
    }
}

/// Best-effort last log write: freshest among stats field, log mtime, stats file mtime.
/// Prefer max so a stale stats snapshot never masks a fresher log write (false STALL).
pub fn last_log_time(log_path: &Path) -> Option<DateTime<Utc>> {
    let mut best: Option<DateTime<Utc>> = None;
    if let Some(stats) = StreamStats::load(log_path) {
        if let Some(s) = stats.last_log_at.as_deref() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                best = Some(dt.with_timezone(&Utc));
            }
        }
    }
    for t in [
        file_mtime(log_path),
        file_mtime(&StreamStats::stats_path(log_path)),
    ]
    .into_iter()
    .flatten()
    {
        best = Some(match best {
            Some(b) if b >= t => b,
            _ => t,
        });
    }
    best
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    system_time_to_utc(modified)
}

fn system_time_to_utc(t: SystemTime) -> Option<DateTime<Utc>> {
    let dur = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    DateTime::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos())
}

pub fn format_duration_short(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    }
}

/// Enrich a status JSON value's `slots` array with activity + liveness fields.
pub fn enrich_status_json(
    v: &mut serde_json::Value,
    state_slots: &[SlotState],
    cfg: &Config,
    paths: &crate::paths::SparPaths,
    run_id: &str,
) {
    let warn = cfg.timeouts.stall_warn_secs;
    let by_id: std::collections::HashMap<&str, &SlotState> =
        state_slots.iter().map(|s| (s.id.as_str(), s)).collect();
    let hb_map = crate::bus::heartbeat_map(paths, Some(run_id));
    let Some(slots) = v.get_mut("slots").and_then(|s| s.as_array_mut()) else {
        return;
    };
    for slot_val in slots.iter_mut() {
        let id = slot_val
            .get("id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let Some(slot) = id.as_deref().and_then(|id| by_id.get(id).copied()) else {
            continue;
        };
        let hb = hb_map
            .get(&crate::bus::resolve_addr(Some(run_id), &slot.id))
            .copied();
        let hard = crate::executor::timeout_for_role(cfg, slot.role).as_secs();
        let act = SlotActivity::observe(slot, warn, hard, hb);
        let token = crate::markers::read_pid(paths, run_id, &slot.id)
            .or_else(|| slot.pid.map(crate::process::PidToken::from_pid));
        let pid = token.map(|t| t.pid);
        let pid_alive = token.map(|t| t.alive()).unwrap_or(false);
        if let Some(obj) = slot_val.as_object_mut() {
            // Mirror the slot id under `slot` too: the state serialization names it `id`,
            // but outer agents key per-slot data by `slot`.
            obj.insert("slot".into(), serde_json::Value::String(slot.id.clone()));
            if let Some(t) = &act.last_log_at {
                obj.insert("last_log_at".into(), serde_json::Value::String(t.clone()));
            }
            if let Some(s) = act.silent_for_secs {
                obj.insert("silent_for_secs".into(), serde_json::json!(s));
            }
            if let Some(t) = &act.last_heartbeat_at {
                obj.insert(
                    "last_heartbeat_at".into(),
                    serde_json::Value::String(t.clone()),
                );
            }
            obj.insert("stalled".into(), serde_json::Value::Bool(act.stalled));
            obj.insert(
                "pid".into(),
                match pid {
                    Some(p) => serde_json::json!(p),
                    None => serde_json::Value::Null,
                },
            );
            obj.insert("pid_alive".into(), serde_json::Value::Bool(pid_alive));
            if let Some(c) = slot.exit_code {
                obj.insert("exit_code".into(), serde_json::json!(c));
            }
            if let Some(sig) = slot.signal {
                obj.insert("signal".into(), serde_json::json!(sig));
            }
        }
    }
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "stall_warn_secs".into(),
            serde_json::json!(cfg.timeouts.stall_warn_secs),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_ref::ProviderRef;
    use crate::state::{SlotRole, SlotState};
    use std::io::Write;
    use tempfile::tempdir;

    fn slot_with_log(log: &Path, status: SlotStatus) -> SlotState {
        let pref = ProviderRef::parse("cli:claude").unwrap();
        SlotState {
            id: "impl".into(),
            provider: "cli:claude".into(),
            role: SlotRole::Implementer,
            status,
            backend: None,
            exec_backend: Some(pref.backend),
            cwd: None,
            log_path: Some(log.to_path_buf()),
            error: None,
            pid: None,
            exit_code: None,
            signal: None,
            artifact: None,
            usage: None,
            model: None,
        }
    }

    #[test]
    fn duration_short() {
        assert_eq!(format_duration_short(45), "45s");
        assert_eq!(format_duration_short(120), "2m");
        assert_eq!(format_duration_short(3700), "1h1m");
    }

    #[test]
    fn observes_log_mtime_and_stall() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("s.log");
        {
            let mut f = std::fs::File::create(&log).unwrap();
            writeln!(f, "hello").unwrap();
        }
        // Backdate mtime to simulate long silence.
        let past = SystemTime::now() - std::time::Duration::from_secs(600);
        filetime_set(&log, past);

        let slot = slot_with_log(&log, SlotStatus::Running);
        // No heartbeat: log-silent past threshold ⇒ stalled.
        let act = SlotActivity::observe(&slot, 300, 1800, None);
        assert!(act.silent_for_secs.unwrap() >= 500);
        assert!(act.stalled);
        assert!(act.last_log_at.is_some());

        let done = slot_with_log(&log, SlotStatus::Done);
        let act2 = SlotActivity::observe(&done, 300, 1800, None);
        assert!(!act2.stalled);
    }

    #[test]
    fn fresh_heartbeat_prevents_stall_within_role_budget() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("s.log");
        std::fs::write(&log, "hello\n").unwrap();
        let past = SystemTime::now() - std::time::Duration::from_secs(600);
        filetime_set(&log, past);

        let slot = slot_with_log(&log, SlotStatus::Running);
        // Log 600s stale, heartbeat fresh, still well inside the 1800s role budget ⇒ working.
        let act = SlotActivity::observe(&slot, 300, 1800, Some(Utc::now()));
        assert!(act.silent_for_secs.unwrap() >= 500);
        assert!(
            !act.stalled,
            "fresh heartbeat within budget must clear the stall"
        );
        assert!(act.last_heartbeat_at.is_some());

        // A stale heartbeat (older than the warn threshold) no longer protects it.
        let stale_hb = Utc::now() - chrono::Duration::seconds(600);
        let act2 = SlotActivity::observe(&slot, 300, 1800, Some(stale_hb));
        assert!(act2.stalled, "stale heartbeat + silent log ⇒ stalled");
    }

    #[test]
    fn hard_cap_stalls_alive_but_hung() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("s.log");
        std::fs::write(&log, "x\n").unwrap();
        // Log silent for 2000s — past the 1800s role budget.
        let past = SystemTime::now() - std::time::Duration::from_secs(2000);
        filetime_set(&log, past);
        let slot = slot_with_log(&log, SlotStatus::Running);

        // Fresh heartbeat (alive) but silent past the hard cap ⇒ stalled anyway.
        let act = SlotActivity::observe(&slot, 300, 1800, Some(Utc::now()));
        assert!(
            act.stalled,
            "log-silent past hard cap ⇒ stalled even while heartbeating"
        );
        // Same silence with the hard cap disabled (0) and a fresh heartbeat ⇒ not stalled.
        let act2 = SlotActivity::observe(&slot, 300, 0, Some(Utc::now()));
        assert!(!act2.stalled);
    }

    #[test]
    fn fresher_log_mtime_beats_stale_stats() {
        use chrono::Datelike;
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("s.log");
        std::fs::write(&log, "x\n").unwrap();
        let stats = StreamStats {
            last_log_at: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        stats.save(&log).unwrap();
        // Fresh log write after stale stats stamp.
        std::fs::write(&log, "fresh\n").unwrap();
        let t = last_log_time(&log).unwrap();
        assert!(
            t.year() >= 2025,
            "must prefer fresher log mtime over stale stats, got {t}"
        );
    }

    #[test]
    fn stall_warn_zero_disables_flag() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("s.log");
        std::fs::write(&log, "x\n").unwrap();
        let past = SystemTime::now() - std::time::Duration::from_secs(600);
        filetime_set(&log, past);
        let slot = slot_with_log(&log, SlotStatus::Running);
        let act = SlotActivity::observe(&slot, 0, 1800, None);
        assert!(act.silent_for_secs.unwrap() >= 500);
        assert!(!act.stalled);
    }

    fn filetime_set(path: &Path, t: SystemTime) {
        // Use utime via filetime crate? Not a dep — use `touch -d` style via libc or std.
        // On Linux, set with filetime from std is not available; use Command touch.
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let _ = std::process::Command::new("touch")
            .args(["-d", &format!("@{secs}"), path.to_str().unwrap()])
            .status();
    }
}
