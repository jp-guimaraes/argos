use argos_core::error::{ArgosError, Result};
use std::path::PathBuf;

// Fields are unused until the run() body below is implemented (E6); keeping the
// real shape now avoids an API break once it is.
#[allow(dead_code)]
pub struct Args {
    pub device: String,
    pub iso: PathBuf,
}

/// **Not implemented yet** -- backlog epic E6.
pub fn run(_args: Args) -> Result<()> {
    Err(ArgosError::NotImplemented("argos verify (backlog epic E6)"))
}
