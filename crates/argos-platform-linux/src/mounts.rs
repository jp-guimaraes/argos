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
/// `/dev/mmcblk0p1` -> `/dev/mmcblk0`, `/dev/loop0p1` -> `/dev/loop0`.
/// Devices already naming a whole disk are returned unchanged -- notably
/// `/dev/loop0`, which the bare-digit-stripping rule below would otherwise
/// mangle into `/dev/loop` (and, worse, collapse every loop device on the
/// system into the same "whole disk").
pub fn whole_disk_of(partition_path: &str) -> String {
    // nvme0n1p2 / mmcblk0p1 / loop0p1 use a literal "p" separator before the
    // partition number; sdb1 does not. Only treat it as a partition suffix
    // when what's left still looks like a whole-disk name for that family
    // (ends in a digit from the namespace, e.g. "n1" or "0"), so we don't
    // mangle disks that legitimately have no partitions.
    if let Some(whole_disk) = strip_p_digit_suffix(partition_path) {
        return whole_disk;
    }
    // loop0, loop12, ... are already whole-disk names -- the trailing digit
    // is the loop device's own index, not a partition number, unlike
    // sdb1/hdb1/vdb1 below.
    let base_name = partition_path.rsplit('/').next().unwrap_or(partition_path);
    if base_name.starts_with("loop") {
        return partition_path.to_string();
    }
    partition_path
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .to_string()
}

fn strip_p_digit_suffix(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(|c: char| c.is_ascii_digit());
    let stripped = trimmed.strip_suffix('p')?;
    stripped
        .ends_with(|c: char| c.is_ascii_digit())
        .then(|| stripped.to_string())
}

/// True if any mount in `mounts` both (a) lives on `whole_disk` and (b) sits at a
/// mountpoint that makes the disk a system disk.
///
/// `resolve_physical_disks` maps a mount's raw source (e.g. `/dev/sdb1`, but
/// just as often `/dev/mapper/vg-home` for an LVM-backed mount) down to the
/// physical whole disk(s) that actually back it -- production code passes
/// [`crate::dm::resolve_mount_source`], which walks any device-mapper stack
/// (LVM, RAID, dm-crypt); tests pass a simpler stand-in. Without this
/// resolution step, a disk holding an LVM physical volume for `/home` would
/// never be recognized as a system disk, because `/proc/mounts` only ever
/// shows the top of the device-mapper stack, never the physical partition
/// underneath.
pub fn disk_holds_a_critical_mount(
    mounts: &[MountEntry],
    whole_disk: &str,
    resolve_physical_disks: &impl Fn(&str) -> Vec<String>,
) -> bool {
    mounts.iter().any(|m| {
        CRITICAL_MOUNTPOINTS.contains(&m.mountpoint.as_str())
            && resolve_physical_disks(&m.source)
                .iter()
                .any(|disk| disk == whole_disk)
    })
}

