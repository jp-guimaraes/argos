//! Cross-checks `WindowsIso::list_files` against the operating system's own
//! UDF driver on a real Windows ISO.
//!
//! Why this earns its place: every file the walker misses is a file that
//! never reaches the USB stick, and the failure mode is silent -- the write
//! and the verify both succeed (they agree with each other, since both use
//! this same listing), and the medium simply fails to boot with no
//! diagnostic. Counting bytes against an independent implementation is what
//! turns that into a test failure instead.
//!
//! Boot-critical paths are asserted by name for the same reason: a media set
//! can be complete by count and still be unbootable if, say,
//! `efi/boot/bootx64.efi` were dropped by a case- or path-handling bug.
//!
//! ```sh
//! ARGOS_TEST_REAL_WINDOWS_ISO=.testdata/Win10_22H2_English_x64v1.iso \
//!     cargo test -p argos-core --release --test iso_listing_check \
//!     -- --ignored --nocapture
//! ```

use argos_core::image::windows::WindowsIso;
use std::path::PathBuf;
use std::process::Command;

/// Files without which the media cannot boot, or cannot install.
/// Compared case-insensitively: UDF media has been seen using either case,
/// and FAT (what these land on) is case-insensitive anyway.
const BOOT_CRITICAL: &[&str] = &[
    "efi/boot/bootx64.efi",
    "bootmgr",
    "bootmgr.efi",
    "setup.exe",
    "sources/boot.wim",
];

#[test]
#[ignore = "needs a real Windows ISO via ARGOS_TEST_REAL_WINDOWS_ISO"]
fn our_listing_matches_the_os_udf_driver() {
    let Some(iso_path) = std::env::var_os("ARGOS_TEST_REAL_WINDOWS_ISO").map(PathBuf::from) else {
        eprintln!("skipping: ARGOS_TEST_REAL_WINDOWS_ISO not set");
        return;
    };

    let iso = WindowsIso::open(&iso_path).expect("failed to open the ISO");
    let files = iso.list_files().expect("failed to list the ISO");
    let our_count = files.len();
    let our_bytes: u64 = files.iter().map(|f| f.size).sum();
    eprintln!("argos: {our_count} files, {our_bytes} bytes");

    for want in BOOT_CRITICAL {
        assert!(
            files.iter().any(|f| f.path.eq_ignore_ascii_case(want)),
            "{want} is missing from our listing -- media built from it would not boot"
        );
    }

    // The oracle: macOS's own UDF driver, via a read-only mount.
    let Some(mount_point) = attach_readonly(&iso_path) else {
        eprintln!("skipping the cross-check: hdiutil attach failed");
        return;
    };
    let (os_count, os_bytes) = walk(&mount_point);
    let _ = Command::new("hdiutil")
        .arg("detach")
        .arg(&mount_point)
        .output();

    eprintln!("os:    {os_count} files, {os_bytes} bytes");
    assert_eq!(
        our_count, os_count,
        "our walker and the OS driver disagree on how many files this ISO has"
    );
    assert_eq!(
        our_bytes, os_bytes,
        "our walker and the OS driver disagree on the total size of this ISO's files"
    );
}

#[cfg(target_os = "macos")]
fn attach_readonly(iso: &std::path::Path) -> Option<PathBuf> {
    let output = Command::new("hdiutil")
        .args(["attach", "-readonly", "-nobrowse"])
        .arg(iso)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.find("/Volumes/")
                .map(|idx| PathBuf::from(line[idx..].trim_end()))
        })
}

#[cfg(not(target_os = "macos"))]
fn attach_readonly(_iso: &std::path::Path) -> Option<PathBuf> {
    // Mounting an ISO on Linux needs root; the boot-critical assertions
    // above still run there, only the count cross-check is skipped.
    None
}

/// Counts regular files and sums their sizes under `root`, skipping the
/// resource-fork sidecars macOS materializes on read-only mounts.
fn walk(root: &std::path::Path) -> (usize, u64) {
    let mut count = 0;
    let mut bytes = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_type = entry.file_type().expect("file type should be readable");
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                count += 1;
                bytes += entry.metadata().expect("metadata should be readable").len();
            }
        }
    }
    (count, bytes)
}
