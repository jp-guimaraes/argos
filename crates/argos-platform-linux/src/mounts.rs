//! Parsing of `/proc/mounts` and the system-disk safety judgement built on top of
//! it. Kept as pure string-processing functions (no `/proc` access inside this
//! module) so the logic that decides "is this disk holding my root filesystem" is
//! unit-testable without a real Linux mount table.

use std::path::Path;

/// One line of `/proc/mounts`: `<source> <mountpoint> <fstype> ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub source: String,
    pub mountpoint: String,
}

/// Mountpoints that mean "this is a system disk, never offer it for writing".
const CRITICAL_MOUNTPOINTS: &[&str] = &["/", "/boot", "/boot/efi", "/home"];

pub fn parse_proc_mounts(contents: &str) -> Vec<MountEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let mountpoint = fields.next()?;
            Some(MountEntry {
                source: source.to_string(),
                mountpoint: mountpoint.to_string(),
            })
        })
        .collect()
}

/// Strips a trailing partition number from a Linux block device path, e.g.
/// `/dev/sdb1` -> `/dev/sdb`, `/dev/nvme0n1p2` -> `/dev/nvme0n1`,
/// `/dev/mmcblk0p1` -> `/dev/mmcblk0`. Devices already naming a whole disk are
/// returned unchanged.
pub fn whole_disk_of(partition_path: &str) -> String {
    let trimmed = partition_path.trim_end_matches(|c: char| c.is_ascii_digit());
    // nvme0n1p2 / mmcblk0p1 use a literal "p" separator before the partition
    // number; sdb1 does not. Only strip the "p" when what's left still looks like
    // a whole-disk name for that family (ends in a digit from the namespace, e.g.
    // "n1"), so we don't mangle disks that legitimately have no partitions.
    if let Some(stripped) = trimmed.strip_suffix('p') {
        if stripped.ends_with(|c: char| c.is_ascii_digit()) {
            return stripped.to_string();
        }
    }
    trimmed.to_string()
}

/// True if any mount in `mounts` both (a) lives on `whole_disk` and (b) sits at a
/// mountpoint that makes the disk a system disk.
pub fn disk_holds_a_critical_mount(mounts: &[MountEntry], whole_disk: &str) -> bool {
    mounts.iter().any(|m| {
        whole_disk_of(&m.source) == whole_disk
            && CRITICAL_MOUNTPOINTS.contains(&m.mountpoint.as_str())
    })
}

/// Finds the mount whose mountpoint is the longest matching prefix of `path` --
/// i.e. the filesystem that actually contains `path` -- and returns the whole
/// disk backing it. Used for the source/target collision preflight check.
pub fn whole_disk_containing_path(mounts: &[MountEntry], path: &Path) -> Option<String> {
    let path_str = path.to_str()?;
    mounts
        .iter()
        .filter(|m| {
            path_str == m.mountpoint
                || path_str.starts_with(&format!("{}/", m.mountpoint.trim_end_matches('/')))
                || m.mountpoint == "/"
        })
        .max_by_key(|m| m.mountpoint.len())
        .map(|m| whole_disk_of(&m.source))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
/dev/nvme0n1p1 /boot/efi vfat rw,relatime 0 0
/dev/sdb1 /media/user/USBSTICK vfat rw,relatime 0 0
tmpfs /tmp tmpfs rw 0 0
";

    #[test]
    fn parses_lines_into_source_and_mountpoint() {
        let mounts = parse_proc_mounts(SAMPLE);
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0].source, "/dev/nvme0n1p2");
        assert_eq!(mounts[0].mountpoint, "/");
    }

    #[test]
    fn whole_disk_of_strips_sd_style_partition_suffix() {
        assert_eq!(whole_disk_of("/dev/sdb1"), "/dev/sdb");
        assert_eq!(whole_disk_of("/dev/sdb"), "/dev/sdb");
    }

    #[test]
    fn whole_disk_of_strips_nvme_style_partition_suffix() {
        assert_eq!(whole_disk_of("/dev/nvme0n1p2"), "/dev/nvme0n1");
    }

    #[test]
    fn whole_disk_of_strips_mmcblk_style_partition_suffix() {
        assert_eq!(whole_disk_of("/dev/mmcblk0p1"), "/dev/mmcblk0");
    }

    #[test]
    fn system_disk_is_detected_via_root_mount() {
        let mounts = parse_proc_mounts(SAMPLE);
        assert!(disk_holds_a_critical_mount(&mounts, "/dev/nvme0n1"));
    }

    #[test]
    fn usb_stick_is_not_flagged_as_system_disk() {
        let mounts = parse_proc_mounts(SAMPLE);
        assert!(!disk_holds_a_critical_mount(&mounts, "/dev/sdb"));
    }

    #[test]
    fn finds_whole_disk_containing_a_path_via_longest_prefix_match() {
        let mounts = parse_proc_mounts(SAMPLE);
        let disk = whole_disk_containing_path(&mounts, Path::new("/boot/efi/EFI/BOOT/BOOTX64.EFI"));
        assert_eq!(disk.as_deref(), Some("/dev/nvme0n1"));
    }

    #[test]
    fn falls_back_to_root_mount_when_no_more_specific_mount_matches() {
        let mounts = parse_proc_mounts(SAMPLE);
        let disk = whole_disk_containing_path(&mounts, Path::new("/home/user/ubuntu.iso"));
        assert_eq!(disk.as_deref(), Some("/dev/nvme0n1"));
    }
}
