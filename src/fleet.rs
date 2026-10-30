//! State shared by every box on a shared filesystem (O141, O142): the fleet quota file
//! and per-slot heartbeat leases, under `[quota] shared_path` from the user config.
//!
//! A plain directory, not a service: NFS, sshfs or syncthing all work, with their own
//! caveats (see `skills/core.md`). When the directory is missing (an unmounted share)
//! everything here falls back to the per-project state and warns once per process.
use crate::config::UserConfig;
use crate::paths::SparPaths;
use crate::process::PidToken;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// How often a live process rewrites its slot leases.
const LEASE_REFRESH_SECS: u64 = 30;
/// A lease not refreshed for this long is stale and expires, whatever host wrote it.
const LEASE_TTL_SECS: i64 = 120;

/// The shared directory and the account labels that key it.
pub struct Shared {
    pub dir: PathBuf,
    accounts: HashMap<String, String>,
}

static WARNED: AtomicBool = AtomicBool::new(false);

pub fn warn_once(msg: &str) {
    if !WARNED.swap(true, Ordering::SeqCst) {
        eprintln!("warning: {msg}; using this project's quota state only");
    }
}

impl Shared {
    /// `None` when no shared path is configured, or when it is configured but missing.
    /// A missing directory is never created: on an unmounted share that would write the
    /// fleet's state to a local disk nobody else reads.
    pub fn open() -> Option<Self> {
        let user = match UserConfig::load() {
            Ok(u) => u,
            Err(e) => {
                warn_once(&format!("{e:#}"));
                return None;
            }
        };
        let raw = user.quota.shared_path.trim();
        if raw.is_empty() {
            return None;
        }
        let dir = crate::util::expand_home(raw);
        if !dir.is_dir() {
            warn_once(&format!(
                "[quota] shared_path {} is unavailable",
                dir.display()
            ));
            return None;
        }
        let accounts = user
            .accounts
            .into_iter()
            .map(|(k, v)| (crate::quota::normalize_key(&k), v))
            .collect();
        Some(Self { dir, accounts })
    }

