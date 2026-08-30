//! `argos write`: the full destructive flow. Orchestration (classify, preflight,
//! confirm, invoke `argos-helper`, render progress) lives here rather than in
//! `argos-core` for now; if a GUI needs the same flow later, it's a small lift
//! to extract this into a shared, UI-agnostic function. Talking to
//! `argos-helper` itself is shared with `argos verify` -- see
//! `super::helper`.

use super::helper;
use crate::platform_select::current_platform;
use argos_core::device::Device;
use argos_core::error::{ArgosError, Result};
use argos_core::image;
use argos_core::preflight;
use argos_platform::PlatformOps;
use argos_privileged::protocol::{Plan, WritePlan};
use std::io::Write;
use std::path::PathBuf;

pub struct Args {
    pub iso: PathBuf,
    pub device: String,
    pub no_verify: bool,
    pub no_eject: bool,
    pub i_know_what_im_doing: bool,
}

pub fn run(args: Args) -> Result<()> {
    let platform = current_platform();

    let device = platform
        .refresh(&args.device, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(args.device.clone()))?;

    check_device_is_offerable(&device, args.i_know_what_im_doing)?;

    let classification = image::classify(&args.iso)?;
    if !classification.is_writable_as_dd_image() {
        return Err(ArgosError::UnsupportedIso(args.iso.clone()));
    }

    let image_size_bytes = std::fs::metadata(&args.iso)?.len();
    preflight::check_capacity(
        &device.platform_id,
        device.size_bytes,
        &args.iso,
        image_size_bytes,
    )?;

    if let Some(backing_device_id) = platform.backing_device_of(&args.iso)? {
        preflight::check_no_source_target_collision(
            &args.iso,
            &backing_device_id,
            &device.platform_id,
        )?;
    }

    confirm_or_abort(&device, &args.iso, image_size_bytes)?;

    let plan = WritePlan {
        device_path: device.platform_id.clone(),
        expected_serial: device.serial.clone(),
        expected_size_bytes: device.size_bytes,
        image_path: args.iso.clone(),
        image_size_bytes,
        verify: !args.no_verify,
    };

    let written_hash = helper::run_plan(&Plan::Write(plan))?;
    println!("Done. SHA-256: {written_hash}");

    if !args.no_eject {
        eject_best_effort(&platform, &device);
    }

    Ok(())
}

/// Ejects the just-written device. Never fails the command over this: the
/// write itself already succeeded (that's what matters and what the exit
/// code reflects), and both current `PlatformOps` backends already treat the
/// underlying `eject`/`diskutil eject` call as best-effort internally --
/// this only adds the same posture at the one call site that skipped it
/// until now, plus a message either way so the user isn't left guessing
/// whether it's safe to unplug.
fn eject_best_effort(platform: &impl PlatformOps, device: &Device) {
    match platform.eject(device) {
        Ok(()) => println!("Ejected {}. Safe to unplug.", device.platform_id),
        Err(err) => eprintln!(
            "warning: could not eject {}: {err} (the write itself succeeded -- eject it manually before unplugging)",
            device.platform_id
        ),
    }
}

/// The non-negotiable part of the safety gate: a system disk is refused
/// unconditionally, no flag overrides it. A disk the OS doesn't report as
/// removable can only proceed with `--i-know-what-im-doing` -- and still has
/// to survive the retyped-path confirmation below.
fn check_device_is_offerable(device: &Device, i_know_what_im_doing: bool) -> Result<()> {
    if device.is_system_disk {
        return Err(ArgosError::DeviceIsSystemDisk(device.platform_id.clone()));
    }
    if !device.is_safe_to_write() && !i_know_what_im_doing {
        return Err(ArgosError::DeviceNotRemovable(device.platform_id.clone()));
    }
    Ok(())
}

/// Requires the user to retype the exact device path -- not just "y/N" -- so a
/// hasty Enter can't confirm the wrong drive.
fn confirm_or_abort(device: &Device, iso: &std::path::Path, image_size_bytes: u64) -> Result<()> {
    println!("About to overwrite:");
    println!(
        "  device:  {} ({})",
        device.platform_id, device.display_name
    );
    println!("  size:    {}", helper::human_size(device.size_bytes));
    println!(
        "  serial:  {}",
        device.serial.as_deref().unwrap_or("unknown")
    );
    println!("  image:   {}", iso.display());
    println!("  image size: {}", helper::human_size(image_size_bytes));
    println!();
    println!(
        "This will PERMANENTLY ERASE all data on {}.",
        device.platform_id
    );
    print!("Type the device path ({}) to confirm: ", device.platform_id);
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if input.trim() != device.platform_id {
        println!("Confirmation did not match; aborting. Nothing was written.");
        return Err(ArgosError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::device::Bus;

    fn usb_stick() -> Device {
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

    // The single most important test in this project: no flag, typo, or code
    // path may talk `check_device_is_offerable` into accepting a system disk.
    #[test]
    fn never_offers_a_system_disk_even_with_i_know_what_im_doing() {
        let mut device = usb_stick();
        device.is_system_disk = true;
        let err = check_device_is_offerable(&device, true).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceIsSystemDisk(_)));
    }

    #[test]
    fn offers_a_plain_usb_stick_by_default() {
        assert!(check_device_is_offerable(&usb_stick(), false).is_ok());
    }

    #[test]
    fn refuses_a_non_removable_disk_without_the_override_flag() {
        let mut device = usb_stick();
        device.os_reports_removable = false;
        let err = check_device_is_offerable(&device, false).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceNotRemovable(_)));
    }

    #[test]
    fn accepts_a_non_removable_non_system_disk_with_the_override_flag() {
        let mut device = usb_stick();
        device.os_reports_removable = false;
        assert!(check_device_is_offerable(&device, true).is_ok());
    }
}
