use std::path::Path;
use std::process::Command;

/// Wrap `program args` so writes are confined to `cwd` (best-effort).
/// Read access to the rest of the filesystem remains for tools/toolchains.
pub fn wrap_command(cwd: &Path, program: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new("bwrap");
    cmd.arg("--die-with-parent");
    cmd.arg("--unshare-pid");
    // host root read-only
    cmd.arg("--ro-bind").arg("/").arg("/");
    // tmp
    cmd.arg("--tmpfs").arg("/tmp");
    // worktree writable
    cmd.arg("--bind").arg(cwd).arg(cwd);
    // keep /dev and /proc for tooling
    cmd.arg("--dev").arg("/dev");
    cmd.arg("--proc").arg("/proc");
    cmd.arg("--chdir").arg(cwd);
    cmd.arg("--");
    cmd.arg(program);
    for a in args {
        cmd.arg(a);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builds_bwrap_argv() {
        let cmd = wrap_command(
            Path::new("/tmp/wt"),
            Path::new("/usr/bin/echo"),
            &["hi".into()],
        );
        let prog = cmd.get_program().to_string_lossy();
        assert_eq!(prog, "bwrap");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--bind".into()));
        assert!(args.iter().any(|a| a == "/usr/bin/echo"));
        let _ = PathBuf::from("/tmp/wt");
    }
}
