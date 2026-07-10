use crate::paths::SparPaths;
use crate::process::pid_alive;
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("run {run_id} already has a running orchestrator (pid {owner_pid}); use 'spar stop {run_id}' first")]
pub struct OrchestratorBusy {
    pub run_id: String,
    pub owner_pid: u32,
}

/// Single-orchestrator guard for a run id, backed by `orchestrator.lock`.
///
/// Acquisition is atomic via `O_EXCL`; a lock left by a dead pid is stale and
/// gets taken over. `Drop` releases the lock only while it still names us, so a
/// takeover that handed the file to another pid is never clobbered.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    pid: u32,
}

fn lock_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    paths.run_dir(run_id).join("orchestrator.lock")
}

fn read_owner_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse::<u32>().ok()
}

impl RunLock {
    pub fn acquire(paths: &SparPaths, run_id: &str) -> Result<RunLock> {
        paths.ensure_run_dirs(run_id)?;
        let path = lock_path(paths, run_id);
        let me = std::process::id();
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                write!(f, "{me}").with_context(|| format!("write {}", path.display()))?;
                return Ok(RunLock { path, pid: me });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| format!("open {}", path.display()));
            }
        }

        if let Some(owner) = read_owner_pid(&path) {
            if pid_alive(owner) {
                return Err(OrchestratorBusy {
                    run_id: run_id.to_string(),
                    owner_pid: owner,
                }
                .into());
            }
        }

        // Stale lock (dead or unreadable owner): take it over in place.
        fs::write(&path, me.to_string())
            .with_context(|| format!("take over {}", path.display()))?;
        let _ = crate::events::append(
            paths,
            run_id,
            &crate::events::Event::info(format!("orchestrator lock taken over by pid {me}")),
        );
        Ok(RunLock { path, pid: me })
    }

    pub fn owner(paths: &SparPaths, run_id: &str) -> Option<u32> {
        read_owner_pid(&lock_path(paths, run_id))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if read_owner_pid(&self.path) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_fresh_succeeds() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let lock = RunLock::acquire(&paths, "r1").unwrap();
        assert_eq!(lock.pid, std::process::id());
        assert_eq!(RunLock::owner(&paths, "r1"), Some(std::process::id()));
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
        assert_eq!(RunLock::owner(&paths, "r1"), None);
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
        assert_eq!(RunLock::owner(&paths, "r1"), Some(std::process::id()));
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
            RunLock::owner(&paths, "r1"),
            Some(i32::MAX as u32),
            "drop must not delete a lock a takeover handed to someone else"
        );
    }
}
