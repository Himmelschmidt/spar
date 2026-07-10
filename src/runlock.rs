use crate::paths::SparPaths;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("run {run_id} already has a running orchestrator (pid {owner_pid}); use 'spar stop {run_id}' first")]
pub struct OrchestratorBusy {
    pub run_id: String,
    pub owner_pid: u32,
}

/// Single-orchestrator guard for a run id, backed by an advisory (`flock`) lock
/// on `orchestrator.lock`.
///
/// Exclusion is enforced by the kernel per open file description, so acquisition
/// is race-free even under concurrent takeover and a lock held by a crashed
/// orchestrator is released automatically when its process dies. The file body
/// only carries the holder pid for observability (`owner`, `spar status`).
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    pid: u32,
    file: File,
}

fn lock_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    paths.run_dir(run_id).join("orchestrator.lock")
}

fn read_owner_token(path: &Path) -> Option<crate::process::PidToken> {
    crate::process::PidToken::parse(&fs::read_to_string(path).ok()?)
}

fn read_owner_pid(path: &Path) -> Option<u32> {
    read_owner_token(path).map(|t| t.pid)
}

impl RunLock {
    pub fn acquire(paths: &SparPaths, run_id: &str) -> Result<RunLock> {
        paths.ensure_run_dirs(run_id)?;
        let path = lock_path(paths, run_id);
        let me = std::process::id();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(OrchestratorBusy {
                    run_id: run_id.to_string(),
                    owner_pid: read_owner_pid(&path).unwrap_or(0),
                }
                .into());
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("lock {}", path.display()));
            }
        }
        let reclaimed = read_owner_pid(&path).filter(|&prev| prev != me);
        file.set_len(0)
            .with_context(|| format!("truncate {}", path.display()))?;
        (&file)
            .write_all(crate::process::PidToken::capture(me).encode().as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        if let Some(prev) = reclaimed {
            let _ = crate::events::append(
                paths,
                run_id,
                &crate::events::Event::info(format!(
                    "orchestrator lock reclaimed by pid {me} from crashed pid {prev}"
                )),
            );
        }
        Ok(RunLock {
            path,
            pid: me,
            file,
        })
    }

    pub fn owner(paths: &SparPaths, run_id: &str) -> Option<crate::process::PidToken> {
        read_owner_token(&lock_path(paths, run_id))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Clear the pid so `owner` reports none once released, but only while the
        // file still names us; the kernel drops the flock as the file closes.
        if read_owner_pid(&self.path) == Some(self.pid) {
            let _ = self.file.set_len(0);
        }
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn owner_pid(paths: &SparPaths, run_id: &str) -> Option<u32> {
        RunLock::owner(paths, run_id).map(|t| t.pid)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owner_carries_starttime_and_matches_self() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let _held = RunLock::acquire(&paths, "r1").unwrap();
        let owner = RunLock::owner(&paths, "r1").expect("owner recorded");
        assert_eq!(owner.pid, std::process::id());
        assert!(owner.starttime.is_some(), "lock must record a start-time");
        assert!(owner.alive(), "live self must match its own start-time");
    }

    #[test]
    fn acquire_fresh_succeeds() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let lock = RunLock::acquire(&paths, "r1").unwrap();
        assert_eq!(lock.pid, std::process::id());
        assert_eq!(owner_pid(&paths, "r1"), Some(std::process::id()));
    }

    #[test]
    fn second_acquire_fails_with_owner_pid() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let _held = RunLock::acquire(&paths, "r1").unwrap();
        let err = RunLock::acquire(&paths, "r1").unwrap_err();
        let busy = err
            .downcast_ref::<OrchestratorBusy>()
            .expect("busy error carrying owner pid");
        assert_eq!(busy.owner_pid, std::process::id());
    }

    #[test]
    fn drop_releases_for_next_acquire() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let first = RunLock::acquire(&paths, "r1").unwrap();
        drop(first);
        assert_eq!(owner_pid(&paths, "r1"), None);
        let _second = RunLock::acquire(&paths, "r1").unwrap();
    }

    #[test]
    fn dead_pid_is_taken_over() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        fs::write(lock_path(&paths, "r1"), (i32::MAX as u32).to_string()).unwrap();
        let lock = RunLock::acquire(&paths, "r1").unwrap();
        assert_eq!(lock.pid, std::process::id());
        assert_eq!(owner_pid(&paths, "r1"), Some(std::process::id()));
    }

    #[test]
    fn concurrent_takeover_yields_single_winner() {
        use std::sync::Arc;
        use std::thread;
        let tmp = tempdir().unwrap();
        let paths = Arc::new(SparPaths::new(tmp.path()));
        for _ in 0..200 {
            paths.ensure_run_dirs("r1").unwrap();
            fs::write(lock_path(&paths, "r1"), (i32::MAX as u32).to_string()).unwrap();
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let p = Arc::clone(&paths);
                    thread::spawn(move || RunLock::acquire(&p, "r1"))
                })
                .collect();
            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let winners = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(winners, 1, "exactly one orchestrator must win the takeover");
            for r in &results {
                if let Err(e) = r {
                    assert!(
                        e.downcast_ref::<OrchestratorBusy>().is_some(),
                        "losers must report busy, got: {e:#}"
                    );
                }
            }
            assert_eq!(owner_pid(&paths, "r1"), Some(std::process::id()));
            drop(results);
            let _ = fs::remove_file(lock_path(&paths, "r1"));
        }
    }

    #[test]
    fn drop_does_not_delete_after_takeover() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let lock = RunLock::acquire(&paths, "r1").unwrap();
        // A concurrent takeover replaces the file contents with another pid.
        fs::write(lock_path(&paths, "r1"), (i32::MAX as u32).to_string()).unwrap();
        drop(lock);
        assert_eq!(
            owner_pid(&paths, "r1"),
            Some(i32::MAX as u32),
            "drop must not delete a lock a takeover handed to someone else"
        );
    }
}
