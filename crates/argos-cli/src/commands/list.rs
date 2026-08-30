use super::helper::human_size;
use crate::platform_select::current_platform;
use argos_core::device::Device;
use argos_core::error::Result;
use argos_platform::PlatformOps;

pub fn run() -> Result<()> {
    let devices = current_platform().list_removable_disks()?;

    if devices.is_empty() {
        println!("No disks found.");
        return Ok(());
    }

    println!(
        "{:<12} {:>10} {:<8} {:<10} {:<20} SAFE TO WRITE?",
        "DEVICE", "SIZE", "BUS", "REMOVABLE", "SERIAL"
    );
    for device in &devices {
        print_row(device);
    }
    Ok(())
}

fn print_row(device: &Device) {
    let safe = if device.is_safe_to_write() {
        "yes"
    } else if device.is_system_disk {
        "no (system disk)"
    } else {
        "no"
    };
    println!(
        "{:<12} {:>10} {:<8?} {:<10} {:<20} {}",
        device.platform_id,
        human_size(device.size_bytes),
        device.bus,
        device.os_reports_removable,
        device.serial.as_deref().unwrap_or("-"),
        safe,
    );
}
