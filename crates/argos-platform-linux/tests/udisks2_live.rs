//! Backlog E2: sanity-checks `udisks2::Udisks2Snapshot::fetch()` against
//! whatever `udisksd` is actually running on the machine running this test --
//! no root needed (reading UDisks2's properties is unprivileged), so this
//! runs as a normal test, not an `#[ignore]`d one. It softly skips (never
//! fails) when no D-Bus/UDisks2 is reachable at all, since that's an
//! expected, supported configuration this crate falls back from gracefully,
//! not a bug -- CI containers and minimal installs may not run `udisksd`.

#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn fetch_reports_sane_data_for_the_running_systems_devices() {
    let Some(snapshot) = argos_platform_linux::udisks2::Udisks2Snapshot::fetch() else {
        eprintln!("skipping: no UDisks2 reachable on this machine's D-Bus system bus");
        return;
    };

    // Find *some* real, physically-transported block device via lsblk to
    // cross-check against, rather than hardcoding an assumption about this
    // machine's disk layout. TYPE=disk alone isn't enough -- it also matches
    // purely virtual "disks" like zram, which (correctly) have no UDisks2
    // Drive at all; requiring a non-empty TRAN (usb/sata/nvme/...) filters
    // those out.
    let Ok(output) = Command::new("lsblk")
        .args(["-ndo", "PATH,TYPE,TRAN"])
        .output()
    else {
        eprintln!("skipping: lsblk not available to cross-check against");
        return;
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    let Some(whole_disk_path) = listing
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let path = parts.next()?;
            let kind = parts.next()?;
            let transport = parts.next(); // absent entirely for e.g. zram
            (kind == "disk" && transport.is_some()).then(|| path.to_string())
        })
        .next()
    else {
        eprintln!("skipping: lsblk reported no physically-transported whole disks to check");
        return;
    };

    // A real whole disk (lsblk TYPE=disk, so not a partition, loop, or
    // dm/md device) should always resolve to a UDisks2 entry keyed by
    // exactly this device path -- the whole point of preferring the
    // block device without a Partition interface when several block
    // devices share one Drive object (the disk itself plus each of its
    // partitions).
    let info = snapshot.get(&whole_disk_path).unwrap_or_else(|| {
        panic!("UDisks2 has no entry for the real whole disk {whole_disk_path}")
    });

    // Whatever UDisks2 says should at least be internally consistent: a
    // drive marked removable must be on some named bus, not silently blank.
    if info.removable || info.media_removable {
        assert!(
            !info.connection_bus.is_empty(),
            "UDisks2 reported {whole_disk_path} as removable but with no ConnectionBus"
        );
    }
}
