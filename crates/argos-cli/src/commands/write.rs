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
use argos_core::partition::windows::{WindowsFat32Plan, WindowsPartitionPlan};
use argos_core::{image, preflight};
use argos_platform::PlatformOps;
use argos_privileged::protocol::{Plan, WindowsLayout, WritePlan, WriteWindowsPlan};
use argos_privileged::windows_fat32::{fat32_layout_for, plan_copy_actions, CopyAction};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Args {
    pub iso: PathBuf,
    pub device: String,
    pub no_verify: bool,
    pub no_eject: bool,
    pub i_know_what_im_doing: bool,
    pub layout: WindowsLayout,
}

pub fn run(args: Args) -> Result<()> {
    let platform = current_platform();

    let device = platform
        .refresh(&args.device, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(args.device.clone()))?;

    check_device_is_offerable(&device, args.i_know_what_im_doing)?;

    // Linux-hybrid ISOs (DD mode) first, since that's the overwhelmingly
    // common case; only try the Windows-installer shape (UDF/ISO9660, see
    // `image::windows`) once that's ruled out. A plain-data or corrupt image
    // matches neither and falls through to `UnsupportedIso` below.
    if image::classify(&args.iso)?.is_writable_as_dd_image() {
        return run_dd_write(&platform, &device, &args);
    }
    if image::windows::classify(&args.iso)?.is_windows_installer_iso() {
        return run_windows_write(&platform, &device, &args);
    }
    Err(ArgosError::UnsupportedIso(args.iso.clone()))
}

fn run_dd_write(platform: &impl PlatformOps, device: &Device, args: &Args) -> Result<()> {
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

    confirm_or_abort(device, &args.iso, image_size_bytes)?;

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
        eject_best_effort(platform, device);
    }

    Ok(())
}

/// The UEFI:NTFS write path (backlog #27, W5): same shape as
/// [`run_dd_write`] above (preflight, confirm, invoke `argos-helper`,
/// eject), but against `Plan::WriteWindowsImage` instead, and with a
/// two-partition layout to show instead of a single image size.
///
/// Unlike DD mode, there's no inline post-write verification here (no
/// `plan.verify` field on `WriteWindowsPlan` at all -- `--no-verify` simply
/// doesn't apply to a Windows write): `execute_write_windows_image` already
/// runs as one privileged elevation covering partition+format+copy, and
/// bolting a second, separate verify pass onto that would mean either a
/// second `pkexec`/`sudo` prompt or turning the one-shot IPC protocol into a
/// stateful one -- the same tradeoff `CONTRIBUTING.md`'s scoped exception
/// already declined for the copy step itself. `argos verify` covers it as
/// its own explicit step instead.
fn run_windows_write(platform: &impl PlatformOps, device: &Device, args: &Args) -> Result<()> {
    // Both branches compute their layout purely for the preflight + the
    // confirmation prompt below -- the privileged side independently
    // recomputes it either way (see windows_partition_plan_for).
    match args.layout {
        WindowsLayout::Ntfs => {
            // The NTFS layout still needs mkfs.ntfs + ntfs-3g, which is why
            // it stays Linux-only; the FAT32 layout (phase 3 M4, #34) needs
            // neither and runs on macOS too.
            if !cfg!(target_os = "linux") {
                return Err(ArgosError::WindowsImageRequiresLinux);
            }
            let layout = windows_partition_plan_for(&args.iso)?;
            preflight::check_windows_capacity(
                &device.platform_id,
                device.size_bytes,
                &args.iso,
                &layout,
            )?;
            check_source_collision(platform, device, args)?;
            confirm_windows_write_or_abort(device, &args.iso, &layout)?;
        }
        WindowsLayout::Fat32 => {
            let (layout, actions) = windows_fat32_plan_for(&args.iso)?;
            preflight::check_windows_fat32_capacity(
                &device.platform_id,
                device.size_bytes,
                &args.iso,
                &layout,
            )?;
            check_source_collision(platform, device, args)?;
            confirm_windows_fat32_write_or_abort(device, &args.iso, &layout, &actions)?;
        }
    }

    let plan = WriteWindowsPlan {
        device_path: device.platform_id.clone(),
        expected_serial: device.serial.clone(),
        expected_size_bytes: device.size_bytes,
        iso_path: args.iso.clone(),
        layout: args.layout,
    };

    let outcome = helper::run_plan(&Plan::WriteWindowsImage(plan))?;
    println!("Done. {outcome}.");
    println!(
        "Run `argos verify {} {}` to confirm it.",
        device.platform_id,
        args.iso.display()
    );

    if !args.no_eject {
        eject_best_effort(platform, device);
    }

    Ok(())
}

/// Builds the same [`WindowsPartitionPlan`] `execute_write_windows_image`
/// will independently recompute -- purely for display in the confirmation
/// prompt below; the privileged side never trusts this one, the same
/// never-trust-the-caller posture the rest of the Windows write path uses.
fn windows_partition_plan_for(iso: &Path) -> Result<WindowsPartitionPlan> {
    let files_total_size_bytes: u64 = image::windows::WindowsIso::open(iso)?
        .list_files()?
        .iter()
        .map(|f| f.size)
        .sum();
    Ok(WindowsPartitionPlan::new(
        argos_privileged::windows::uefi_ntfs_image_size_bytes(),
        files_total_size_bytes,
    ))
}

