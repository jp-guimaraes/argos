//! Phase 3 M3.6 (backlog #43): exercises the real FAT32 Windows installer
//! write (`execute_write_windows_fat32`) and verify
//! (`execute_verify_windows_fat32`) paths against a throwaway file-backed
//! loop device.
//!
//! Deliberately needs much less than the retired NTFS path's equivalent
//! test did: root and `losetup` only -- no `mkfs.ntfs`, no `ntfs-3g`, no
//! `--partscan` (the FAT32 path never asks the kernel for partition device
//! nodes: it writes the partition's byte range through the whole-device
//! fd). That difference *is* the milestone's acceptance criterion, so this
//! file asserting it stays honest. Run e.g.:
//!
//! ```sh
//! sudo -E cargo test -p argos-privileged --features test-overrides \
//!     --test write_windows_fat32 -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test skips itself (rather than failing) when a prerequisite isn't
//! met, so running this without `--ignored`+root is harmless.

#![cfg(target_os = "linux")]

use argos_core::image::windows::fixtures::udf_windows_installer_iso as windows_installer_iso;
use argos_core::partition::windows::FAT32_MIN_PARTITION_BYTES;
use argos_core::progress::{CancelToken, NoopProgress};
use argos_privileged::protocol::{VerifyWindowsPlan, WindowsLayout, WriteWindowsPlan};
use std::process::Command;

struct LoopDevice {
    path: String,
    _backing_file: tempfile::NamedTempFile,
}

impl LoopDevice {
    /// Creates a `size_bytes`-large (sparse) backing file and attaches it as
    /// a loop device. Returns `None` (never panics) when we can't -- e.g.
    /// not root, `losetup` missing -- so callers can skip cleanly. No
    /// `--partscan`, deliberately: see the module docs.
    fn attach(size_bytes: u64) -> Option<Self> {
        let mut backing_file = tempfile::NamedTempFile::new().ok()?;
        backing_file.as_file_mut().set_len(size_bytes).ok()?;

        let output = Command::new("losetup")
            .args(["--find", "--show"])
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

/// The FAT32 plan's 512 MiB partition floor plus room for alignment and the
/// GPT structures on both ends.
const DEVICE_SIZE: u64 = FAT32_MIN_PARTITION_BYTES + 4 * 1024 * 1024;

fn write_plan(loop_device: &LoopDevice, iso_path: &std::path::Path) -> WriteWindowsPlan {
    WriteWindowsPlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso_path.to_path_buf(),
        layout: WindowsLayout::Fat32,
        eject: false,
    }
}

#[test]
#[ignore = "needs root and losetup; see module docs"]
fn writes_a_windows_installer_iso_via_fat32_with_no_external_tools() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let outcome = argos_privileged::windows_fat32::execute_write_windows_fat32(
        &write_plan(&loop_device, iso.path()),
        &NoopProgress,
        &CancelToken::new(),
    )
    .expect("writing the Windows installer image via FAT32 should succeed");

    assert_eq!(outcome.files_copied, 2); // BOOTMGR + SOURCES/BOOT.WIM
    assert_eq!(outcome.file_hashes.len(), 2);

    // A real GPT landed on the device...
    let mut header = [0u8; 8];
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut device = std::fs::File::open(&loop_device.path).unwrap();
        device.seek(SeekFrom::Start(512)).unwrap();
        device.read_exact(&mut header).unwrap();
    }
    assert_eq!(&header, b"EFI PART");

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

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(false, false)).unwrap();

    let err = argos_privileged::windows_fat32::execute_write_windows_fat32(
        &write_plan(&loop_device, iso.path()),
        &NoopProgress,
        &CancelToken::new(),
    )
    .unwrap_err();
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
#[ignore = "needs root and losetup; see module docs"]
fn verify_matches_a_prior_fat32_write() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    argos_privileged::windows_fat32::execute_write_windows_fat32(
        &write_plan(&loop_device, iso.path()),
        &NoopProgress,
        &CancelToken::new(),
    )
    .expect("the write itself should succeed");

    let verify_plan = VerifyWindowsPlan {
        device_path: loop_device.path.clone(),
        iso_path: iso.path().to_path_buf(),
        layout: WindowsLayout::Fat32,
    };
    let outcome =
        argos_privileged::windows_fat32::execute_verify_windows_fat32(&verify_plan, &NoopProgress)
            .expect("verify against what was just written should succeed");
    assert_eq!(outcome.files_verified, 2);

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}
