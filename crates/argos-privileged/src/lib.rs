//! Exposes the `argos-helper` <-> `argos` IPC contract, device re-validation,
//! and the actual write/verify orchestration as a library. Depending on this
//! crate's library target does **not** pull in anything privileged: `main.rs`
//! (the `argos-helper` binary) is the only thing here that ever runs as root,
//! and Cargo links a dependent only against the library target.
//!
//! Splitting [`execute`] out of `main.rs` also means the loop-device
//! integration tests (backlog E9) can call it directly against a real device
//! node, without spawning the compiled binary and piping JSON through stdin.

pub mod platform_select;
pub mod protocol;
pub mod windows;

use argos_core::error::{ArgosError, Result};
use argos_core::image::checksum::sha256_stream;
use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_core::write::dd_mode;
use argos_core::{preflight, verify};
use argos_platform::PlatformOps;
use protocol::{VerifyPlan, WritePlan};
use std::fs::{File, OpenOptions};
use std::io::Write;

/// Re-validates the target device, unmounts it, writes `plan.image_path` to
/// `plan.device_path`, and (unless `plan.verify` is false) reads it back to
/// confirm it matches. This is everything `argos-helper` does; `main.rs` only
/// adds stdin/stdout JSON framing around it.
pub fn execute(plan: &WritePlan, progress: &dyn ProgressSink) -> Result<String> {
    let platform = platform_select::current_platform();

    let refreshed = platform.refresh(&plan.device_path, plan.expected_serial.as_deref())?;
    protocol::validate_refreshed_device(plan, refreshed.as_ref())?;
    let device = refreshed
        .expect("validate_refreshed_device already returned Ok, so refreshed must be Some");

    preflight::check_capacity(
        &plan.device_path,
        plan.expected_size_bytes,
        &plan.image_path,
        plan.image_size_bytes,
    )?;

    // The safe-open precondition (backlog #20): unmounting here, right before
    // opening the device, rather than in the unprivileged `argos-cli` before
    // elevating, keeps the window between "unmounted" and "opened for write"
    // as small as possible, and works regardless of whether unmounting itself
    // needs privilege on a given platform (this process already has it
    // either way). A no-op, not an error, when nothing was mounted --
    // confirmed against a fresh, filesystem-less test device on macOS.
    progress.on_phase(Phase::Unmounting);
    platform.unmount(&device)?;

    let mut image = File::open(&plan.image_path)?;
    let mut device = OpenOptions::new().write(true).open(&plan.device_path)?;

    // No cancellation source is wired up yet -- see main.rs's module doc comment.
    let cancel = CancelToken::new();
    let written_hash = dd_mode::write_stream(
        &mut image,
        &mut device,
        plan.image_size_bytes,
        progress,
        &cancel,
    )?;
    device.flush().map_err(ArgosError::Io)?;
    drop(device);

    if plan.verify {
        let mut device_for_read = File::open(&plan.device_path)?;
        verify::verify_written_image(
            &mut device_for_read,
            plan.image_size_bytes,
            &written_hash,
            progress,
        )?;
    }

    Ok(written_hash)
}

/// Re-runs verification against a device without writing again: hashes
/// `plan.iso_path` (the `Checksumming` phase -- unlike [`execute`], nothing
/// upstream of this call has already produced that hash, since there was no
/// write), then reads `plan.iso_size_bytes` back off `plan.device_path` and
/// compares (the `Verifying` phase, inside
/// [`verify::verify_written_image`]). Returns the matched hash on success.
///
/// Independently re-resolves `plan.device_path` first, the same
/// never-trust-the-caller posture [`execute`] uses -- see
/// [`protocol::VerifyPlan`]'s doc comment for why this doesn't reuse
/// [`protocol::validate_refreshed_device`]'s fuller TOCTOU refusal: a
/// read-only operation has no destructive window to guard.
pub fn execute_verify(plan: &VerifyPlan, progress: &dyn ProgressSink) -> Result<String> {
    let platform = platform_select::current_platform();
    platform
        .refresh(&plan.device_path, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(plan.device_path.clone()))?;

    progress.on_phase(Phase::Checksumming);
    let mut iso = File::open(&plan.iso_path)?;
    let expected_hash = sha256_stream(&mut iso, |_| {}).map_err(ArgosError::Io)?;

    let mut device = File::open(&plan.device_path)?;
    verify::verify_written_image(&mut device, plan.iso_size_bytes, &expected_hash, progress)?;

    Ok(expected_hash)
}
