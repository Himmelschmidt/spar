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
            // The one moment spar knows an orchestrator died: settle the slots it left
            // at `running` before anything reads them (O49). Best-effort: a run with no
            // state file yet is the normal case on a first acquire.
            //
            // `Nobody` is asserted, not observed. `reclaimed` is only `Some` when the
            // previous owner left its pid behind, which `Drop` clears, so it died hard;
            // and we hold the flock, so it cannot be driving anything. The lock file has
            // already been overwritten with *our* live pid by this point, so observing
            // it here would answer "an orchestrator is alive" and skip the demotion.
            if let Ok(mut state) = crate::state::RunState::load(paths, run_id) {
                let _ = state.reconcile_and_save(
                    paths,
                    crate::state::RunOwner::Nobody,
                    crate::state::ORPHANED_SLOT,
                );
            }
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

    /// Reclaiming a crashed orchestrator's lock is the one moment spar knows a run lost
    /// its driver, and it must settle the slots that driver left at `running`. The trap
    /// is ordering: `acquire` stamps its **own live pid** into the lock before it can
    /// reconcile, so a reconcile that re-derives liveness from that file finds itself,
    /// concludes an orchestrator is alive, and skips the demotion entirely. Against that
    /// ordering this test sees `running`. The 86 corpus slots with no terminal marker
    /// have nothing else that can settle them.
    #[test]
    fn reclaiming_a_dead_orchestrator_settles_the_slots_it_left_running() {
        use crate::state::{RunState, SlotStatus};
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "crashed",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.phase = crate::state::Phase::Dispatch;
        let mut slot =
            crate::executor::init_slot("impl", "cli:claude", crate::state::SlotRole::Implementer);
        slot.status = SlotStatus::Running;
        state.slots.push(slot);
        state.save(&paths).unwrap();
        // No terminal marker and no `.pid`: exactly the 86-slot population.
        fs::write(lock_path(&paths, "crashed"), (i32::MAX as u32).to_string()).unwrap();

        let _lock = RunLock::acquire(&paths, "crashed").unwrap();

        let on_disk = RunState::load(&paths, "crashed").unwrap();
        assert_eq!(
            on_disk.slots[0].status,
            SlotStatus::Failed,
            "the reclaim must settle the dead orchestrator's slots"
        );
        assert_eq!(
            on_disk.slots[0].error.as_deref(),
            Some(crate::state::ORPHANED_SLOT)
        );
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
