//! Exposes the `argos-helper` <-> `argos` IPC contract, device re-validation,
//! and the actual write/verify orchestration as a library. Depending on this
//! crate's library target does **not** pull in anything privileged: `main.rs`
//! (the `argos-helper` binary) is the only thing here that ever runs as root,
//! and Cargo links a dependent only against the library target.
//!
//! Splitting [`execute`] out of `main.rs` also means the loop-device
//! integration tests (backlog E9) can call it directly against a real device
//! node, without spawning the compiled binary and piping JSON through stdin.

pub mod partition_io;
pub mod platform_select;
pub mod protocol;
pub mod windows_fat32;

use argos_core::error::{ArgosError, Result};
use argos_core::image::checksum::sha256_stream;
use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_core::write::dd_mode;
use argos_core::{preflight, verify};
use argos_platform::{PlatformOps, WrittenBytes};
use protocol::{VerifyPlan, WritePlan};
use std::fs::{File, OpenOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Re-validates the target device, unmounts it, writes `plan.image_path` to
/// `plan.device_path`, and (unless `plan.verify` is false) reads it back to
/// confirm it matches. This is everything `argos-helper` does; `main.rs` only
/// adds stdin/stdout JSON framing around it.
pub fn execute(
    plan: &WritePlan,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<String> {
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

    // Resolved from the `Device` before the binding below shadows it with the
    // open file handle, and read *before* the first byte is written: the
    // counter is cumulative for the device's whole lifetime, so only the
    // delta from here means anything. A platform with no counter leaves this
    // `None`, and progress falls back to bytes handed to the OS.
    let counter = platform.written_bytes(&device);
    let baseline = counter.as_ref().and_then(WrittenBytes::read).unwrap_or(0);
    let committed = || {
        counter
            .as_ref()?
            .read()
            .map(|now| now.saturating_sub(baseline))
    };

    let mut image = File::open(&plan.image_path)?;
    let mut device = OpenOptions::new().write(true).open(&plan.device_path)?;

    // No cancellation source is wired up yet -- see main.rs's module doc comment.
    let written_hash = dd_mode::write_stream(
        &mut image,
        &mut device,
        plan.image_size_bytes,
        progress,
        cancel,
        &committed,
    )?;
    flush_write(progress, &device, plan.image_size_bytes, &committed)?;
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

/// Forces what [`dd_mode::write_stream`] handed the OS out to the physical
/// device before anything downstream trusts it's actually there.
///
/// Not `device.flush()`: that's a no-op default on `std::fs::File`, which
/// doesn't override it. A plain `write()` only queues bytes in the page
/// cache -- the kernel can report success in milliseconds while it's still
/// flushing gigabytes to a slow USB stick in the background, so without a
/// real `fsync` here, "Done" (and, if verify runs next, the read-back it
/// does) can both happen against data that isn't actually on the device
/// yet. See [`partition_io::sync_device`]'s doc comment for the macOS
/// device-node quirk this already had to work around once, on the other
/// write path.
///
/// This is where most of a fast-looking write's real time is spent, so it
/// samples `committed` on a second thread while `fsync` blocks: the phase
/// that used to be a motionless bar (the whole reason it was given a name in
/// 1.5.4) now advances through the flush with the device's own numbers. The
/// sampler is deliberately the only thing that thread does -- no I/O against
/// the device it's reporting on -- so it can't slow down the flush it's
/// watching.
fn flush_write(
    progress: &dyn ProgressSink,
    device: &File,
    total_size: u64,
    committed: &(impl Fn() -> Option<u64> + Sync),
) -> Result<()> {
    progress.on_phase(Phase::Flushing);

    let flushing = AtomicBool::new(true);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while flushing.load(Ordering::Relaxed) {
                if let Some(bytes) = committed() {
                    progress.on_progress(bytes.min(total_size), total_size);
                }
                std::thread::sleep(SAMPLE_INTERVAL);
            }
        });

        let result = partition_io::sync_device(device).map_err(ArgosError::Io);
        flushing.store(false, Ordering::Relaxed);
        result
    })
}

/// How often the flush phase asks the device how far it has got. Frequent
/// enough to look live, rare enough that reading a sysfs file is nothing next
/// to the flush itself.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Ejects a device the helper has just finished writing.
///
/// This lives on the privileged side because that is where the privilege is.
/// `argos-cli` used to do it right after the helper exited, and on a stock
/// Ubuntu that fails: `/dev/sdX` is `root:disk` mode 0660 and the user
/// running `argos` is typically not in `disk`, so `eject` couldn't even open
/// the device -- "cannot open /dev/sdg: Permission denied", on the very
/// device the write had just succeeded on (reported from real hardware, on a
/// user in `adm cdrom sudo dip plugdev users lpadmin` and no `disk`).
///
/// Re-resolves the device rather than trusting the path alone, the same
/// posture [`execute`] takes -- though without
/// [`protocol::validate_refreshed_device`]'s full refusal, since ejecting is
/// not destructive and the write it follows has already been verified.
pub fn eject_device(device_path: &str) -> Result<()> {
    let platform = platform_select::current_platform();
    let device = platform
        .refresh(device_path, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(device_path.to_string()))?;
    platform.eject(&device)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records the sequence of phases it's told about -- enough to assert
    /// ordering without pulling in a real progress bar.
    #[derive(Default)]
    struct RecordingProgress {
        phases: Mutex<Vec<Phase>>,
        progress: Mutex<Vec<(u64, u64)>>,
    }

    impl ProgressSink for RecordingProgress {
        fn on_phase(&self, phase: Phase) {
            self.phases.lock().unwrap().push(phase);
        }

        fn on_progress(&self, bytes_done: u64, bytes_total: u64) {
            self.progress
                .lock()
                .unwrap()
                .push((bytes_done, bytes_total));
        }
    }

    /// The regression this guards: a write that only ever called the
    /// no-op `File::flush()` reported success (and, with verify on, read
    /// back) without ever forcing the OS to actually commit the bytes.
    /// Reported from real hardware -- a fast USB write that looked done
    /// instantly, then stalled during verify behind the kernel's own
    /// background writeback of everything `write_stream` had only queued.
    #[test]
    fn flushing_a_write_reports_the_flushing_phase_and_syncs() {
        let progress = Arc::new(RecordingProgress::default());
        let device = tempfile::tempfile().unwrap();

        flush_write(progress.as_ref(), &device, 100, &|| None).unwrap();

        let phases = progress.phases.lock().unwrap();
        assert_eq!(*phases, vec![Phase::Flushing]);
    }

    /// The flush is where a fast-looking write actually spends its time, and
    /// it used to show a motionless bar for all of it. It now samples the
    /// device's own counter while `fsync` blocks -- so a flush that takes
    /// long enough to sample at all reports what the device has taken, and
    /// never more than the write's own total.
    #[test]
    fn flushing_reports_progress_from_the_device_counter() {
        let progress = Arc::new(RecordingProgress::default());
        // Big enough that the flush outlasts at least one sample interval,
        // rather than racing the sampler's first tick.
        let mut device = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut device, &vec![0u8; 32 * 1024 * 1024]).unwrap();

        // Runs away past the total, the way a device-wide counter can when
        // something else touches the same disk.
        flush_write(progress.as_ref(), &device, 1000, &|| Some(u64::MAX)).unwrap();

        let reported = progress.progress.lock().unwrap();
        assert!(
            reported.iter().all(|&(done, total)| done <= total),
            "progress must never exceed the total: {reported:?}"
        );
    }
}