    #[cfg(test)]
    pub fn at(dir: &Path, accounts: &[(&str, &str)]) -> Self {
        Self {
            dir: dir.to_path_buf(),
            accounts: accounts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    pub fn quota_file(&self) -> PathBuf {
        self.dir.join("quota.json")
    }

    pub fn quota_lock(&self) -> PathBuf {
        self.dir.join("quota.lock")
    }

    fn slots_dir(&self) -> PathBuf {
        self.dir.join("slots")
    }

    /// The fleet key for a storage key: the provider bucket plus the operator's account
    /// label, so two boxes logged into one account share a pause and two accounts on one
    /// provider do not.
    pub fn account_key(&self, storage_key: &str) -> String {
        match self.accounts.get(storage_key) {
            Some(label) if !label.is_empty() => format!("{storage_key}#{label}"),
            _ => storage_key.to_string(),
        }
    }

    /// The local storage key an account key maps to, when this box uses that account.
    pub fn storage_key_for(&self, account_key: &str) -> Option<String> {
        let storage = account_key.split('#').next()?;
        (self.account_key(storage) == account_key).then(|| storage.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    host: String,
    pid: u32,
    #[serde(default)]
    starttime: Option<u64>,
    project_root: PathBuf,
    run_id: String,
    slot_id: String,
    account: String,
    heartbeat_at: DateTime<Utc>,
}

impl LeaseRecord {
    fn stale(&self, host: &str, now: DateTime<Utc>) -> bool {
        if now - self.heartbeat_at > chrono::Duration::seconds(LEASE_TTL_SECS) {
            return true;
        }
        self.host == host
            && !PidToken {
                pid: self.pid,
                starttime: self.starttime,
            }
            .alive()
    }
}

fn this_host() -> String {
    crate::util::hostname().unwrap_or_else(|| "localhost".into())
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

static LIVE: Mutex<Vec<(PathBuf, LeaseRecord)>> = Mutex::new(Vec::new());
static REFRESHER: OnceLock<()> = OnceLock::new();

fn refresh_live() {
    let now = Utc::now();
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    for (path, rec) in live.iter_mut() {
        rec.heartbeat_at = now;
        if let Ok(text) = serde_json::to_string(rec) {
            let _ = write_atomic(path, &text);
        }
    }
}

/// One live slot's claim on its account, held for the dispatch's lifetime and refreshed
/// by a background thread, so `[daemon] max_slots_per_bucket` can count slots on every
/// box. Dropping it removes the file; a crash leaves a file that expires on its own.
pub struct SlotLease(PathBuf);

impl SlotLease {
    pub fn start(paths: &SparPaths, run_id: &str, slot_id: &str, provider: &str) -> Option<Self> {
        let shared = Shared::open()?;
        let storage = crate::provider_ref::ProviderRef::parse(provider)
            .ok()?
            .storage_key();
        let host = this_host();
        let pid = std::process::id();
        let rec = LeaseRecord {
            pid,
            starttime: PidToken::capture(pid).starttime,
            project_root: paths.project_root.clone(),
            run_id: run_id.to_string(),
            slot_id: slot_id.to_string(),
            account: shared.account_key(&storage),
            heartbeat_at: Utc::now(),
            host: host.clone(),
        };
        let dir = shared.slots_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn_once(&format!("{}: {e}", dir.display()));
            return None;
        }
        let name = crate::util::sanitize_slot(&format!("{host}-{pid}-{run_id}-{slot_id}"));
        let path = dir.join(format!("{name}.json"));
        if let Err(e) = write_atomic(&path, &serde_json::to_string(&rec).ok()?) {
            warn_once(&format!("{}: {e}", path.display()));
            return None;
        }
        LIVE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((path.clone(), rec));
        REFRESHER.get_or_init(|| {
            std::thread::spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(LEASE_REFRESH_SECS));
                refresh_live();
            });
        });
        Some(Self(path))
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        LIVE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(p, _)| p != &self.0);
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Live slots on other boxes and other projects, by local storage key. This project's
/// own slots on this host are left out: the daemon already counts them from run state.
/// Stale leases are deleted on the way past.
pub fn remote_slots(paths: &SparPaths) -> HashMap<String, u32> {
    let Some(shared) = Shared::open() else {
        return HashMap::new();
    };
    count_remote_slots(&shared, &paths.project_root, &this_host(), Utc::now())
}

fn count_remote_slots(
    shared: &Shared,
    project_root: &Path,
    host: &str,
    now: DateTime<Utc>,
) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(shared.slots_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(rec) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<LeaseRecord>(&t).ok())
        else {
            continue;
        };
        if rec.stale(host, now) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if rec.host == host && rec.project_root == project_root {
            continue;
        }
        if let Some(storage) = shared.storage_key_for(&rec.account) {
            *out.entry(storage).or_insert(0) += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn lease(dir: &Path, name: &str, rec: &LeaseRecord) {
        let slots = dir.join("slots");
        std::fs::create_dir_all(&slots).unwrap();
        std::fs::write(
            slots.join(format!("{name}.json")),
            serde_json::to_string(rec).unwrap(),
        )
        .unwrap();
    }

    fn rec(host: &str, root: &str, account: &str, age_secs: i64) -> LeaseRecord {
        LeaseRecord {
            host: host.into(),
            pid: 1,
            starttime: None,
            project_root: root.into(),
            run_id: "r1".into(),
            slot_id: "implementer".into(),
            account: account.into(),
            heartbeat_at: Utc::now() - chrono::Duration::seconds(age_secs),
        }
    }

    #[test]
    fn account_keys_carry_the_label_and_map_back_only_for_the_same_account() {
        let tmp = tempdir().unwrap();
        let a = Shared::at(tmp.path(), &[("cli:claude", "acct-a")]);
        assert_eq!(a.account_key("cli:claude"), "cli:claude#acct-a");
        assert_eq!(a.account_key("cli:codex"), "cli:codex");
        assert_eq!(
            a.storage_key_for("cli:claude#acct-a").as_deref(),
            Some("cli:claude")
        );
        assert_eq!(a.storage_key_for("cli:claude#acct-b"), None);
        assert_eq!(a.storage_key_for("cli:claude"), None);
        assert_eq!(a.storage_key_for("cli:codex").as_deref(), Some("cli:codex"));
    }

    #[test]
    fn remote_slots_count_other_boxes_and_projects_and_expire_stale_leases() {
        let tmp = tempdir().unwrap();
        let shared = Shared::at(tmp.path(), &[("cli:claude", "acct-a")]);
        let me = "box1";
        lease(tmp.path(), "a", &rec("box2", "/p", "cli:claude#acct-a", 5));
        lease(tmp.path(), "b", &rec("box3", "/q", "cli:claude#acct-a", 5));
        lease(tmp.path(), "c", &rec("box2", "/p", "cli:claude#acct-b", 5));
        lease(tmp.path(), "d", &rec("box2", "/p", "cli:codex", 5));
        let mut own = rec(me, "/mine", "cli:claude#acct-a", 5);
        own.pid = std::process::id();
        lease(tmp.path(), "own", &own);
        lease(
            tmp.path(),
            "old",
            &rec("box4", "/p", "cli:claude#acct-a", 600),
        );
        let mut same_host_other_project = rec(me, "/other", "cli:claude#acct-a", 5);
        same_host_other_project.pid = std::process::id();
        same_host_other_project.starttime = PidToken::capture(std::process::id()).starttime;
        lease(tmp.path(), "e", &same_host_other_project);
        let mut dead = rec(me, "/other", "cli:claude#acct-a", 5);
        dead.pid = u32::MAX - 7;
        lease(tmp.path(), "dead", &dead);

        let counts = count_remote_slots(&shared, Path::new("/mine"), me, Utc::now());
        assert_eq!(counts.get("cli:claude"), Some(&3), "{counts:?}");
        assert_eq!(counts.get("cli:codex"), Some(&1));
        assert!(
            !tmp.path().join("slots/old.json").exists(),
            "stale lease expired"
        );
        assert!(
            !tmp.path().join("slots/dead.json").exists(),
            "a dead local pid expires at once"
        );
        assert!(tmp.path().join("slots/own.json").exists());
    }
}
