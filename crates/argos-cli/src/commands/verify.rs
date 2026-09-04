//! `argos verify`: re-runs post-write verification against a device without
//! writing again (backlog E8). Reuses the same `argos-helper` elevation path
//! as `argos write` (`super::helper`) since reading raw device bytes needs
//! the same privilege writing does, on every platform this project supports
//! -- confirmed empirically on macOS, where even reading an external disk's
//! device node fails with a plain permission error as a normal user.
//!
//! Unlike `write`, there is no destructive confirmation prompt here: verify
//! never modifies the device, so there's nothing for a hasty Enter to ruin.

use super::helper;
use crate::platform_select::current_platform;
use argos_core::error::{ArgosError, Result};
use argos_core::image;
use argos_platform::PlatformOps;
use argos_privileged::protocol::{Plan, VerifyPlan, VerifyWindowsPlan, WindowsLayout};
use std::path::PathBuf;

pub struct Args {
    pub device: String,
    pub iso: PathBuf,
    pub layout: WindowsLayout,
}

pub fn run(mut args: Args) -> Result<()> {
    let platform = current_platform();

    // See helper::canonicalize_iso_path's doc comment: a relative ISO path
    // cannot survive to the Plan sent across the privilege boundary, since
    // argos-helper resolves it against its own (elevator-dependent) working
    // directory, not the shell's.
    args.iso = helper::canonicalize_iso_path(&args.iso)?;

    // Resolved unprivileged, before elevating, purely so a typo'd device
    // path fails fast with a clear error instead of only after a sudo/pkexec
    // prompt -- the same reason `argos write` resolves the device first.
    // The result itself isn't otherwise used: neither `VerifyPlan` nor
    // `VerifyWindowsPlan` carries serial/size fields for `argos-helper` to
    // re-check against (see their doc comments), since a read-only
    // operation has no destructive TOCTOU window to guard.
    platform
        .refresh(&args.device, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(args.device.clone()))?;

    // Same DD-mode-first, then-Windows-installer-shape classification order
    // `argos write` uses -- see its doc comment for why.
    if image::classify(&args.iso)?.is_writable_as_dd_image() {
        return run_dd_verify(&args);
    }
    if image::windows::classify(&args.iso)?.is_windows_installer_iso() {
        return run_windows_verify(&args);
    }
    Err(ArgosError::UnsupportedIso(args.iso.clone()))
}

fn run_dd_verify(args: &Args) -> Result<()> {
    let iso_size_bytes = std::fs::metadata(&args.iso)?.len();

    let plan = VerifyPlan {
        device_path: args.device.clone(),
        iso_path: args.iso.clone(),
        iso_size_bytes,
    };

    let hash = helper::run_plan(&Plan::Verify(plan))?;
    println!("Verified. SHA-256: {hash} matches {}.", args.iso.display());
    Ok(())
}

/// The Windows-write counterpart to [`run_dd_verify`] above (phase 3 M3,
/// backlog #43): same shape, against `Plan::VerifyWindowsImage` instead.
/// Runs on both hosts -- the FAT32 layout is the only one Argos produces
/// since NTFS's retirement (decision point M4.3, see docs/architecture.md),
/// and it needs no host-specific gate.
fn run_windows_verify(args: &Args) -> Result<()> {
    let plan = VerifyWindowsPlan {
        device_path: args.device.clone(),
        iso_path: args.iso.clone(),
        layout: args.layout,
    };

    let outcome = helper::run_plan(&Plan::VerifyWindowsImage(plan))?;
    println!("Verified. {outcome}.");
    Ok(())
}
