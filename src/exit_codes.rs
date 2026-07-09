use std::process::ExitCode as StdExitCode;

/// Stable exit codes for outer agents to branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Success = 0,
    Failure = 1,
    /// Waiting on human gate (plan approve, winner confirm, ship confirm)
    HumanGate = 2,
    /// Stuck / escalated after policy chain
    Stuck = 3,
    /// Provider quota exhausted / paused
    Quota = 4,
}

impl From<ExitCode> for StdExitCode {
    fn from(code: ExitCode) -> Self {
        StdExitCode::from(code as u8)
    }
}

impl ExitCode {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}