/// [`windows_partition_plan_for`]'s FAT32 counterpart (phase 3 M3.5,
/// backlog #43) -- same display-only role, and it front-runs the helper's
/// own refusals so an ISO the FAT32 layout genuinely cannot hold fails
/// here, before any sudo/pkexec prompt or destructive confirmation.
///
/// Calls the helper's own [`plan_copy_actions`], deliberately, rather than
/// reimplementing the "will this fit?" rules: the first version of this
/// function had its own copy of the pre-splitter check, which went stale
/// the moment the WIM splitter landed and made `argos write --layout
/// fat32` refuse real Windows media the helper handled fine. Sharing the
/// function is what keeps the prompt's numbers and the helper's behaviour
/// from ever disagreeing again.
///
/// Returns the layout plus the copy actions, so the caller can show how
/// many `.swm` parts an oversized `install.wim` will become.
fn windows_fat32_plan_for(iso: &Path) -> Result<(WindowsFat32Plan, Vec<CopyAction>)> {
    let image = image::windows::WindowsIso::open(iso)?;
    let files = image.list_files()?;
    let actions = plan_copy_actions(&image, &files)?;
    let layout = fat32_layout_for(&actions);
    Ok((layout, actions))
}

/// The source-on-target-device guard, shared verbatim by both Windows
/// layouts (and the same check `run_dd_write` does inline).
fn check_source_collision(platform: &impl PlatformOps, device: &Device, args: &Args) -> Result<()> {
    if let Some(backing_device_id) = platform.backing_device_of(&args.iso)? {
        preflight::check_no_source_target_collision(
            &args.iso,
            &backing_device_id,
            &device.platform_id,
        )?;
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
fn confirm_or_abort(device: &Device, iso: &Path, image_size_bytes: u64) -> Result<()> {
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
        return Err(ArgosError::NotConfirmed);
    }
    Ok(())
}

/// The Windows-write counterpart to [`confirm_or_abort`] above: same
/// retype-the-device-path guard, but shows the two-partition layout Argos is
/// about to create (backlog #27, W5) instead of a single image size, since
/// there's no single "image size" for a two-partition write to show.
fn confirm_windows_write_or_abort(
    device: &Device,
    iso: &Path,
    layout: &WindowsPartitionPlan,
) -> Result<()> {
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
    println!("  image:   {} (Windows installer)", iso.display());
    println!();
    println!("Argos will create a new partition table with:");
    println!(
        "  partition 1 (EFI boot, FAT):       {} at offset {}",
        helper::human_size(layout.boot_partition.size_bytes),
        helper::human_size(layout.boot_partition.start_offset_bytes)
    );
    println!(
        "  partition 2 (Windows files, NTFS): {} at offset {}",
        helper::human_size(layout.windows_partition.size_bytes),
        helper::human_size(layout.windows_partition.start_offset_bytes)
    );
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
        return Err(ArgosError::NotConfirmed);
    }
    Ok(())
}

/// The FAT32-layout confirmation prompt (phase 3 M3.5): same
/// retype-the-device-path guard as [`confirm_windows_write_or_abort`], with
/// the single-partition layout shown instead of two.
fn confirm_windows_fat32_write_or_abort(
    device: &Device,
    iso: &Path,
    layout: &WindowsFat32Plan,
    actions: &[CopyAction],
) -> Result<()> {
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
    println!("  image:   {} (Windows installer)", iso.display());
    println!();
    println!("Argos will create a new partition table with:");
    println!(
        "  partition 1 (Windows files, FAT32): {} at offset {}",
        helper::human_size(layout.windows_partition.size_bytes),
        helper::human_size(layout.windows_partition.start_offset_bytes)
    );
    // Splitting is invisible in the layout above but very visible on the
    // resulting media (install.wim becomes install.swm + install2.swm ...),
    // so say so before the user commits to the write.
    for action in actions {
        if let CopyAction::SplitWim {
            source_path,
            part_paths,
            ..
        } = action
        {
            println!(
                "  note: {source_path} is over FAT32's 4GiB file limit and will be split into {} parts ({})",
                part_paths.len(),
                part_paths.join(", ")
            );
        }
    }
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
        return Err(ArgosError::NotConfirmed);
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

    /// Regression guard for a bug found by a user running `argos write
    /// --layout fat32` against a real Windows 11 ISO: this function used to
    /// carry its own pre-splitter "every file must fit FAT32" check, so it
    /// rejected media the helper could write perfectly well, before even
    /// prompting. The planning logic is now shared with the helper --
    /// asserting on a real oversized WIM here is what proves the two agree.
    ///
    /// Uses the synthetic UDF fixture (small files only), so it pins the
    /// non-refusal for ordinary media; the oversized-WIM half is covered by
    /// `argos-privileged`'s own `plan_copy_actions` tests, which run against
    /// a WIM too large for FAT32.
    #[test]
    fn fat32_planning_accepts_ordinary_windows_media() {
        let iso = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            iso.path(),
            argos_core::image::windows::fixtures::udf_windows_installer_iso(true, true),
        )
        .unwrap();

        let (layout, actions) = windows_fat32_plan_for(iso.path()).expect(
            "a Windows installer ISO must plan cleanly for fat32 -- a refusal here means the \
             CLI and the helper disagree about what the layout can hold",
        );
        assert!(!actions.is_empty());
        assert!(layout.total_bytes_required() > 0);
    }
}
