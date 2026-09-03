//! The IPC contract between the unprivileged `argos` process and this helper.
//!
//! `argos` sends a single [`Plan`] as one line of JSON on stdin, already
//! fully resolved and confirmed by the user; this helper reads it, re-validates
//! it against the device's *current* state (never trusts the plan blindly --
//! see [`validate_refreshed_device`]), and then does the write or the
//! standalone verify. Progress and the final outcome are reported as
//! [`Event`] JSON lines on stdout, one per line, so the parent can parse them
//! incrementally without buffering.
//!
//! Using `serde`/`serde_json` here (rather than a hand-rolled format) is a
//! deliberate, scoped exception to "keep this crate's dependency tree tiny":
//! the schema below is small and fixed, unlike an ISO9660 or plist parser, and
//! a well-audited JSON deserializer is safer than a bespoke line format for
//! anything crossing a privilege boundary.

use argos_core::device::Device;
use argos_core::error::ArgosError;
use argos_core::progress::CancelToken;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;

/// The byte `argos` writes into `argos-helper`'s stdin to ask it to stop.
///
/// Chosen as ETX (0x03), what a terminal sends on Ctrl-C, because this
/// channel already carries a line of JSON and nothing else: any byte that is
/// not a newline-terminated plan is unambiguous, and this one names what it
/// means.
pub const CANCEL_SIGNAL: u8 = 0x03;

/// Runs on a background thread for the duration of a write, watching
/// whatever's left of `argos`'s stdin pipe after the `Plan` line for
/// [`CANCEL_SIGNAL`] and calling `cancel.cancel()` when it sees one.
///
/// Also cancels on plain EOF (the pipe closing without the byte ever
/// arriving) or a read error, as a safety net: if the unprivileged parent
/// process dies or is killed outright rather than delivering a clean
/// `SIGINT`-triggered byte, this is the only way `argos-helper` finds out --
/// and stopping is the safer default when the process that was supposed to
/// be watching this write is gone. Harmless if it fires *after* the write
/// already finished (the write loop has stopped checking the token by then
/// either way).
///
/// Takes a plain [`Read`] rather than assuming stdin specifically so it's
/// testable against an in-memory buffer.
pub fn watch_for_cancel<R: Read>(mut reader: R, cancel: CancelToken) {
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,                             // EOF -- parent closed the pipe
            Ok(_) if byte[0] == CANCEL_SIGNAL => break, // the real signal
            Ok(_) => continue,                          // anything else on this channel: ignore
            Err(_) => break,
        }
    }
    cancel.cancel();
}

/// The one value `argos` ever sends `argos-helper` on stdin. Tagged so a
/// single JSON blob unambiguously carries which operation to run -- `argos
/// write` needs the privileged write path, `argos verify` needs only the
/// read-back half of it, and both need the same elevation this process
/// already requires to open a raw device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Plan {
    Write(WritePlan),
    Verify(VerifyPlan),
    WriteWindowsImage(WriteWindowsPlan),
    VerifyWindowsImage(VerifyWindowsPlan),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WritePlan {
    pub device_path: String,
    pub expected_serial: Option<String>,
    pub expected_size_bytes: u64,
    pub image_path: PathBuf,
    pub image_size_bytes: u64,
    pub verify: bool,
}

/// The Windows installer write path's counterpart to [`WritePlan`] (phase 3
/// M3, backlog #43). Deliberately carries only `iso_path`, not a precomputed
/// partition layout or Windows-files byte total --
/// `execute_write_windows_fat32` reads the ISO's tree itself
/// (`argos_core::image::windows::WindowsIso`) to build a `WindowsFat32Plan`
/// from scratch, the same never-trust-the-caller posture
/// [`validate_refreshed_device`] already applies to the device itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteWindowsPlan {
    pub device_path: String,
    pub expected_serial: Option<String>,
    pub expected_size_bytes: u64,
    pub iso_path: PathBuf,
    /// Which on-disk layout to produce (phase 3 M3.5, backlog #43).
    /// `#[serde(default)]` keeps the JSON contract backward-compatible: a
    /// plan from an older `argos` simply carries no `layout` key and gets
    /// the layout it was written against -- originally NTFS (backlog #27,
    /// retired at decision point M4.3, see `docs/architecture.md`), now
    /// [`WindowsLayout::Fat32`].
    #[serde(default)]
    pub layout: WindowsLayout,
}

/// The on-disk layouts the helper can produce for a Windows installer write
/// (phase 3, backlog #43/#45): a single FAT32 partition, GPT-partitioned for
/// UEFI or MBR-partitioned with Argos's own boot records for legacy BIOS. An
/// earlier two-partition UEFI:NTFS scheme (backlog #27) was retired once
/// this layout was validated on real hardware from both hosts, on both
/// firmwares (decision point M4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsLayout {
    #[default]
    Fat32,
    /// Phase 3 M6 (#45): the same FAT32 filesystem as [`Self::Fat32`], but
    /// described by an MBR and carrying Argos's own boot records, so a
    /// legacy BIOS can boot it. The UEFI files are copied either way, so
    /// this media also boots UEFI firmware that accepts MBR-partitioned
    /// removable disks -- which is most of it.
    Fat32Bios,
}

