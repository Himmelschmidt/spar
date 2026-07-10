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
    /// Running and silent longer than `timeouts.stall_warn_secs` (0 disables).
    pub stalled: bool,
}

impl SlotActivity {
    pub fn observe(slot: &SlotState, stall_warn_secs: u64) -> Self {
        let last = slot.log_path.as_ref().and_then(|p| last_log_time(p));
        let silent_for_secs = last.map(|t| {
            let now = Utc::now();
            (now - t).num_seconds().max(0) as u64
        });
        let stalled = slot.status == SlotStatus::Running
            && stall_warn_secs > 0
            && silent_for_secs
                .map(|s| s >= stall_warn_secs)
                .unwrap_or(false);
        Self {
            last_log_at: last.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            silent_for_secs,
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
        let act = SlotActivity::observe(slot, warn);
        let token = crate::markers::read_pid(paths, run_id, &slot.id)
            .or_else(|| slot.pid.map(crate::process::PidToken::from_pid));
        let pid = token.map(|t| t.pid);
        let pid_alive = token.map(|t| t.alive()).unwrap_or(false);
        if let Some(obj) = slot_val.as_object_mut() {
            if let Some(t) = &act.last_log_at {
                obj.insert("last_log_at".into(), serde_json::Value::String(t.clone()));
            }
            if let Some(s) = act.silent_for_secs {
                obj.insert("silent_for_secs".into(), serde_json::json!(s));
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
        let act = SlotActivity::observe(&slot, 300);
        assert!(act.silent_for_secs.unwrap() >= 500);
        assert!(act.stalled);
        assert!(act.last_log_at.is_some());

        let done = slot_with_log(&log, SlotStatus::Done);
        let act2 = SlotActivity::observe(&done, 300);
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
        let act = SlotActivity::observe(&slot, 0);
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
