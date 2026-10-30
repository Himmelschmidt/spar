//! State shared by every box on a shared filesystem (O141, O142): the fleet quota file
//! and per-slot heartbeat leases, under `[quota] shared_path` from the user config.
//!
//! A plain directory, not a service: NFS, sshfs or syncthing all work, with their own
//! caveats (see `skills/core.md`). Every touch of the shared path runs on a helper
//! thread with a deadline (`with_timeout`), because a hung NFS mount blocks the caller
//! in the kernel forever. A missing directory or a deadline miss falls back to the
//! per-project state, warns once, and keeps the share off for this process until the
//! retry interval passes.
use crate::config::UserConfig;
use crate::paths::SparPaths;
use crate::process::PidToken;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How often a live process rewrites its slot leases.
const LEASE_REFRESH_SECS: u64 = 30;
/// A lease not refreshed for this long no longer counts, whatever host wrote it.
const LEASE_TTL_SECS: i64 = 120;
/// Clock disagreement tolerated between boxes. Another host's lease is deleted only
/// once it is this much past the TTL, so a box whose clock runs fast never reaps a live
/// peer's lease. The quota merge uses the same allowance.
pub const CLOCK_SKEW_SECS: i64 = 60;
/// The longest any shared-path operation may take before the share is treated as hung.
const SHARED_TIMEOUT: Duration = Duration::from_secs(4);
/// After a hang, how long this process leaves the share alone before trying again.
const SHARED_RETRY: Duration = Duration::from_secs(60);

/// The shared directory and the account labels that key it.
#[derive(Clone)]
pub struct Shared {
    pub dir: PathBuf,
    accounts: HashMap<String, String>,
}

static WARNED: AtomicBool = AtomicBool::new(false);
static DOWN_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

pub fn warn_once(msg: &str) {
    if !WARNED.swap(true, Ordering::SeqCst) {
        eprintln!("warning: {msg}; using this project's quota state only");
    }
}

/// Run `f` (a shared-path operation) on a helper thread and wait at most
/// `SHARED_TIMEOUT`. On a miss the thread is abandoned (it may be stuck in the kernel on
/// a dead mount), the share is marked down for `SHARED_RETRY`, and `None` comes back.
/// Also `None` without running `f` while the share is marked down.
pub fn with_timeout<T: Send + 'static>(
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    {
        let mut down = DOWN_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
        match *down {
            Some(until) if Instant::now() < until => return None,
            Some(_) => *down = None,
            None => {}
        }
    }
    match within(SHARED_TIMEOUT, f) {
        Some(v) => Some(v),
        None => {
            *DOWN_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + SHARED_RETRY);
            warn_once(&format!(
                "shared quota path did not answer within {}s ({what})",
                SHARED_TIMEOUT.as_secs()
            ));
            None
        }
    }
}

/// `f`'s result if it finishes inside `deadline`; the thread is left behind otherwise.
fn within<T: Send + 'static>(
    deadline: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(deadline).ok()
}

