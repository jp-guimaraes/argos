//! Regression test for the macOS auto-mount failure (phase 3 M4, #34):
//! reported by a user mid-write against a real USB stick.
//!
//! Once `format_volume` makes the FAT32 partition recognizable,
//! `diskarbitrationd` mounts it (`/Volumes/ARGOS-WIN`) without being asked.
//! Any later open of the whole-disk device node then fails with `EBUSY`,
//! which killed both the running copy and the subsequent `argos verify`.
//!
//! This test forces exactly that situation -- write, let macOS mount the
//! result, then write and verify again over it -- and asserts both survive.
//! It fails against the pre-fix code and passes with the exclusive open.
//!
//! ```sh
//! cargo test -p argos-privileged --features test-overrides \
//!     --test automount_regression -- --ignored --nocapture
//! ```

#![cfg(target_os = "macos")]

use argos_core::image::windows::fixtures::udf_windows_installer_iso as windows_installer_iso;
use argos_core::partition::windows::FAT32_MIN_PARTITION_BYTES;
use argos_core::progress::NoopProgress;
use argos_privileged::protocol::{VerifyWindowsPlan, WindowsLayout, WriteWindowsPlan};
use std::process::Command;

const DEVICE_SIZE: u64 = FAT32_MIN_PARTITION_BYTES + 4 * 1024 * 1024;

struct AttachedImage {
    device_node: String,
    _backing_file: tempfile::NamedTempFile,
}

impl AttachedImage {
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
        let device_node = String::from_utf8(output.stdout)
            .ok()?
            .split_whitespace()
            .next()?
            .to_string();
        Some(Self {
            device_node,
            _backing_file: backing_file,
        })
    }

    /// Asks macOS to mount whatever it now recognizes on this disk, the way
    /// diskarbitrationd does on its own for a real stick, and reports
    /// whether anything actually got mounted.
    fn provoke_automount(&self) -> bool {
        let _ = Command::new("diskutil")
            .args(["mountDisk", &self.device_node])
            .output();
        let mounts = Command::new("mount").output().expect("mount(8) should run");
        String::from_utf8_lossy(&mounts.stdout).contains(&self.device_node)
    }
}

impl Drop for AttachedImage {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", &self.device_node, "-force"])
            .output();
    }
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn write_and_verify_survive_a_mounted_partition_from_a_previous_write() {
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach an hdiutil image");
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
        layout: WindowsLayout::Fat32,
    };
    let verify_plan = VerifyWindowsPlan {
        device_path: image.device_node.clone(),
        iso_path: iso.path().to_path_buf(),
        layout: WindowsLayout::Fat32,
    };

    argos_privileged::windows_fat32::execute_write_windows_fat32(&write_plan, &NoopProgress)
        .expect("the first write should succeed");

    // Now do what macOS does on its own with a real stick.
    let mounted = image.provoke_automount();
    eprintln!("partition mounted after the write: {mounted}");

    // Both of these failed with EBUSY (os error 16) before the fix.
    argos_privileged::windows_fat32::execute_verify_windows_fat32(&verify_plan, &NoopProgress)
        .expect("verify must work with the just-written partition mounted");

    let _ = image.provoke_automount();
    argos_privileged::windows_fat32::execute_write_windows_fat32(&write_plan, &NoopProgress)
        .expect("rewriting over a mounted partition must work");

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

/// Pins the property a write depends on: while the device is held open, the
/// OS must not be able to mount a partition on it -- a mount mid-copy makes
/// every later write to that partition's region fail with `EBUSY`.
///
/// **Honest scope note.** This test passes both with and without `O_EXCL`,
/// because a plain `O_RDWR` open of a whole-disk node already blocks mounts
/// here. It therefore does *not* discriminate the exclusive-open fix, and it
/// cannot reproduce the real-hardware failure it was written for: an
/// `hdiutil` image attached with `-nomount` is exempt from disk arbitration,
/// whereas a real USB stick is not (macOS 15's FSKit `msdos` driver mounts
/// it the moment the fresh FAT32 becomes recognizable). Kept because it
/// still guards the invariant on every platform CI runs, and because the
/// asymmetry is worth recording; the fix itself can only be confirmed on
/// physical media (M5, #44).
#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn holding_the_device_open_blocks_macos_from_mounting_it() {
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach an hdiutil image");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    // Put a real, mountable FAT32 partition on the device first.
    argos_privileged::windows_fat32::execute_write_windows_fat32(
        &WriteWindowsPlan {
            device_path: image.device_node.clone(),
            expected_serial: None,
            expected_size_bytes: DEVICE_SIZE,
            iso_path: iso.path().to_path_buf(),
            layout: WindowsLayout::Fat32,
        },
        &NoopProgress,
    )
    .expect("the setup write should succeed");
    let _ = Command::new("diskutil")
        .args(["unmountDisk", &image.device_node])
        .output();

    {
        let _held = argos_privileged::partition_io::open_device_exclusive(&image.device_node)
            .expect("opening the unmounted device exclusively should succeed");
        let mounted_while_held = image.provoke_automount();
        assert!(
            !mounted_while_held,
            "macOS mounted a partition while the device was held open exclusively -- \
             a write in progress could be interrupted by EBUSY at any moment"
        );
    }

    // Released: mounting works again, proving the block above came from the
    // exclusive open and not from the disk being unmountable for some other
    // reason (which would make the assertion above vacuous).
    assert!(
        image.provoke_automount(),
        "the partition should mount once the exclusive handle is dropped"
    );

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}
