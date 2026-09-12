//! Pure parsing helpers for the two Linux data sources this backend reads:
//! `/sys/block/<name>` (topology, size, removable flag) and the udev database
//! under `/run/udev/data/b<major>:<minor>` (serial number, `ID_BUS`). Reading
//! either of those from the real filesystem happens in `enumerate.rs`; this
//! module only turns their contents into structured data, so the parsing itself
//! can be unit-tested with plain strings.
//!
//! Going through the udev database as flat text files (rather than linking
//! against libudev via bindgen, or talking to UDisks2 over D-Bus) is a
//! deliberate v1 simplification: it needs no extra system libraries to build,
//! at the cost of only working on systems that run udev (virtually all desktop
//! and server Linux distros). See the backlog (E2.1) for the D-Bus/UDisks2
//! upgrade path, which would additionally match what desktop file managers show.

use argos_core::device::Bus;
use std::collections::HashMap;

/// Best-effort bus classification from a resolved sysfs device path, e.g.
/// `/sys/devices/pci0000:00/0000:00:14.0/usb1/1-1/.../block/sda`.
pub fn detect_bus_from_syspath(syspath: &str) -> Bus {
    if syspath.contains("/usb") {
        Bus::Usb
    } else if syspath.contains("/mmc") {
        Bus::Sdio
    } else if syspath.contains("/nvme") {
        Bus::Nvme
    } else if syspath.contains("/ata") {
        Bus::Ata
    } else {
        Bus::Unknown
    }
}

/// Parses a udev database record (the format found in
/// `/run/udev/data/b<major>:<minor>`): one `KEY=value` assignment per line,
/// prefixed with a record-type letter and a colon (we only care about `E:`,
/// exported properties).
pub fn parse_udev_db_record(contents: &str) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("E:"))
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Names under `/sys/block` that are never real, independently-writable disks:
/// loop devices, ramdisks, device-mapper/LVM/RAID targets, zram, and optical
/// drives (out of scope for v1).
pub fn is_excluded_block_device_name(name: &str) -> bool {
    const EXCLUDED_PREFIXES: &[&str] = &["loop", "ram", "dm-", "md", "zram", "sr"];
    EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Pulls "sectors written" out of a `/sys/block/<dev>/stat` line: field 7
/// (1-based) of a whitespace-separated row, in 512-byte sectors regardless of
/// the device's own block size (kernel `Documentation/block/stat.rst`).
///
/// This is completed writes as counted by the block layer, so it tracks what
/// has actually reached the device rather than what is still sitting in the
/// page cache -- which is what makes it usable as a progress signal that
/// doesn't lie, without an `fsync` barrier to force the answer to be true.
/// Verified against a controlled 64MiB write: 131344 sectors for 131072
/// written, the difference being filesystem journal traffic a raw device
/// write doesn't have.
pub fn parse_sectors_written(stat_contents: &str) -> Option<u64> {
    stat_contents.split_whitespace().nth(6)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_usb_bus_from_syspath_segment() {
        let path = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-1/1-1:1.0/host0/target0:0:0/0:0:0:0/block/sdb";
        assert_eq!(detect_bus_from_syspath(path), Bus::Usb);
    }

    #[test]
    fn detects_nvme_bus_from_syspath_segment() {
        let path = "/sys/devices/pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1";
        assert_eq!(detect_bus_from_syspath(path), Bus::Nvme);
    }

    #[test]
    fn unknown_bus_when_no_segment_matches() {
        assert_eq!(
            detect_bus_from_syspath("/sys/devices/virtual/block/sdb"),
            Bus::Unknown
        );
    }

    #[test]
    fn parses_relevant_udev_exported_properties() {
        let record =
            "P:/devices/.../block/sdb\nE:ID_BUS=usb\nE:ID_SERIAL_SHORT=ABC123\nS:disk/by-id/foo\n";
        let props = parse_udev_db_record(record);
        assert_eq!(props.get("ID_BUS").map(String::as_str), Some("usb"));
        assert_eq!(
            props.get("ID_SERIAL_SHORT").map(String::as_str),
            Some("ABC123")
        );
    }

    #[test]
    fn excludes_virtual_and_loop_devices() {
        assert!(is_excluded_block_device_name("loop0"));
        assert!(is_excluded_block_device_name("dm-1"));
        assert!(is_excluded_block_device_name("zram0"));
        assert!(is_excluded_block_device_name("sr0"));
        assert!(!is_excluded_block_device_name("sdb"));
        assert!(!is_excluded_block_device_name("nvme0n1"));
    }

    /// A real row, copied verbatim off `/sys/block/sdg/stat` mid-write on the
    /// USB stick this was built for: 1796632 is the sectors-written field,
    /// and picking the wrong column here would silently report someone
    /// else's number (read sectors, merges, ticks) as write progress.
    #[test]
    fn reads_the_sectors_written_column_of_a_real_stat_row() {
        let row = "     265      415    14090     1371      999   171818  1796632   \
                   433014        1   261347   434386        0        0        0        0        \
                   0        0";
        assert_eq!(parse_sectors_written(row), Some(1_796_632));
    }

    #[test]
    fn a_stat_row_that_is_too_short_or_unparsable_reports_nothing() {
        assert_eq!(parse_sectors_written(""), None);
        assert_eq!(parse_sectors_written("1 2 3 4 5 6"), None);
        assert_eq!(parse_sectors_written("1 2 3 4 5 6 not-a-number"), None);
    }
}
