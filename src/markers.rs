use crate::paths::SwarmPaths;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};

pub fn write_marker(paths: &SwarmPaths, run_id: &str, name: &str, body: &str) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let p = paths.marker(run_id, name);
    std::fs::write(&p, body).with_context(|| format!("write marker {}", p.display()))?;
    Ok(())
}

pub fn marker_exists(paths: &SwarmPaths, run_id: &str, name: &str) -> bool {
    paths.marker(run_id, name).is_file()
}

pub fn write_done(paths: &SwarmPaths, run_id: &str, slot_id: &str) -> Result<()> {
    write_marker(paths, run_id, &format!("{slot_id}.done"), "ok\n")
}

pub fn write_failed(paths: &SwarmPaths, run_id: &str, slot_id: &str, reason: &str) -> Result<()> {
    write_marker(paths, run_id, &format!("{slot_id}.failed"), reason)
}

/// Wait until an artifact file is non-empty.
#[allow(dead_code)]
pub fn wait_for_artifact(
    paths: &SwarmPaths,
    run_id: &str,
    name: &str,
    timeout: Duration,
) -> Result<bool> {
    let path = paths.artifact(run_id, name);
    let start = Instant::now();
    let poll = Duration::from_millis(200);
    loop {
        if path.is_file() {
            let meta = std::fs::metadata(&path)?;
            if meta.len() > 0 {
                return Ok(true);
            }
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn markers_roundtrip() {
        let tmp = tempdir().unwrap();
        let paths = SwarmPaths::new(tmp.path());
        write_done(&paths, "r1", "slot-a").unwrap();
        assert!(marker_exists(&paths, "r1", "slot-a.done"));
    }
}
