//! The IPC contract between the unprivileged `argos` process and this helper.
//!
//! `argos` sends a single [`WritePlan`] as one line of JSON on stdin, already
//! fully resolved and confirmed by the user; this helper reads it, re-validates
//! it against the device's *current* state (never trusts the plan blindly --
//! see [`validate_refreshed_device`]), and then does the write. Progress and
//! the final outcome are reported as [`Event`] JSON lines on stdout, one per
//! line, so the parent can parse them incrementally without buffering.
//!
//! Using `serde`/`serde_json` here (rather than a hand-rolled format) is a
//! deliberate, scoped exception to "keep this crate's dependency tree tiny":
//! the schema below is small and fixed, unlike an ISO9660 or plist parser, and
//! a well-audited JSON deserializer is safer than a bespoke line format for
//! anything crossing a privilege boundary.

use argos_core::device::Device;
use argos_core::error::ArgosError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WritePlan {
    pub device_path: String,
    pub expected_serial: Option<String>,
    pub expected_size_bytes: u64,
    pub image_path: PathBuf,
    pub image_size_bytes: u64,
    pub verify: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Phase { phase: String },
    Progress { bytes_done: u64, bytes_total: u64 },
    Done { written_hash: String },
    Error { message: String, exit_code: i32 },
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
    let device = refreshed.ok_or_else(|| ArgosError::DeviceNotFound(plan.device_path.clone()))?;

    if device.is_system_disk {
        return Err(ArgosError::DeviceIsSystemDisk(plan.device_path.clone()));
    }

    if device.size_bytes != plan.expected_size_bytes {
        return Err(ArgosError::DeviceNotFound(format!(
            "{} changed size since it was confirmed ({} -> {} bytes); aborting",
            plan.device_path, plan.expected_size_bytes, device.size_bytes
        )));
    }

    if plan.expected_serial.is_some() && device.serial != plan.expected_serial {
        return Err(ArgosError::DeviceNotFound(format!(
            "{} no longer matches the confirmed serial number; aborting",
            plan.device_path
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
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
}
