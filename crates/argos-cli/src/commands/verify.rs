//! `argos verify`: re-runs post-write verification against a device without
//! writing again (backlog E8). Reuses the same elevation path as `argos
//! write` (`argos_session::spawn`) since reading raw device bytes needs the
//! same privilege writing does, on every platform this project supports --
//! confirmed empirically on macOS, where even reading an external disk's
//! device node fails with a plain permission error as a normal user.
//!
//! Unlike `write`, there is no destructive confirmation prompt here: verify
//! never modifies the device, so there's nothing for a hasty Enter to ruin.

use super::progress::Presenter;
use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::WindowsLayout;
use argos_session::{self as session, Outcome, VerifyRequest};
use std::path::PathBuf;

pub struct Args {
    pub device: String,
    pub iso: PathBuf,
    pub layout: WindowsLayout,
}

pub fn run(args: Args) -> Result<()> {
    let platform = session::current_platform();

    let prepared = session::prepare_verify(
        &platform,
        &VerifyRequest {
            device_id: args.device,
            iso: args.iso,
            layout: args.layout,
        },
    )?;

    let running = session::spawn(prepared.plan())?;
    let canceller = running.canceller();
    let _ = ctrlc::set_handler(move || canceller.cancel());

    let mut presenter = Presenter::new();
    let outcome = running.stream(&mut presenter)?;

    match outcome {
        Outcome::Verify { hash } => println!(
            "Verified. SHA-256: {hash} matches {}.",
            prepared.iso.display()
        ),
        Outcome::WindowsVerify { files_verified } => {
            println!("Verified. {files_verified} files verified.")
        }
        // `argos verify` only ever sends a verify plan, so the helper can
        // only ever answer with a verify outcome.
        other => {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "argos-helper answered a verify with {other:?}"
            ))))
        }
    }
    Ok(())
}
