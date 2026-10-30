//! Build-cache pool (O143, O144): implementers share a few warm build directories
//! (`[worktree] build_cache_pool`) instead of each compiling into its own worktree.
//!
//! A pool slot is free when no live process holds its lease. The lease is a sibling file
//! `<dir>.spar-lease` carrying the holder's pid and start time, the run lock's liveness
//! model: a crashed orchestrator's lease frees itself the moment its pid is gone.
use crate::process::PidToken;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Expand `~/` and one `{a..b}` integer range per entry, in order:
/// `~/projects/run-target-{1..3}` is three directories.
pub fn expand(entries: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in entries {
        for s in expand_range(entry.trim()) {
            let p = crate::util::expand_home(&s);
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn expand_range(s: &str) -> Vec<String> {
    let Some(open) = s.find('{') else {
        return vec![s.to_string()];
    };
    let Some(close) = s[open..].find('}').map(|i| open + i) else {
        return vec![s.to_string()];
    };
    let Some((a, b)) = s[open + 1..close].split_once("..") else {
        return vec![s.to_string()];
    };
    let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) else {
        return vec![s.to_string()];
    };
    let (head, tail) = (&s[..open], &s[close + 1..]);
    let range: Vec<u32> = if a <= b {
        (a..=b).collect()
    } else {
        (b..=a).rev().collect()
    };
    range
        .into_iter()
        .map(|n| format!("{head}{n}{tail}"))
        .collect()
}

#[derive(Debug, Serialize, Deserialize)]
struct LeaseFile {
    pid: u32,
    #[serde(default)]
    starttime: Option<u64>,
    run_id: String,
    slot_id: String,
    project_root: PathBuf,
    leased_at: DateTime<Utc>,
}

impl LeaseFile {
    fn token(&self) -> PidToken {
        PidToken {
            pid: self.pid,
            starttime: self.starttime,
        }
    }
}

fn lease_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".spar-lease");
    dir.with_file_name(name)
}

fn read_lease(dir: &Path) -> Option<LeaseFile> {
    let text = std::fs::read_to_string(lease_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a live process holds `dir`'s lease. `reclaim` asks this before deleting a
/// build directory.
pub fn is_leased(dir: &Path) -> bool {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    [dir.to_path_buf(), canonical]
        .iter()
        .any(|d| read_lease(d).is_some_and(|l| l.token().alive()))
}

/// A held pool slot. Dropping it releases the slot, unless another process has since
/// taken the lease over (it found ours dead, which a drop in a live process cannot be).
#[derive(Debug)]
pub struct PoolLease {
    pub dir: PathBuf,
}

impl Drop for PoolLease {
    fn drop(&mut self) {
        if read_lease(&self.dir).is_some_and(|l| l.pid == std::process::id()) {
            let _ = std::fs::remove_file(lease_path(&self.dir));
        }
    }
}

/// Lease the first free slot, trying `prefer` (the slot this implementer used last
/// round, still warm with its own build) before the pool order. `None` when every slot
/// is held by a live process.
pub fn acquire(
    pool: &[PathBuf],
    prefer: Option<&Path>,
    run_id: &str,
    slot_id: &str,
    project_root: &Path,
) -> Option<PoolLease> {
    let preferred = prefer.filter(|p| pool.iter().any(|d| d == p));
    let order = preferred.into_iter().chain(
        pool.iter()
            .map(PathBuf::as_path)
            .filter(|d| Some(*d) != preferred),
    );
    for dir in order {
        if try_lease(dir, run_id, slot_id, project_root) {
            return Some(PoolLease {
                dir: dir.to_path_buf(),
            });
        }
    }
    None
}

/// Check-and-write under a per-slot `flock`, so two orchestrators racing for the same
/// slot cannot both find it free.
fn try_lease(dir: &Path, run_id: &str, slot_id: &str, project_root: &Path) -> bool {
    let lease = lease_path(dir);
    if let Some(parent) = lease.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    let mut lock_name = lease.file_name().unwrap_or_default().to_os_string();
    lock_name.push(".lock");
    let Ok(lock) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lease.with_file_name(lock_name))
    else {
        return false;
    };
    if lock.lock().is_err() {
        return false;
    }
    let free = read_lease(dir).is_none_or(|l| !l.token().alive());
    let taken = free && {
        let pid = std::process::id();
        let rec = LeaseFile {
            pid,
            starttime: PidToken::capture(pid).starttime,
            run_id: run_id.to_string(),
            slot_id: slot_id.to_string(),
            project_root: project_root.to_path_buf(),
            leased_at: Utc::now(),
        };
        let tmp = lease.with_extension(format!("tmp.{pid}"));
        serde_json::to_string(&rec)
            .ok()
            .is_some_and(|text| std::fs::write(&tmp, text).is_ok())
            && std::fs::rename(&tmp, &lease).is_ok()
    };
    let _ = lock.unlock();
    taken
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn hold(dir: &Path, pid: u32, starttime: Option<u64>) {
        let rec = LeaseFile {
            pid,
            starttime,
            run_id: "other".into(),
            slot_id: "implementer".into(),
            project_root: "/x".into(),
            leased_at: Utc::now(),
        };
        std::fs::write(lease_path(dir), serde_json::to_string(&rec).unwrap()).unwrap();
    }

    #[test]
    fn expand_takes_ranges_and_dedupes() {
        let got = expand(&[
            "/p/t-{1..3}".to_string(),
            "/p/t-2".to_string(),
            "/q/{2..1}-x".to_string(),
            "/r/{a..b}".to_string(),
        ]);
        let want: Vec<PathBuf> = [
            "/p/t-1",
            "/p/t-2",
            "/p/t-3",
            "/q/2-x",
            "/q/1-x",
            "/r/{a..b}",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn a_live_lease_is_skipped_a_dead_one_is_taken_and_drop_releases() {
        let tmp = tempdir().unwrap();
        let pool: Vec<PathBuf> = (1..=2).map(|n| tmp.path().join(format!("t-{n}"))).collect();
        let me = std::process::id();
        hold(&pool[0], me, PidToken::capture(me).starttime);
        let lease = acquire(&pool, None, "r1", "implementer", tmp.path()).unwrap();
        assert_eq!(lease.dir, pool[1], "the held slot is skipped");
        assert!(is_leased(&pool[1]));
        assert!(acquire(&pool, None, "r2", "implementer", tmp.path()).is_none());
        drop(lease);
        assert!(!is_leased(&pool[1]), "drop releases");

        hold(&pool[0], u32::MAX - 7, None);
        let again = acquire(&pool, None, "r3", "implementer", tmp.path()).unwrap();
        assert_eq!(again.dir, pool[0], "a dead holder's slot is free");
    }

    #[test]
    fn the_previous_slot_is_preferred_when_free() {
        let tmp = tempdir().unwrap();
        let pool: Vec<PathBuf> = (1..=3).map(|n| tmp.path().join(format!("t-{n}"))).collect();
        let lease = acquire(&pool, Some(&pool[2]), "r1", "impl", tmp.path()).unwrap();
        assert_eq!(lease.dir, pool[2]);
        let outside = tmp.path().join("elsewhere");
        let other = acquire(&pool, Some(&outside), "r1", "impl", tmp.path()).unwrap();
        assert_eq!(
            other.dir, pool[0],
            "a preference outside the pool is ignored"
        );
    }
}
