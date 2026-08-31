//! Backlog #34, WM3: exercises the real Windows installer write
//! (`execute_write_windows_image`) and verify (`execute_verify_windows_image`)
//! paths on macOS, against a throwaway raw disk image attached via `hdiutil`
//! -- the macOS counterpart of `write_windows_image.rs`'s loop-device
//! coverage on Linux, built the same way `hdiutil_image_write.rs` attaches an
//! image for the DD-mode path.
//!
//! Requires `ntfs-3g` and `mkfs.ntfs` (via Homebrew, e.g. `brew install
//! ntfs-3g-mac`) **and** the macFUSE system extension already approved in
//! System Settings > Privacy & Security -- a one-time manual step nothing
//! here can do on its own, which is exactly why this suite is *not* part of
//! CI (see `docs/architecture.md`'s phase 3 guiding decisions): GitHub-hosted
//! `macos-latest` runners have no way to click through that approval dialog
//! on an ephemeral machine. Run this explicitly, locally, once macFUSE is
//! approved:
//!
//! ```sh
//! cargo test -p argos-privileged --features test-overrides \
//!     --test write_windows_image_macos -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test skips itself (rather than failing) when a prerequisite isn't
//! met, so running this without `--ignored`+ntfs-3g on a developer machine
//! that hasn't set macFUSE up is harmless. `--test-threads=1` for the same
//! reason `write_windows_image.rs` uses it: each test partitions and
//! mounts/unmounts a real filesystem on its own attached image, real
//! stateful OS resources these tests were never designed to share
//! concurrently.

#![cfg(target_os = "macos")]

// Aliased: real Windows installer media is UDF, not plain ISO9660 (see
// image::windows's top doc comment), so the UDF fixture is what this
// end-to-end test should build -- the alias just keeps the rest of this
// file's naming unchanged.
use argos_core::image::windows::fixtures::udf_windows_installer_iso as windows_installer_iso;
use argos_core::progress::NoopProgress;
use argos_platform::PlatformOps;
use argos_privileged::protocol::{VerifyWindowsPlan, WriteWindowsPlan};
use std::process::Command;

struct AttachedImage {
    device_node: String,
    _backing_file: tempfile::NamedTempFile,
}

impl AttachedImage {
    /// Creates a `size_bytes`-large zeroed raw disk image and attaches it
    /// with `hdiutil attach -nomount`, returning the whole-disk device node
    /// (e.g. `/dev/disk7`) hdiutil assigned it. Returns `None` (never
    /// panics) if `hdiutil` is missing or the attach fails, so callers can
    /// skip cleanly. Duplicated from `hdiutil_image_write.rs` rather than
    /// shared, matching how `write_windows_image.rs`'s `LoopDevice` already
    /// duplicates `loop_device_write.rs`'s equivalent instead of factoring
    /// out a shared test-only helper crate for one struct.
    fn attach(size_bytes: u64) -> Option<Self> {
        let backing_file = tempfile::NamedTempFile::new().ok()?;
        backing_file.as_file().set_len(size_bytes).ok()?;

        let output = Command::new("hdiutil")
            .args([
                "attach",
                "-nomount",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
            ])
            .arg(backing_file.path())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8(output.stdout).ok()?;
        let device_node = stdout
            .lines()
            .next()?
            .split_whitespace()
            .next()?
            .to_string();
        if device_node.is_empty() {
            return None;
        }

        Some(Self {
            device_node,
            _backing_file: backing_file,
        })
    }
}

impl Drop for AttachedImage {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", &self.device_node])
            .status();
    }
}

fn command_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Same sizing rationale as `write_windows_image.rs`'s `DEVICE_SIZE`: room
/// for the boot image, this test's tiny fixture ISO, the NTFS overhead
/// margin, and GPT overhead, while staying a throwaway-sized sparse file.
const DEVICE_SIZE: u64 = 200 * 1024 * 1024;

