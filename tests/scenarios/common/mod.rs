//! Every scenario spawn of `spar` goes through here. HOME, XDG_CONFIG_HOME and SPAR_HOME
//! point at a per-test temp dir, so no test reads the operator's user config (a shared
//! quota path, ssh remotes) or registry, and `SPAR_TEST_ISOLATED=1` makes the binary
//! ignore `[quota] shared_path` and `[registry] remotes` even if one leaked through.
//! A file sets its own SPAR_HOME after this when it needs a known one.
#![allow(dead_code)]

use std::path::PathBuf;

thread_local! {
    static ROOT: tempfile::TempDir = tempfile::tempdir().expect("test home");
}

/// This test's isolated root (one per test thread).
pub fn test_root() -> PathBuf {
    ROOT.with(|d| d.path().to_path_buf())
}

pub fn isolate(cmd: &mut std::process::Command) -> &mut std::process::Command {
    let root = test_root();
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".config")).expect("test home");
    cmd.env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("SPAR_HOME", root.join("spar-home"))
        .env("SPAR_TEST_ISOLATED", "1")
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
}

/// An isolated `std::process::Command` for the spar binary.
pub fn spar_std() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_spar"));
    isolate(&mut cmd);
    cmd
}

/// An isolated `assert_cmd::Command` for the spar binary.
pub fn spar() -> assert_cmd::Command {
    assert_cmd::Command::from_std(spar_std())
}
