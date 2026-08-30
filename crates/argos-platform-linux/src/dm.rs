//! Resolves a mount source down to the physical whole disk(s) that actually
//! back it, walking through any device-mapper stack in between (LVM, software
//! RAID, dm-crypt, multipath -- the kernel represents all of these the same
//! way: a `dm-N` block device whose `/sys/block/dm-N/slaves/` directory lists
//! what it sits on top of, however many layers deep).
//!
//! Without this, a disk that is itself unremarkable (say, an internal drive
//! added as an LVM physical volume to the volume group holding `/home`) would
//! never be recognized as a system disk by [`crate::mounts`], because
//! `/proc/mounts` only ever shows the *top* of the stack (e.g.
//! `/dev/mapper/vg-home`), never the physical partition underneath. The same
//! gap silently breaks the source/target collision check for any ISO stored
//! on an LVM-backed filesystem -- a very common default layout on desktop
//! Linux installs.
//!
//! The recursive walk itself ([`physical_disks_of`]) is pure and takes the
//! sysfs read as an injected closure, so it's unit-testable without a real
//! device-mapper stack; [`sysfs_slaves_of`] is the thin, effectively
//! untestable-without-root glue that actually reads `/sys/block/*/slaves`.

use crate::mounts::whole_disk_of;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Given a block device name as it appears under `/sys/block` (e.g. `"sda1"`,
/// `"dm-0"`), returns the whole physical disk(s) that ultimately back it.
/// `slaves_of` returns the direct sysfs "slaves" of a device name (empty for
/// a device that isn't device-mapper-backed, i.e. the recursion's base case).
pub fn physical_disks_of(start: &str, slaves_of: &impl Fn(&str) -> Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    resolve(start, slaves_of, &mut seen, &mut results);
    results
}

fn resolve(
    name: &str,
    slaves_of: &impl Fn(&str) -> Vec<String>,
    seen: &mut HashSet<String>,
    results: &mut Vec<String>,
) {
    // Device-mapper stacks are DAGs in practice, not cycles, but a defensive
    // guard costs nothing and turns a hypothetical bug into "returns an
    // incomplete answer" rather than "hangs forever".
    if !seen.insert(name.to_string()) {
        return;
    }

    let slaves = slaves_of(name);
    if slaves.is_empty() {
        let disk = whole_disk_of(&format!("/dev/{name}"));
        if !results.contains(&disk) {
            results.push(disk);
        }
        return;
    }

    for slave in slaves {
        resolve(&slave, slaves_of, seen, results);
    }
}

/// The real, sysfs-backed implementation of the `slaves_of` closure
/// `physical_disks_of` expects. Returns an empty list (meaning "this is
/// already a physical/terminal device") for anything that isn't tracked by
/// device-mapper, including when `/sys/block/<name>/slaves` doesn't exist.
pub fn sysfs_slaves_of(name: &str) -> Vec<String> {
    let path = Path::new("/sys/block").join(name).join("slaves");
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Resolves a `/proc/mounts` source (e.g. `/dev/sdb1`, `/dev/dm-0`,
/// `/dev/mapper/vg-root`) to the sysfs block device name `physical_disks_of`
/// expects as a starting point, by resolving any `/dev/mapper/*` symlink to
/// its real `dm-N` target. Falls back to a naive `/dev/` strip if the path no
/// longer exists (e.g. the device was unplugged since `/proc/mounts` was read).
pub fn block_device_name_of_mount_source(source: &str) -> String {
    fs::canonicalize(source)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| source.trim_start_matches("/dev/").to_string())
}

/// Real end-to-end resolver: mount source string -> physical whole disk(s).
/// This is what [`crate::mounts`]'s callers pass in production; tests use
/// simpler, injected resolvers instead (see `mounts::tests`).
pub fn resolve_mount_source(source: &str) -> Vec<String> {
    physical_disks_of(&block_device_name_of_mount_source(source), &sysfs_slaves_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_partition_with_no_slaves_resolves_to_its_own_whole_disk() {
        let slaves_of = |_: &str| Vec::new();
        assert_eq!(physical_disks_of("sdb1", &slaves_of), vec!["/dev/sdb"]);
    }

    #[test]
    fn single_level_lvm_resolves_through_one_dm_hop() {
        // A typical LVM logical volume (dm-0) sitting directly on one
        // physical volume (sdb1) -- the common single-disk desktop layout.
        let slaves_of = |name: &str| match name {
            "dm-0" => vec!["sdb1".to_string()],
            _ => Vec::new(),
        };
        assert_eq!(physical_disks_of("dm-0", &slaves_of), vec!["/dev/sdb"]);
    }

    #[test]
    fn multi_level_stack_resolves_through_dm_crypt_under_lvm() {
        // LV (dm-1) -> LUKS/dm-crypt volume (dm-0) -> physical partition.
        let slaves_of = |name: &str| match name {
            "dm-1" => vec!["dm-0".to_string()],
            "dm-0" => vec!["sdc2".to_string()],
            _ => Vec::new(),
        };
        assert_eq!(physical_disks_of("dm-1", &slaves_of), vec!["/dev/sdc"]);
    }

    #[test]
    fn striped_lv_across_two_disks_resolves_to_both() {
        let slaves_of = |name: &str| match name {
            "dm-2" => vec!["sdb1".to_string(), "sdc1".to_string()],
            _ => Vec::new(),
        };
        let mut disks = physical_disks_of("dm-2", &slaves_of);
        disks.sort();
        assert_eq!(disks, vec!["/dev/sdb", "/dev/sdc"]);
    }

    #[test]
    fn does_not_infinite_loop_on_a_cyclic_graph() {
        // Should never happen for real device-mapper devices, but the guard
        // must not hang if it somehow does.
        let slaves_of = |name: &str| match name {
            "dm-0" => vec!["dm-1".to_string()],
            "dm-1" => vec!["dm-0".to_string()],
            _ => Vec::new(),
        };
        // Just must terminate; the exact (empty) result isn't the point.
        let _ = physical_disks_of("dm-0", &slaves_of);
    }
}
