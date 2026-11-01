//! Bare `spar` with no subcommand prints help and exits 0 (O145: the TUI is gone).

mod common;
use predicates::prelude::*;
use tempfile::tempdir;

fn spar_cmd() -> assert_cmd::Command {
    let mut c = common::spar();
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

#[test]
fn bare_spar_prints_help_and_exits_zero() {
    let tmp = tempdir().unwrap();
    spar_cmd()
        .current_dir(tmp.path())
        .assert()
        .code(0)
        .stdout(predicate::str::contains("Usage: spar"))
        .stdout(predicate::str::contains("Commands:"));
    assert!(
        !tmp.path().join(".spar").exists(),
        "printing help must not create project state"
    );
}

#[test]
fn dashboard_is_no_longer_a_subcommand() {
    let tmp = tempdir().unwrap();
    spar_cmd()
        .current_dir(tmp.path())
        .arg("dashboard")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}
