//! Phase 3 M4.2 (backlog #34): exercises the FAT32 Windows write and verify
//! paths **on macOS**, against a throwaway raw disk image attached with
//! `hdiutil` -- the macOS counterpart of `write_windows_fat32.rs`
//! (Linux/`losetup`), and the same pattern `hdiutil_image_write.rs` already
//! uses for DD mode.
//!
//! This is what M4 comes down to: the phase-3 plan replaced the NTFS layout
//! precisely so the macOS Windows path would need *nothing macOS-specific*.
//! Unlike the macFUSE route #34 was originally scoped around, these tests
//! need **no macFUSE, no ntfs-3g, no kext approval, and no root** --
//! `hdiutil attach -nomount` is an ordinary unprivileged operation. They
//! only need the `test-overrides` feature (for `ARGOS_TEST_FORCE_REMOVABLE`),
//! so they're `#[ignore]`d like their siblings rather than gated on
//! privilege. Run:
//!
//! ```sh
//! cargo test -p argos-privileged --features test-overrides \
//!     --test hdiutil_windows_fat32 -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test skips itself (rather than failing) when `hdiutil` is missing
//! or attaching fails.

#![cfg(target_os = "macos")]

use argos_core::image::windows::fixtures::udf_windows_installer_iso as windows_installer_iso;
use argos_core::partition::windows::FAT32_MIN_PARTITION_BYTES;
use argos_core::progress::{CancelToken, NoopProgress};
use argos_privileged::protocol::{VerifyWindowsPlan, WindowsLayout, WriteWindowsPlan};
use std::process::Command;

struct AttachedImage {
    device_node: String,
    _backing_file: tempfile::NamedTempFile,
}

impl AttachedImage {
    /// Creates a `size_bytes`-large zeroed raw disk image and attaches it
    /// with `hdiutil attach -nomount`, returning the whole-disk device node
    /// (e.g. `/dev/disk7`). `None` (never a panic) if hdiutil is missing or
    /// the attach fails, so callers can skip cleanly.
    fn attach(size_bytes: u64) -> Option<Self> {
        let backing_file = tempfile::NamedTempFile::new().ok()?;
        backing_file.as_file().set_len(size_bytes).ok()?;

        // `-imagekey diskimage-class=CRawDiskImage` is required: without it
        // hdiutil identifies the format from the file extension, and a
        // tempfile path has none. (Learned the hard way in the E9 tests.)
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
        let device_node = String::from_utf8(output.stdout)
            .ok()?
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
            .args(["detach", &self.device_node, "-force"])
            .output();
    }
}

/// The FAT32 plan's 512 MiB partition floor plus room for alignment and the
/// GPT structures on both ends.
const DEVICE_SIZE: u64 = FAT32_MIN_PARTITION_BYTES + 4 * 1024 * 1024;

fn write_plan(image: &AttachedImage, iso_path: &std::path::Path) -> WriteWindowsPlan {
    WriteWindowsPlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso_path.to_path_buf(),
        layout: WindowsLayout::Fat32,
    }
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn writes_a_windows_installer_iso_via_fat32_on_macos() {
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach an hdiutil image");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    let outcome = argos_privileged::windows_fat32::execute_write_windows_fat32(
        &write_plan(&image, iso.path()),
        &NoopProgress,
        &CancelToken::new(),
    )
    .expect("the FAT32 Windows write should succeed on macOS");

    assert_eq!(outcome.files_copied, 2); // BOOTMGR + SOURCES/BOOT.WIM
    assert_eq!(outcome.file_hashes.len(), 2);

    // A real GPT landed on the device. Note this read goes through the
    // device node too -- the SizedDevice wrapper is what made the *write*
    // possible here at all (macOS device nodes can't answer SEEK_END).
    let mut header = [0u8; 8];
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut device = std::fs::File::open(&image.device_node).unwrap();
        device.seek(SeekFrom::Start(512)).unwrap();
        device.read_exact(&mut header).unwrap();
    }
    assert_eq!(&header, b"EFI PART");

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn verify_matches_a_prior_fat32_write_on_macos() {
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach an hdiutil image");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    argos_privileged::windows_fat32::execute_write_windows_fat32(
        &write_plan(&image, iso.path()),
        &NoopProgress,
        &CancelToken::new(),
    )
    .expect("the write itself should succeed");

    let verify_plan = VerifyWindowsPlan {
        device_path: image.device_node.clone(),
        iso_path: iso.path().to_path_buf(),
        layout: WindowsLayout::Fat32,
    };
    let outcome =
        argos_privileged::windows_fat32::execute_verify_windows_fat32(&verify_plan, &NoopProgress)
            .expect("verify against what was just written should succeed");
    assert_eq!(outcome.files_verified, 2);

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn refuses_a_non_windows_iso_on_macos() {
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach an hdiutil image");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(false, false)).unwrap();

    let err = argos_privileged::windows_fat32::execute_write_windows_fat32(
        &write_plan(&image, iso.path()),
        &NoopProgress,
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        argos_core::error::ArgosError::NotWindowsInstallerIso(_)
    ));

    // Nothing touched the device: no GPT signature at LBA 1.
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