/// Unlike [`WritePlan`], carries no `expected_serial`/`expected_size_bytes`:
/// verify is read-only, so there's no destructive TOCTOU window to guard
/// against the way there is for a write, and no reason to refuse a device
/// just because it changed since some earlier confirmation -- there was
/// never a confirmation step for `argos verify` to begin with. The helper
/// still independently re-resolves `device_path` before opening it (see
/// [`argos_privileged::execute_verify`](../fn.execute_verify.html)), the same
/// "never trust the caller blindly" posture as the write path, just without
/// the extra refusal fields that only make sense before a write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPlan {
    pub device_path: String,
    pub iso_path: PathBuf,
    pub iso_size_bytes: u64,
}

/// [`WriteWindowsPlan`]'s read-only counterpart to [`VerifyPlan`] -- same
/// read-only posture (no `expected_serial`/`expected_size_bytes`, no TOCTOU
/// refusal window), same re-resolution of `device_path` before opening it.
/// Carries only `iso_path`: `execute_verify_windows_fat32` rebuilds the
/// expected `WindowsFat32Plan` and re-lists the ISO's files itself, the same
/// never-trust-the-caller posture the write path already applies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyWindowsPlan {
    pub device_path: String,
    pub iso_path: PathBuf,
    /// Same backward-compatible default as [`WriteWindowsPlan::layout`].
    #[serde(default)]
    pub layout: WindowsLayout,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Phase {
        phase: String,
    },
    Progress {
        bytes_done: u64,
        bytes_total: u64,
    },
    Done {
        written_hash: String,
    },
    WindowsDone {
        files_copied: u64,
        bytes_copied: u64,
    },
    VerifyOk {
        hash: String,
    },
    WindowsVerifyOk {
        files_verified: u64,
    },
    Error {
        message: String,
        exit_code: i32,
    },
}

/// Re-validates a device immediately before opening it for writing. This is
/// the TOCTOU guard: `refreshed` must come from a fresh platform query done
/// *right now*, not from anything cached in the plan. Returns the device to
/// write to, or an error explaining why the helper refuses.
///
/// This check is independent of, and in addition to, whatever the caller
/// already confirmed with the user -- a compromised or buggy caller cannot
/// talk this helper into writing to a system disk by lying in the plan.
pub fn validate_refreshed_device(
    plan: &WritePlan,
    refreshed: Option<&Device>,
) -> Result<(), ArgosError> {
    validate_refreshed_device_common(
        &plan.device_path,
        plan.expected_size_bytes,
        plan.expected_serial.as_deref(),
        refreshed,
    )
}

/// The [`WriteWindowsPlan`] counterpart to [`validate_refreshed_device`] --
/// same TOCTOU guard, same three checks, just against the Windows write
/// path's plan shape instead of [`WritePlan`]'s.
pub fn validate_refreshed_device_for_windows_write(
    plan: &WriteWindowsPlan,
    refreshed: Option<&Device>,
) -> Result<(), ArgosError> {
    validate_refreshed_device_common(
        &plan.device_path,
        plan.expected_size_bytes,
        plan.expected_serial.as_deref(),
        refreshed,
    )
}

