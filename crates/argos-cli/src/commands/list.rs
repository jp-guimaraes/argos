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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_without_decimals() {
        assert_eq!(human_size(512), "512B");
    }

    #[test]
    fn formats_gibibytes_with_one_decimal() {
        assert_eq!(human_size(8 * 1024 * 1024 * 1024), "8.0GiB");
    }
}
