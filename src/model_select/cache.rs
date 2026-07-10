//! On-disk cache for vals snapshots under `~/.spar/cache/vals/`.

use crate::model_select::vals::BenchSnapshot;
use crate::registry;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct CacheMeta {
    pub snapshot: BenchSnapshot,
    pub mtime_secs: u64,
}

pub fn cache_path(bench: &str) -> PathBuf {
    registry::spar_home()
        .join("cache")
        .join("vals")
        .join(format!("{bench}.json"))
}

pub fn load_cached(path: &Path) -> Result<Option<CacheMeta>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read cache {}", path.display()))?;
    let snapshot: BenchSnapshot =
        serde_json::from_str(&text).with_context(|| format!("parse cache {}", path.display()))?;
    let mtime_secs = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(Some(CacheMeta {
        snapshot,
        mtime_secs,
    }))
}

pub fn save_cached(path: &Path, snap: &BenchSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(snap)?;
    std::fs::write(path, text).with_context(|| format!("write cache {}", path.display()))?;
    Ok(())
}

pub fn cache_age_secs(meta: &CacheMeta) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(meta.mtime_secs)
}
