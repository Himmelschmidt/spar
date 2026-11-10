use anyhow::{bail, Result};
use std::time::Duration;

/// Parse durations like `30s`, `5m`, `2h`, `1d`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration number in {s}"))?;
    let secs = match unit {
        "s" | "S" => n,
        "m" | "M" => n.saturating_mul(60),
        "h" | "H" => n.saturating_mul(3600),
        "d" | "D" => n.saturating_mul(86400),
        _ => bail!("unknown duration unit in {s} (use s/m/h/d)"),
    };
    Ok(Duration::from_secs(secs))
}

pub fn short_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Make a slot id safe to use as a filename, a path component, and a git refname.
/// Lowercases, maps every char outside `[a-z0-9_-]` to `-`, collapses dash runs,
/// and trims leading/trailing dashes.
pub fn sanitize_slot(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        let c = if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

pub fn expand_home(raw: &str) -> std::path::PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(raw)),
        None => std::path::PathBuf::from(raw),
    }
}

/// Block `SIGCHLD` on the calling thread until the guard drops.
///
/// Slot children are spawned and reaped on other threads of this process. At
/// `SIGCHLD`'s default disposition the kernel never interrupts a syscall for
/// one, but any installed handler (this process installs none today; a future
/// dependency may) turns every child reap into an EINTR on whatever blocking
/// read is in flight. ureq forwards that as a typed `Error::Io` with kind
/// `Interrupted` instead of retrying, and re-sending a POST after one is
/// wrong: the server may already have accepted or answered it, billing another
/// completion. Hold this guard for the whole HTTP call instead, and the kernel
/// delivers the signal to a thread that leaves it unblocked. The previous mask
/// is restored on drop.
pub struct SigchldGuard {
    saved: libc::sigset_t,
    _thread: std::marker::PhantomData<*const ()>,
}

