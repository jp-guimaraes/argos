//! `argos write`: the full destructive flow. Orchestration (classify, preflight,
//! confirm, invoke `argos-helper`, render progress) lives here rather than in
//! `argos-core` for now; if a GUI needs the same flow later, it's a small lift
//! to extract this into a shared, UI-agnostic function.

use crate::platform_select::current_platform;
use argos_core::device::Device;
use argos_core::error::{ArgosError, Result};
use argos_core::image;
use argos_core::preflight;
use argos_platform::PlatformOps;
use argos_privileged::protocol::{Event, WritePlan};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

// Fields are unused until the run() body below is implemented (E4-E8); keeping
// the real shape now avoids an API break once it is.
#[allow(dead_code)]
pub struct Args {
    pub iso: PathBuf,
    pub device: String,
    pub no_verify: bool,
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

    run_helper(&plan)
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
    println!("  size:    {}", human_size(device.size_bytes));
    println!(
        "  serial:  {}",
        device.serial.as_deref().unwrap_or("unknown")
    );
    println!("  image:   {}", iso.display());
    println!("  image size: {}", human_size(image_size_bytes));
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

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// Locates `argos-helper` (preferring the binary installed next to `argos`
/// itself, falling back to PATH lookup), elevates via `pkexec` when available
/// (Linux, integrates with polkit) or `sudo` otherwise, feeds it the plan on
/// stdin, and turns its JSON event stream into an `indicatif` progress bar.
fn run_helper(plan: &WritePlan) -> Result<()> {
    let helper_path = locate_helper_binary();
    let elevation_command = if cfg!(target_os = "linux") && command_exists("pkexec") {
        "pkexec"
    } else {
        "sudo"
    };

    let mut child = Command::new(elevation_command)
        .arg(&helper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().expect("stdin was piped");
        let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
        stdin.write_all(plan_json.as_bytes())?;
    }
    // Dropping stdin (by taking it out of scope) signals EOF to the helper.
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout was piped");
    let outcome = stream_helper_events(BufReader::new(stdout));

    let status = child.wait()?;
    match outcome {
        Some(Ok(hash)) => {
            println!("Done. SHA-256: {hash}");
            Ok(())
        }
        Some(Err(err)) => Err(err),
        None if status.success() => Ok(()),
        None => Err(ArgosError::Io(std::io::Error::other(format!(
            "argos-helper exited with {status} and reported no result"
        )))),
    }
}

/// Reads one JSON [`Event`] per line from the helper, driving a progress bar,
/// until `Done` or `Error` settles the outcome (or the stream simply ends).
fn stream_helper_events<R: std::io::Read>(
    reader: BufReader<R>,
) -> Option<std::result::Result<String, ArgosError>> {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );

    let mut outcome = None;
    for line in reader.lines().map_while(std::result::Result::ok) {
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            continue;
        };
        match event {
            Event::Phase { phase } => bar.set_message(phase),
            Event::Progress {
                bytes_done,
                bytes_total,
            } => {
                bar.set_length(bytes_total);
                bar.set_position(bytes_done);
            }
            Event::Done { written_hash } => {
                bar.finish_with_message("done");
                outcome = Some(Ok(written_hash));
            }
            Event::Error { message, .. } => {
                bar.abandon();
                outcome = Some(Err(ArgosError::Io(std::io::Error::other(message))));
            }
        }
    }
    outcome
}

fn locate_helper_binary() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let sibling = dir.join("argos-helper");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("argos-helper")
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
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