/// Finds the mount whose mountpoint is the longest matching prefix of `path` --
/// i.e. the filesystem that actually contains `path` -- and returns the whole
/// disk backing it (see `resolve_physical_disks` on
/// [`disk_holds_a_critical_mount`] for why this needs to resolve through
/// device-mapper too: an ISO stored on an LVM-backed `/home`, a very common
/// desktop Linux layout, would otherwise never be recognized as living on the
/// disk it actually lives on). Used for the source/target collision preflight
/// check. When the mount resolves to more than one physical disk (e.g. an LV
/// striped across multiple physical volumes), only the first is returned --
/// good enough to catch the common single-disk case, not a complete answer
/// for exotic multi-disk layouts.
pub fn whole_disk_containing_path(
    mounts: &[MountEntry],
    path: &Path,
    resolve_physical_disks: &impl Fn(&str) -> Vec<String>,
) -> Option<String> {
    let path_str = path.to_str()?;
    mounts
        .iter()
        .filter(|m| {
            path_str == m.mountpoint
                || path_str.starts_with(&format!("{}/", m.mountpoint.trim_end_matches('/')))
                || m.mountpoint == "/"
        })
        .max_by_key(|m| m.mountpoint.len())
        .and_then(|m| resolve_physical_disks(&m.source).into_iter().next())
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
    fn whole_disk_of_leaves_a_bare_loop_device_unchanged() {
        // A loop device's trailing digit is its own index, not a partition
        // number -- unlike sdb1, "/dev/loop0" must not become "/dev/loop"
        // (which would incorrectly treat every loop device as the same disk).
        assert_eq!(whole_disk_of("/dev/loop0"), "/dev/loop0");
        assert_eq!(whole_disk_of("/dev/loop12"), "/dev/loop12");
    }

    #[test]
    fn whole_disk_of_strips_loop_style_partition_suffix() {
        assert_eq!(whole_disk_of("/dev/loop0p1"), "/dev/loop0");
    }

    /// The trivial resolver used by most tests: no device-mapper indirection,
    /// a mount source's whole disk is just itself.
    fn direct(source: &str) -> Vec<String> {
        vec![whole_disk_of(source)]
    }

    const LVM_SAMPLE: &str = "\
/dev/mapper/vg-root / ext4 rw,relatime 0 0
/dev/sda1 /boot/efi vfat rw,relatime 0 0
/dev/mapper/vg-home /home ext4 rw,relatime 0 0
";

    #[test]
    fn system_disk_is_detected_via_root_mount() {
        let mounts = parse_proc_mounts(SAMPLE);
        assert!(disk_holds_a_critical_mount(
            &mounts,
            "/dev/nvme0n1",
            &direct
        ));
    }

    #[test]
    fn usb_stick_is_not_flagged_as_system_disk() {
        let mounts = parse_proc_mounts(SAMPLE);
        assert!(!disk_holds_a_critical_mount(&mounts, "/dev/sdb", &direct));
    }

    #[test]
    fn finds_whole_disk_containing_a_path_via_longest_prefix_match() {
        let mounts = parse_proc_mounts(SAMPLE);
        let disk = whole_disk_containing_path(
            &mounts,
            Path::new("/boot/efi/EFI/BOOT/BOOTX64.EFI"),
            &direct,
        );
        assert_eq!(disk.as_deref(), Some("/dev/nvme0n1"));
    }

    #[test]
    fn falls_back_to_root_mount_when_no_more_specific_mount_matches() {
        let mounts = parse_proc_mounts(SAMPLE);
        let disk = whole_disk_containing_path(&mounts, Path::new("/home/user/ubuntu.iso"), &direct);
        assert_eq!(disk.as_deref(), Some("/dev/nvme0n1"));
    }

    #[test]
    fn lvm_backed_home_is_detected_as_a_system_disk_through_the_resolver() {
        // /proc/mounts only ever shows "/dev/mapper/vg-home", never the real
        // physical volume underneath -- the resolver is what bridges that gap.
        let mounts = parse_proc_mounts(LVM_SAMPLE);
        let resolve = |source: &str| match source {
            "/dev/mapper/vg-root" | "/dev/mapper/vg-home" => vec!["/dev/sda".to_string()],
            other => vec![whole_disk_of(other)],
        };
        assert!(disk_holds_a_critical_mount(&mounts, "/dev/sda", &resolve));
        assert!(!disk_holds_a_critical_mount(&mounts, "/dev/sdb", &resolve));
    }

    #[test]
    fn iso_on_an_lvm_backed_home_resolves_to_the_real_physical_disk() {
        let mounts = parse_proc_mounts(LVM_SAMPLE);
        let resolve = |source: &str| match source {
            "/dev/mapper/vg-root" | "/dev/mapper/vg-home" => vec!["/dev/sda".to_string()],
            other => vec![whole_disk_of(other)],
        };
        let disk =
            whole_disk_containing_path(&mounts, Path::new("/home/user/ubuntu.iso"), &resolve);
        assert_eq!(disk.as_deref(), Some("/dev/sda"));
    }
}
