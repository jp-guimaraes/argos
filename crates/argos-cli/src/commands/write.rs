use argos_core::error::{ArgosError, Result};
use std::path::PathBuf;

// Fields are unused until the run() body below is implemented (E4-E8); keeping
// the real shape now avoids an API break once it is.
#[allow(dead_code)]
pub struct Args {
    pub iso: PathBuf,
    pub device: String,
    pub no_verify: bool,
    pub i_know_what_im_doing: bool,
}

/// **Not implemented yet.** The full flow (classify ISO, run preflight checks,
/// require the user to retype the device path, invoke `argos-helper`, show
/// progress, verify) is backlog epics E4-E8. `argos list` (E8.1) ships first so
/// there is a real, tested vertical slice before wiring up anything destructive.
pub fn run(_args: Args) -> Result<()> {
    Err(ArgosError::NotImplemented(
        "argos write (backlog epics E4-E8)",
    ))
}
