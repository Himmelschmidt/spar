pub mod bwrap;

use crate::config::IsolationMode;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Optionally wrap a command in bubblewrap when isolation requests it.
pub fn maybe_wrap(
    isolation: IsolationMode,
    cwd: &Path,
    program: &Path,
    args: &[String],
) -> (PathBuf, Vec<String>) {
    if !matches!(isolation, IsolationMode::WorktreeBwrap) {
        return (program.to_path_buf(), args.to_vec());
    }
    if which::which("bwrap").is_err() {
        return (program.to_path_buf(), args.to_vec());
    }
    let wrapped = bwrap::wrap_command(cwd, program, args);
    let program = PathBuf::from(wrapped.get_program());
    let args = wrapped
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    (program, args)
}

pub fn bwrap_available() -> bool {
    which::which("bwrap").is_ok()
}

#[allow(dead_code)]
pub fn apply_to_command(isolation: IsolationMode, cwd: &Path, cmd: Command) -> Command {
    if !matches!(isolation, IsolationMode::WorktreeBwrap) || !bwrap_available() {
        return cmd;
    }
    let program = PathBuf::from(cmd.get_program());
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    bwrap::wrap_command(cwd, &program, &args)
}