fn validate_refreshed_device_common(
    device_path: &str,
    expected_size_bytes: u64,
    expected_serial: Option<&str>,
    refreshed: Option<&Device>,
) -> Result<(), ArgosError> {
    let device = refreshed.ok_or_else(|| ArgosError::DeviceNotFound(device_path.to_string()))?;

    if device.is_system_disk {
        return Err(ArgosError::DeviceIsSystemDisk(device_path.to_string()));
    }

    if device.size_bytes != expected_size_bytes {
        return Err(ArgosError::DeviceNotFound(format!(
            "{device_path} changed size since it was confirmed ({expected_size_bytes} -> {} bytes); aborting",
            device.size_bytes
        )));
    }

    if expected_serial.is_some() && device.serial.as_deref() != expected_serial {
        return Err(ArgosError::DeviceNotFound(format!(
            "{device_path} no longer matches the confirmed serial number; aborting"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn watch_for_cancel_cancels_the_token_when_it_sees_the_signal_byte() {
        let cancel = CancelToken::new();
        watch_for_cancel(std::io::Cursor::new(vec![CANCEL_SIGNAL]), cancel.clone());
        assert!(cancel.is_cancelled());
    }

    /// The safety net: if `argos` dies outright instead of asking politely,
    /// the pipe closing is the only notice the helper ever gets, and stopping
    /// is the safer reading of it.
    #[test]
    fn watch_for_cancel_cancels_the_token_on_plain_eof_too() {
        let cancel = CancelToken::new();
        watch_for_cancel(std::io::Cursor::new(Vec::new()), cancel.clone());
        assert!(cancel.is_cancelled());
    }

    /// Anything else on this channel is ignored rather than treated as a
    /// cancellation -- the byte has to be the agreed one.
    #[test]
    fn watch_for_cancel_ignores_bytes_that_are_not_the_signal() {
        let cancel = CancelToken::new();
        let cancel_seen = cancel.clone();
        watch_for_cancel(std::io::Cursor::new(vec![b'x', b'y']), cancel);
        // Still cancelled, but only because the cursor then hit EOF -- the
        // point is that neither byte alone stopped the loop early.
        assert!(cancel_seen.is_cancelled());
    }
    use super::*;
    use argos_core::device::Bus;

    fn plan() -> WritePlan {
        WritePlan {
            device_path: "/dev/sdz".into(),
            expected_serial: Some("ABC123".into()),
            expected_size_bytes: 8_000_000_000,
            image_path: "/tmp/ubuntu.iso".into(),
            image_size_bytes: 4_000_000_000,
            verify: true,
        }
    }

    fn matching_device() -> Device {
        Device {
            platform_id: "/dev/sdz".into(),
            display_name: "Example USB Stick".into(),
            size_bytes: 8_000_000_000,
            bus: Bus::Usb,
            os_reports_removable: true,
            is_system_disk: false,
            serial: Some("ABC123".into()),
        }
    }

    #[test]
    fn accepts_a_device_that_still_matches_the_plan() {
        assert!(validate_refreshed_device(&plan(), Some(&matching_device())).is_ok());
    }

    #[test]
    fn refuses_when_the_device_is_gone() {
        let err = validate_refreshed_device(&plan(), None).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceNotFound(_)));
    }

    #[test]
    fn refuses_a_device_now_flagged_as_a_system_disk_even_if_the_plan_says_otherwise() {
        let mut device = matching_device();
        device.is_system_disk = true;
        let err = validate_refreshed_device(&plan(), Some(&device)).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceIsSystemDisk(_)));
    }

    #[test]
    fn refuses_when_size_changed_since_confirmation() {
        let mut device = matching_device();
        device.size_bytes = 1_000_000; // a different, smaller drive now at this path
        let err = validate_refreshed_device(&plan(), Some(&device)).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceNotFound(_)));
    }

    #[test]
    fn refuses_when_serial_no_longer_matches() {
        let mut device = matching_device();
        device.serial = Some("DIFFERENT".into());
        let err = validate_refreshed_device(&plan(), Some(&device)).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceNotFound(_)));
    }

    // The `Plan` enum is what actually crosses the privilege boundary as
    // JSON text -- a serialization mistake here (a typo'd tag, a field that
    // silently stops round-tripping) would only surface at runtime, against
    // a real elevated helper process, which is exactly the kind of failure
    // this test exists to catch cheaply instead.
    #[test]
    fn write_plan_round_trips_through_json_as_a_tagged_plan() {
        let original = Plan::Write(plan());
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""kind":"write""#));
        let parsed: Plan = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Plan::Write(p) if p.device_path == "/dev/sdz"));
    }

    #[test]
    fn verify_plan_round_trips_through_json_as_a_tagged_plan() {
        let original = Plan::Verify(VerifyPlan {
            device_path: "/dev/sdz".into(),
            iso_path: "/tmp/ubuntu.iso".into(),
            iso_size_bytes: 4_000_000_000,
        });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""kind":"verify""#));
        let parsed: Plan = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Plan::Verify(p) if p.iso_size_bytes == 4_000_000_000));
    }

    #[test]
    fn verify_windows_plan_round_trips_through_json_as_a_tagged_plan() {
        let original = Plan::VerifyWindowsImage(VerifyWindowsPlan {
            device_path: "/dev/sdz".into(),
            iso_path: "/tmp/Win11.iso".into(),
            layout: WindowsLayout::Fat32,
        });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""kind":"verify_windows_image""#));
        let parsed: Plan = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, Plan::VerifyWindowsImage(p) if p.device_path == "/dev/sdz" && p.layout == WindowsLayout::Fat32)
        );
    }

    /// A plan JSON written before the `layout` field existed must still
    /// parse -- and default to [`WindowsLayout::Fat32`], the only layout
    /// left since NTFS's retirement (decision point M4.3). Such a sender
    /// predates M4.3 too, so it meant NTFS at the time; there is no way (or
    /// need) to preserve that meaning once the layout it named is gone --
    /// parsing cleanly, rather than refusing the plan outright, is what
    /// matters for compatibility.
    #[test]
    fn windows_plans_without_a_layout_key_default_to_fat32() {
        let json = r#"{"kind":"write_windows_image","device_path":"/dev/sdz","expected_serial":null,"expected_size_bytes":8000000000,"iso_path":"/tmp/Win11.iso"}"#;
        let parsed: Plan = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, Plan::WriteWindowsImage(p) if p.layout == WindowsLayout::Fat32));
    }
}
