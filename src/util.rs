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
        debug_assert_eq!(rc, 0);
        SigchldGuard {
            saved,
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for SigchldGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.saved, std::ptr::null_mut());
        }
    }
}

/// A test env var that restores its previous value (or absence) on drop, so a
/// loopback URL a test installs never leaks into a later test in the binary.
#[cfg(test)]
pub struct EnvRestore {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl EnvRestore {
    pub fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvRestore { key, prev }
    }
}

#[cfg(test)]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
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

    /// Child churn on another thread must not interrupt a masked blocking read.
    /// The churn spawns and reaps real children, which is what delivers SIGCHLD
    /// to this process; with the mask held the kernel hands it to a thread that
    /// leaves it unblocked, so the read waits out the slow server instead of
    /// failing with `Interrupted`.
    #[test]
    fn masked_read_survives_sigchld_churn() {
        use std::io::{Read, Write};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(800));
            conn.write_all(b"slow answer").unwrap();
        });

        let running = Arc::new(AtomicBool::new(true));
        let churn_flag = running.clone();
        let churn = std::thread::spawn(move || {
            while churn_flag.load(Ordering::Relaxed) {
                let _ = std::process::Command::new("/bin/true").status();
            }
        });

        let _mask = block_sigchld();
        let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    assert_ne!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted,
                        "a masked read must not draw EINTR from child churn"
                    );
                    panic!("masked read failed: {e:?}");
                }
            }
        }
        assert_eq!(buf, b"slow answer");
        running.store(false, Ordering::Relaxed);
        churn.join().unwrap();
        server.join().unwrap();
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