#[test]
#[ignore = "needs ntfs-3g, mkfs.ntfs, and an approved macFUSE extension; see module docs"]
fn writes_a_windows_installer_iso_to_a_real_attached_image() {
    if !command_available("mkfs.ntfs") || !command_available("ntfs-3g") {
        eprintln!("skipping: mkfs.ntfs/ntfs-3g not installed");
        return;
    }
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };

    // ARGOS_TEST_FORCE_REMOVABLE makes MacOsPlatform::refresh() report this
    // hdiutil-attached image as a removable USB disk -- see
    // argos-platform-macos's test-overrides feature.
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let plan = WriteWindowsPlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.path().to_path_buf(),
    };

    let outcome = argos_privileged::windows::execute_write_windows_image(&plan, &NoopProgress)
        .expect("writing the Windows installer image should succeed");

    assert_eq!(outcome.files_copied, 2); // BOOTMGR + SOURCES/BOOT.WIM
    assert_eq!(
        outcome.boot_partition_hash.len(),
        64,
        "expected a hex SHA-256 digest"
    );
    assert_eq!(outcome.file_hashes.len(), 2);

    // The OS should now see two real partitions for this attached image.
    assert!(std::path::Path::new(&format!("{}s1", image.device_node)).exists());
    assert!(std::path::Path::new(&format!("{}s2", image.device_node)).exists());

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs hdiutil; see module docs"]
fn refuses_a_non_windows_iso() {
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    // A plain, non-Windows-shaped ISO9660 image -- classify() should refuse
    // it before this gets anywhere near partitioning the device.
    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(false, false)).unwrap();

    let plan = WriteWindowsPlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.path().to_path_buf(),
    };

    let err =
        argos_privileged::windows::execute_write_windows_image(&plan, &NoopProgress).unwrap_err();
    assert!(matches!(
        err,
        argos_core::error::ArgosError::NotWindowsInstallerIso(_)
    ));

    // Nothing should have touched the device: no GPT signature at LBA 1.
    let mut header = [0u8; 8];
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut device = std::fs::File::open(&image.device_node).unwrap();
        device.seek(SeekFrom::Start(512)).unwrap();
        device.read_exact(&mut header).unwrap();
    }
    assert_ne!(&header, b"EFI PART");

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs ntfs-3g, mkfs.ntfs, and an approved macFUSE extension; see module docs"]
fn verify_matches_a_prior_windows_write() {
    if !command_available("mkfs.ntfs") || !command_available("ntfs-3g") {
        eprintln!("skipping: mkfs.ntfs/ntfs-3g not installed");
        return;
    }
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let write_plan = WriteWindowsPlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.path().to_path_buf(),
    };
    argos_privileged::windows::execute_write_windows_image(&write_plan, &NoopProgress)
        .expect("the write itself should succeed");

    let verify_plan = VerifyWindowsPlan {
        device_path: image.device_node.clone(),
        iso_path: iso.path().to_path_buf(),
    };
    let outcome =
        argos_privileged::windows::execute_verify_windows_image(&verify_plan, &NoopProgress)
            .expect("verify against what was just written should succeed");
    assert_eq!(outcome.files_verified, 2);

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs ntfs-3g, mkfs.ntfs, and an approved macFUSE extension; see module docs"]
fn verify_rejects_a_file_corrupted_after_the_write() {
    if !command_available("mkfs.ntfs") || !command_available("ntfs-3g") {
        eprintln!("skipping: mkfs.ntfs/ntfs-3g not installed");
        return;
    }
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let write_plan = WriteWindowsPlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.path().to_path_buf(),
    };
    argos_privileged::windows::execute_write_windows_image(&write_plan, &NoopProgress)
        .expect("the write itself should succeed");

    // Corrupt one file directly on the mounted NTFS partition, simulating
    // media corruption (or tampering) that happened after the write --
    // exactly the class of failure verification exists to catch.
    let platform = argos_platform_macos::MacOsPlatform::new();
    let device = platform
        .refresh(&image.device_node, None)
        .unwrap()
        .expect("the attached image should still be there");
    let mountpoint = platform.mount_ntfs_partition(&device, 2).unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mountpoint.join("bootmgr"))
            .unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"corrupted").unwrap();
    }
    platform.unmount_path(&mountpoint).unwrap();
    let _ = std::fs::remove_dir(&mountpoint);

    let verify_plan = VerifyWindowsPlan {
        device_path: image.device_node.clone(),
        iso_path: iso.path().to_path_buf(),
    };
    let err = argos_privileged::windows::execute_verify_windows_image(&verify_plan, &NoopProgress)
        .unwrap_err();
    assert!(matches!(
        err,
        argos_core::error::ArgosError::WindowsFileMismatch { path, .. } if path == "bootmgr"
    ));

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}
