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
/// Slot children are spawned and reaped on other threads of this process, so a
/// thread blocked in a socket read can draw EINTR from `SIGCHLD` (default
/// disposition: no handler runs, the syscall still fails) instead of the
/// caller's timeout. ureq surfaces that as a typed `Error::Io` with kind
/// `Interrupted`, and re-sending a POST after one is wrong: the server may
/// already have accepted or answered it, billing another completion. Hold this
/// guard for the whole HTTP call instead, and the kernel delivers the signal
/// to a thread that leaves it unblocked. The previous mask is restored on drop.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The SIGCHLD mask must hold for the whole HTTP call and restore after.
    /// `TcpStream::read` retries `EINTR` internally, so `Interrupted` can never
    /// surface from a plain read on any kernel: no read shape separates masked
    /// from unmasked here (a directed SIGCHLD storm and a funneled real-child
    /// storm both leave reads undisturbed on this kernel). The guard therefore
    /// pins the mechanism itself: the mask is set inside the guard and restored
    /// on drop, real child exits keep SIGCHLD pending for the duration of a
    /// masked read, and the read still gets its byte. Removing the guard fails
    /// the set-inside assertion on the first iteration.
    #[test]
    fn masked_read_survives_sigchld_churn() {
        use std::io::{Read as _, Write};
        use std::sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        };

        fn sigchld_blocked() -> bool {
            unsafe {
                let mut cur: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut cur);
                libc::sigismember(&cur, libc::SIGCHLD) == 1
            }
        }
        fn sigchld_pending() -> bool {
            unsafe {
                let mut pending: libc::sigset_t = std::mem::zeroed();
                libc::sigpending(&mut pending);
                libc::sigismember(&pending, libc::SIGCHLD) == 1
            }
        }

        assert!(!sigchld_blocked(), "test thread starts unmasked");
        for i in 0..2 {
            let running = Arc::new(AtomicBool::new(true));
            let reaped = Arc::new(AtomicU64::new(0));
            let pending_hits = Arc::new(AtomicU64::new(0));
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = {
                let count = reaped.clone();
                std::thread::spawn(move || {
                    let _mask = block_sigchld();
                    let (mut conn, _) = listener.accept().unwrap();
                    // The storm is a precondition, not a post-assertion: the byte
                    // goes out only after the churn is proven real, so a loaded
                    // machine delays the read instead of failing it. The bound
                    // keeps a dead spawner a loud failure instead of a hang.
                    let mut waited = 0;
                    while count.load(Ordering::Relaxed) <= 500 && waited < 3000 {
                        std::thread::sleep(Duration::from_millis(10));
                        waited += 1;
                    }
                    conn.write_all(b"x").unwrap();
                })
            };
            let churners: Vec<_> = (0..4)
                .map(|_| {
                    let (flag, count) = (running.clone(), reaped.clone());
                    std::thread::spawn(move || {
                        let _mask = block_sigchld();
                        while flag.load(Ordering::Relaxed) {
                            let _ = std::process::Command::new("/bin/true").status();
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            let sampler = {
                let (flag, hits) = (running.clone(), pending_hits.clone());
                std::thread::spawn(move || {
                    let _mask = block_sigchld();
                    while flag.load(Ordering::Relaxed) {
                        if sigchld_pending() {
                            hits.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            };
            let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            let _mask = block_sigchld();
            assert!(
                sigchld_blocked(),
                "guard must block SIGCHLD inside, iter {i}"
            );
            let mut buf = [0u8; 1];
            stream.read_exact(&mut buf).unwrap();
            running.store(false, Ordering::Relaxed);
            for churn in churners {
                churn.join().unwrap();
            }
            sampler.join().unwrap();
            server.join().unwrap();
            drop(_mask);
            assert!(
                !sigchld_blocked(),
                "guard must restore the mask on drop, iter {i}"
            );
            assert_eq!(buf, *b"x", "masked read still gets its byte, iter {i}");
            assert!(
                reaped.load(Ordering::Relaxed) > 500,
                "storm must be real, else the read proves nothing, iter {i}"
            );
            assert!(
                pending_hits.load(Ordering::Relaxed) > 0,
                "SIGCHLD must sit pending for the masked read, iter {i}"
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