pub fn block_sigchld() -> SigchldGuard {
    unsafe {
        let mut add: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut add);
        libc::sigaddset(&mut add, libc::SIGCHLD);
        let mut saved: libc::sigset_t = std::mem::zeroed();
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &add, &mut saved);
        assert_eq!(rc, 0);
        SigchldGuard {
            saved,
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for SigchldGuard {
    fn drop(&mut self) {
        unsafe {
            let rc = libc::pthread_sigmask(libc::SIG_SETMASK, &self.saved, std::ptr::null_mut());
            assert_eq!(rc, 0);
        }
    }
}

pub fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

pub fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

/// `0.8s`, `12s`, `1m02s`, `1h05m`.
pub fn fmt_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Test-only SIGCHLD churn and handler helpers, shared by the mask mechanism
/// test here and the HTTP call-site tests. Installing a handler is what makes
/// `SIGCHLD` interrupt a read at all; the handler is process-global while
/// installed, so every user restores it (via `InstalledHandler`'s drop) before
/// asserting.
#[cfg(test)]
pub(crate) mod sigchld_test {
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };

    extern "C" fn noop(_: libc::c_int) {}

    /// A no-op `SIGCHLD` handler, restored to the previous disposition on drop.
    pub(crate) struct InstalledHandler {
        old: libc::sigaction,
    }

    impl InstalledHandler {
        pub(crate) fn install() -> Self {
            unsafe {
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction = noop as *const () as usize;
                libc::sigemptyset(&mut new.sa_mask);
                new.sa_flags = 0;
                let mut old: libc::sigaction = std::mem::zeroed();
                let rc = libc::sigaction(libc::SIGCHLD, &new, &mut old);
                assert_eq!(rc, 0, "installing the SIGCHLD probe handler");
                Self { old }
            }
        }
    }

    impl Drop for InstalledHandler {
        fn drop(&mut self) {
            unsafe {
                let rc = libc::sigaction(libc::SIGCHLD, &self.old, std::ptr::null_mut());
                assert_eq!(rc, 0, "restoring the SIGCHLD disposition");
            }
        }
    }

    /// Short-lived child churn on unmasked threads: every reap delivers
    /// `SIGCHLD` to some thread that leaves it unblocked.
    pub(crate) struct Churn {
        running: Arc<AtomicBool>,
        reaped: Arc<AtomicU64>,
        handles: Vec<std::thread::JoinHandle<()>>,
    }

    impl Churn {
        pub(crate) fn spawn(threads: usize) -> Self {
            let running = Arc::new(AtomicBool::new(true));
            let reaped = Arc::new(AtomicU64::new(0));
            let handles = (0..threads)
                .map(|_| {
                    let (flag, count) = (running.clone(), reaped.clone());
                    std::thread::spawn(move || {
                        while flag.load(Ordering::Relaxed) {
                            let _ = std::process::Command::new("/bin/true").status();
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            Self {
                running,
                reaped,
                handles,
            }
        }

        pub(crate) fn reaped(&self) -> u64 {
            self.reaped.load(Ordering::Relaxed)
        }

        pub(crate) fn counter(&self) -> Arc<AtomicU64> {
            self.reaped.clone()
        }

        pub(crate) fn stop(self) {
            self.running.store(false, Ordering::Relaxed);
            for handle in self.handles {
                handle.join().unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SIGCHLD mask must hold for the whole HTTP call and restore after.
    /// `SIGCHLD` at its default disposition never interrupts a read, and even
    /// with a handler installed a child-reaping storm is the wrong signal
    /// source for a test: the kernel keeps delivering to one target thread, so
    /// the reading thread wins a lottery and sometimes observes none of
    /// hundreds of reaps. A helper thread instead directs `SIGCHLD` at this
    /// thread with `pthread_kill` while it blocks in `read`, deterministically
    /// and only then; the server holds the byte until the kills are out. One
    /// `read` per syscall surfaces the EINTR (`read_exact` would swallow it by
    /// retrying). The unmasked phase must observe it, the masked phase none.
    /// Removing `block_sigchld()` from the masked phase fails that assertion.
    #[test]
    fn masked_read_survives_sigchld_churn() {
        use std::io::{Read as _, Write};

        fn sigchld_blocked() -> bool {
            unsafe {
                let mut cur: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut cur);
                libc::sigismember(&cur, libc::SIGCHLD) == 1
            }
        }

        fn probe_once(masked: bool) -> (bool, u64) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let server = std::thread::spawn(move || {
                let (mut conn, _) = listener.accept().unwrap();
                done_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the kills go out before the byte");
                conn.write_all(b"x").unwrap();
            });
            let tid = unsafe { libc::pthread_self() };
            let killer = std::thread::spawn(move || {
                for _ in 0..5 {
                    std::thread::sleep(Duration::from_millis(20));
                    let rc = unsafe { libc::pthread_kill(tid, libc::SIGCHLD) };
                    assert_eq!(rc, 0, "directing SIGCHLD at the reading thread");
                }
                done_tx.send(()).unwrap();
            });
            let _mask: Option<SigchldGuard> = masked.then(block_sigchld);
            let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            let mut interrupted = 0u64;
            let mut buf = [0u8; 1];
            let mut got = false;
            for _ in 0..1000 {
                match stream.read(&mut buf) {
                    Ok(_) if buf[0] == b'x' => {
                        got = true;
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        interrupted += 1;
                    }
                    Err(e) => panic!("unexpected read error: {e:?}"),
                }
            }
            drop(_mask);
            killer.join().unwrap();
            server.join().unwrap();
            (got, interrupted)
        }

        let _handler = sigchld_test::InstalledHandler::install();
        assert!(!sigchld_blocked(), "test thread starts unmasked");
        for i in 0..2 {
            let (got, interrupted) = probe_once(false);
            assert!(got, "unmasked read still gets its byte, iter {i}");
            assert!(
                interrupted > 0,
                "the directed SIGCHLD must interrupt the unmasked read, else the masked \
                 phase proves nothing, iter {i}"
            );
            let (got, interrupted) = probe_once(true);
            assert!(got, "masked read still gets its byte, iter {i}");
            assert_eq!(
                interrupted, 0,
                "the mask must hold every SIGCHLD for the masked read, iter {i}"
            );
            assert!(
                !sigchld_blocked(),
                "guard must restore the mask on drop, iter {i}"
            );
        }
    }

    #[test]
    fn fmt_elapsed_scales_with_magnitude() {
        assert_eq!(fmt_elapsed(Duration::from_millis(800)), "0.8s");
        assert_eq!(fmt_elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(fmt_elapsed(Duration::from_secs(62)), "1m02s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3900)), "1h05m");
    }

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn strips_provider_ref_punctuation() {
        assert_eq!(sanitize_slot("cli:claude"), "cli-claude");
    }

    #[test]
    fn strips_model_slug_punctuation() {
        assert_eq!(
            sanitize_slot("cli:codex@anthropic/claude-opus-4.5"),
            "cli-codex-anthropic-claude-opus-4-5"
        );
    }

    #[test]
    fn collapses_runs() {
        assert_eq!(sanitize_slot("cli::claude"), "cli-claude");
        assert_eq!(sanitize_slot("a--b"), "a-b");
    }

    #[test]
    fn trims_edges() {
        assert_eq!(sanitize_slot("@claude@"), "claude");
    }

    #[test]
    fn lowercases() {
        assert_eq!(sanitize_slot("API:Claude"), "api-claude");
    }

    #[test]
    fn idempotent() {
        for case in [
            "cli:claude",
            "cli:codex@anthropic/claude-opus-4.5",
            "cli::claude",
            "@claude@",
            "review-0-a",
            "tencent/hy3:free",
        ] {
            let once = sanitize_slot(case);
            assert_eq!(sanitize_slot(&once), once, "not idempotent for {case}");
        }
    }

    #[test]
    fn preserves_safe_chars() {
        assert_eq!(sanitize_slot("review-0-a"), "review-0-a");
        assert_eq!(sanitize_slot("impl"), "impl");
        assert_eq!(sanitize_slot("test_author"), "test_author");
    }

    #[test]
    fn no_refname_hostile_chars() {
        let out = sanitize_slot("arena-0-cli:claude");
        assert_eq!(out, "arena-0-cli-claude");
        assert!(!out.contains([':', '/', '@', '.', ' ']));
    }
}