impl Shared {
    /// `None` when no shared path is configured, or when it is configured but missing
    /// or hung. A missing directory is never created: on an unmounted share that would
    /// write the fleet's state to a local disk nobody else reads.
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
        let probe = dir.clone();
        if !with_timeout("stat", move || probe.is_dir())? {
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

/// A temp-file name no other box or process writing the same shared directory can pick.
pub fn unique_tmp(path: &Path) -> PathBuf {
    let host = crate::util::sanitize_slot(&this_host());
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    path.with_extension(format!("tmp.{host}.{}.{}", std::process::id(), &nonce[..8]))
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = unique_tmp(path);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Which machine this is: hostname plus `/etc/machine-id`, so two boxes cloned from one
/// image with the same hostname are still told apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Machine {
    host: String,
    #[serde(default)]
    machine_id: Option<String>,
}

fn this_host() -> String {
    crate::util::hostname().unwrap_or_else(|| "localhost".into())
}

fn this_machine() -> Machine {
    Machine {
        host: this_host(),
        machine_id: std::fs::read_to_string("/etc/machine-id")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

impl Machine {
    /// Same box: same hostname, and the same machine id when both sides know one.
    fn same(&self, other: &Machine) -> bool {
        self.host == other.host
            && match (&self.machine_id, &other.machine_id) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    #[serde(flatten)]
    machine: Machine,
    pid: u32,
    #[serde(default)]
    starttime: Option<u64>,
    project_root: PathBuf,
    run_id: String,
    slot_id: String,
    account: String,
    heartbeat_at: DateTime<Utc>,
}

enum LeaseState {
    Live,
    /// Past the TTL but inside the skew margin: not counted, not deleted.
    Expired,
    /// Provably gone: delete it.
    Dead,
}

impl LeaseRecord {
    fn state(&self, me: &Machine, now: DateTime<Utc>) -> LeaseState {
        let local = self.machine.same(me);
        if local {
            let alive = PidToken {
                pid: self.pid,
                starttime: self.starttime,
            }
            .alive();
            return if alive {
                LeaseState::Live
            } else {
                LeaseState::Dead
            };
        }
        let age = now - self.heartbeat_at;
        if age > chrono::Duration::seconds(LEASE_TTL_SECS + CLOCK_SKEW_SECS) {
            LeaseState::Dead
        } else if age > chrono::Duration::seconds(LEASE_TTL_SECS) {
            LeaseState::Expired
        } else {
            LeaseState::Live
        }
    }
}

static LIVE: Mutex<Vec<(PathBuf, LeaseRecord)>> = Mutex::new(Vec::new());
static REFRESHER: OnceLock<()> = OnceLock::new();

/// Rewrite every live lease's heartbeat. The list is copied out first so a hung write
/// never holds the lock a finishing dispatch needs to drop its lease.
fn refresh_live() {
    let now = Utc::now();
    let leases: Vec<(PathBuf, LeaseRecord)> = {
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        for (_, rec) in live.iter_mut() {
            rec.heartbeat_at = now;
        }
        live.clone()
    };
    for (path, rec) in leases {
        let Ok(text) = serde_json::to_string(&rec) else {
            continue;
        };
        with_timeout("refresh slot lease", move || write_atomic(&path, &text));
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
        let machine = this_machine();
        let pid = std::process::id();
        let name =
            crate::util::sanitize_slot(&format!("{}-{pid}-{run_id}-{slot_id}", machine.host));
        let rec = LeaseRecord {
            pid,
            starttime: PidToken::capture(pid).starttime,
            project_root: paths.project_root.clone(),
            run_id: run_id.to_string(),
            slot_id: slot_id.to_string(),
            account: shared.account_key(&storage),
            heartbeat_at: Utc::now(),
            machine,
        };
        let dir = shared.slots_dir();
        let path = dir.join(format!("{name}.json"));
        let text = serde_json::to_string(&rec).ok()?;
        let target = path.clone();
        let written = with_timeout("write slot lease", move || {
            std::fs::create_dir_all(&dir).and_then(|_| write_atomic(&target, &text))
        })?;
        if let Err(e) = written {
            warn_once(&format!("{}: {e}", path.display()));
            return None;
        }
        LIVE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((path.clone(), rec));
        REFRESHER.get_or_init(|| {
            std::thread::spawn(|| loop {
                std::thread::sleep(Duration::from_secs(LEASE_REFRESH_SECS));
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
        let path = self.0.clone();
        with_timeout("remove slot lease", move || std::fs::remove_file(&path));
    }
}

/// Live slots on other boxes and other projects, by local storage key. This project's
/// own slots on this host are left out: the daemon already counts them from run state.
/// Dead leases are deleted on the way past. Never call this under a lock another
/// process waits on: it touches the shared path.
pub fn remote_slots(paths: &SparPaths) -> HashMap<String, u32> {
    let Some(shared) = Shared::open() else {
        return HashMap::new();
    };
    let root = paths.project_root.clone();
    with_timeout("count slot leases", move || {
        count_remote_slots(&shared, &root, &this_machine(), Utc::now())
    })
    .unwrap_or_default()
}

fn count_remote_slots(
    shared: &Shared,
    project_root: &Path,
    me: &Machine,
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
        match rec.state(me, now) {
            LeaseState::Dead => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            LeaseState::Expired => continue,
            LeaseState::Live => {}
        }
        if rec.machine.same(me) && rec.project_root == project_root {
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

    fn machine(host: &str, id: Option<&str>) -> Machine {
        Machine {
            host: host.into(),
            machine_id: id.map(String::from),
        }
    }

    fn rec(m: Machine, root: &str, account: &str, age_secs: i64) -> LeaseRecord {
        LeaseRecord {
            machine: m,
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
    fn remote_slots_count_other_boxes_and_projects_and_expire_only_provably_dead_leases() {
        let tmp = tempdir().unwrap();
        let shared = Shared::at(tmp.path(), &[("cli:claude", "acct-a")]);
        let me = machine("box1", Some("m1"));
        let other = || machine("box2", Some("m2"));
        lease(tmp.path(), "a", &rec(other(), "/p", "cli:claude#acct-a", 5));
        lease(
            tmp.path(),
            "b",
            &rec(machine("box3", None), "/q", "cli:claude#acct-a", 5),
        );
        lease(tmp.path(), "c", &rec(other(), "/p", "cli:claude#acct-b", 5));
        lease(tmp.path(), "d", &rec(other(), "/p", "cli:codex", 5));
        lease(
            tmp.path(),
            "clone",
            &rec(machine("box1", Some("m9")), "/mine", "cli:claude#acct-a", 5),
        );
        let mut own = rec(me.clone(), "/mine", "cli:claude#acct-a", 5);
        own.pid = std::process::id();
        lease(tmp.path(), "own", &own);
        lease(
            tmp.path(),
            "skewed",
            &rec(other(), "/p", "cli:claude#acct-a", LEASE_TTL_SECS + 10),
        );
        lease(
            tmp.path(),
            "old",
            &rec(
                other(),
                "/p",
                "cli:claude#acct-a",
                LEASE_TTL_SECS + CLOCK_SKEW_SECS + 10,
            ),
        );
        let mut here = rec(me.clone(), "/other", "cli:claude#acct-a", 5);
        here.pid = std::process::id();
        here.starttime = PidToken::capture(std::process::id()).starttime;
        lease(tmp.path(), "e", &here);
        let mut dead = rec(me.clone(), "/other", "cli:claude#acct-a", 5);
        dead.pid = u32::MAX - 7;
        lease(tmp.path(), "dead", &dead);

        let counts = count_remote_slots(&shared, Path::new("/mine"), &me, Utc::now());
        assert_eq!(
            counts.get("cli:claude"),
            Some(&4),
            "box2, box3, the same-hostname clone and this host's other project: {counts:?}"
        );
        assert_eq!(counts.get("cli:codex"), Some(&1));
        assert!(
            tmp.path().join("slots/skewed.json").exists(),
            "inside the skew margin a peer's lease is ignored, not deleted"
        );
        assert!(!tmp.path().join("slots/old.json").exists());
        assert!(!tmp.path().join("slots/dead.json").exists());
        assert!(tmp.path().join("slots/own.json").exists());
    }

    #[test]
    fn a_slot_lease_is_written_refreshed_and_removed_on_drop() {
        let tmp = tempdir().unwrap();
        let shared = tmp.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "[quota]\nshared_path = \"{}\"\n[accounts]\n\"cli:claude\" = \"acct-a\"\n",
                shared.display()
            ),
        )
        .unwrap();
        crate::config::set_test_user_config(Some(cfg));
        let paths = SparPaths::new(tmp.path().join("proj"));
        let lease = SlotLease::start(&paths, "r1", "implementer", "cli:claude@opus").unwrap();
        crate::config::set_test_user_config(None);

        let read = || -> LeaseRecord {
            serde_json::from_str(&std::fs::read_to_string(&lease.0).unwrap()).unwrap()
        };
        let first = read();
        assert_eq!(first.account, "cli:claude#acct-a");
        assert_eq!(first.pid, std::process::id());
        assert!(lease.0.starts_with(shared.join("slots")));

        std::thread::sleep(Duration::from_millis(20));
        refresh_live();
        assert!(
            read().heartbeat_at > first.heartbeat_at,
            "refresh moves the heartbeat"
        );

        let path = lease.0.clone();
        drop(lease);
        assert!(!path.exists(), "drop removes the lease");
        assert!(LIVE.lock().unwrap().iter().all(|(p, _)| p != &path));
    }

    #[test]
    fn a_hung_operation_is_abandoned_at_the_deadline() {
        let start = Instant::now();
        let got = within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(5));
            1
        });
        assert_eq!(got, None);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the caller never waits on a hang"
        );
        assert_eq!(within(Duration::from_secs(2), || 7), Some(7));
    }

    #[test]
    fn unique_tmp_names_differ_per_call() {
        let p = Path::new("/x/quota.json");
        assert_ne!(unique_tmp(p), unique_tmp(p));
    }
}
