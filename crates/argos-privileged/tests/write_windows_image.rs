//! Backlog #27, W3/W4: exercises the real Windows installer write
//! (`execute_write_windows_image`) and verify (`execute_verify_windows_image`)
//! paths against a throwaway file-backed loop device -- the Windows-write
//! counterpart to `loop_device_write.rs`'s DD-mode coverage (backlog E9),
//! which bundles its own write+verify tests the same way.
//!
//! Requires root (loop device setup), `losetup`, `mkfs.ntfs`, and `ntfs-3g`,
//! none of which CI's regular, unprivileged test job has. Run explicitly,
//! e.g.:
//!
//! ```sh
//! sudo -E cargo test -p argos-privileged --features test-overrides \
//!     --test write_windows_image -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test skips itself (rather than failing) when a prerequisite isn't
//! met, so running this without `--ignored`+root+the right packages on a
//! developer machine is harmless.
//!
//! `--test-threads=1`: belt-and-suspenders alongside the fix below, not
//! required to reproduce it -- CI hit an intermittent `ENXIO` ("No such
//! device or address") failure even serialized (see
//! `windows::wait_for_path`'s doc comment for the actual root cause: an
//! existence check racing a partition device node's kernel-side readiness).
//! Kept anyway since each test here creates a real GPT and mounts/unmounts a
//! real NTFS filesystem on its own loop device -- real, stateful kernel
//! resources these tests were never designed to share concurrently, unlike
//! `loop_device_write.rs`'s equivalent tests, which only ever `dd` raw bytes
//! onto a whole device.

#![cfg(target_os = "linux")]

// Aliased: real Windows installer media is UDF, not plain ISO9660 (see
// image::windows's top doc comment), so the UDF fixture is what this
// end-to-end test should build -- the alias just keeps the rest of this
// file's naming unchanged.
use argos_core::image::windows::fixtures::udf_windows_installer_iso as windows_installer_iso;
use argos_core::progress::NoopProgress;
use argos_platform::PlatformOps;
use argos_privileged::protocol::{VerifyWindowsPlan, WriteWindowsPlan};
use std::process::Command;

struct LoopDevice {
    path: String,
    _backing_file: tempfile::NamedTempFile,
}

impl LoopDevice {
    /// Creates a `size_bytes`-large (sparse) backing file and attaches it as
    /// a loop device. Returns `None` (never panics) when we can't -- e.g.
    /// not root, `losetup` missing -- so callers can skip cleanly.
    ///
    /// Unlike `loop_device_write.rs`'s copy of this helper, this one passes
    /// `--partscan`: without it, the kernel never creates `/dev/loopNpM`
    /// partition device nodes for this loop device at all, so
    /// `PlatformOps::reread_partition_table`'s `BLKRRPART` ioctl -- which
    /// only asks the kernel to rescan a device that's already set up to be
    /// scanned -- fails outright once a real GPT is written to it.
    fn attach(size_bytes: u64) -> Option<Self> {
        let mut backing_file = tempfile::NamedTempFile::new().ok()?;
        backing_file.as_file_mut().set_len(size_bytes).ok()?;

        let output = Command::new("losetup")
            .args(["--find", "--show", "--partscan"])
            .arg(backing_file.path())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if path.is_empty() {
            return None;
        }

        Some(Self {
            path,
            _backing_file: backing_file,
        })
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let _ = Command::new("losetup")
            .args(["--detach", &self.path])
            .status();
    }
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() takes no arguments and has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn command_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Comfortably larger than a `WindowsPartitionPlan` built from this test's
/// tiny fixture ISO needs (the boot image plus the fixture's ~80 bytes of
/// files plus the 100 MiB NTFS overhead margin plus GPT overhead all fit
/// well inside 200 MiB), while still being a throwaway-sized sparse file.
const DEVICE_SIZE: u64 = 200 * 1024 * 1024;

#[test]
#[ignore = "needs root, losetup, mkfs.ntfs, and ntfs-3g; see module docs"]
fn writes_a_windows_installer_iso_to_a_real_loop_device() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    if !command_available("mkfs.ntfs") || !command_available("ntfs-3g") {
        eprintln!("skipping: mkfs.ntfs/ntfs-3g not installed");
        return;
    }
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };

    // ARGOS_TEST_FORCE_REMOVABLE makes LinuxPlatform::refresh() report this
    // loop device as a removable USB disk, which it otherwise never would be
    // -- see argos-platform-linux's test-overrides feature.
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let plan = WriteWindowsPlan {
        device_path: loop_device.path.clone(),
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

    // The kernel should now see two real partitions for this loop device.
    assert!(std::path::Path::new(&format!("{}p1", loop_device.path)).exists());
    assert!(std::path::Path::new(&format!("{}p2", loop_device.path)).exists());

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs root and losetup; see module docs"]
fn refuses_a_non_windows_iso() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    // A plain, non-Windows-shaped ISO9660 image -- classify() should refuse
    // it before this gets anywhere near partitioning the device.
    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(false, false)).unwrap();

    let plan = WriteWindowsPlan {
        device_path: loop_device.path.clone(),
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
        let mut device = std::fs::File::open(&loop_device.path).unwrap();
        device.seek(SeekFrom::Start(512)).unwrap();
        device.read_exact(&mut header).unwrap();
    }
    assert_ne!(&header, b"EFI PART");

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs root, losetup, mkfs.ntfs, and ntfs-3g; see module docs"]
fn verify_matches_a_prior_windows_write() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    if !command_available("mkfs.ntfs") || !command_available("ntfs-3g") {
        eprintln!("skipping: mkfs.ntfs/ntfs-3g not installed");
        return;
    }
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let write_plan = WriteWindowsPlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.path().to_path_buf(),
    };
    argos_privileged::windows::execute_write_windows_image(&write_plan, &NoopProgress)
        .expect("the write itself should succeed");

    let verify_plan = VerifyWindowsPlan {
        device_path: loop_device.path.clone(),
        iso_path: iso.path().to_path_buf(),
    };
    let outcome =
        argos_privileged::windows::execute_verify_windows_image(&verify_plan, &NoopProgress)
            .expect("verify against what was just written should succeed");
    assert_eq!(outcome.files_verified, 2);

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs root, losetup, mkfs.ntfs, and ntfs-3g; see module docs"]
fn verify_rejects_a_file_corrupted_after_the_write() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    if !command_available("mkfs.ntfs") || !command_available("ntfs-3g") {
        eprintln!("skipping: mkfs.ntfs/ntfs-3g not installed");
        return;
    }
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let write_plan = WriteWindowsPlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.path().to_path_buf(),
    };
    argos_privileged::windows::execute_write_windows_image(&write_plan, &NoopProgress)
        .expect("the write itself should succeed");

    // Corrupt one file directly on the mounted NTFS partition, simulating
    // media corruption (or tampering) that happened after the write --
    // exactly the class of failure verification exists to catch.
    let platform = argos_platform_linux::LinuxPlatform::new();
    let device = platform
        .refresh(&loop_device.path, None)
        .unwrap()
        .expect("the loop device should still be there");
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
        device_path: loop_device.path.clone(),
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
