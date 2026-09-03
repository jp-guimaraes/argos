//! Backlog E9: exercises the real `argos-helper` write+verify path against a
//! throwaway raw disk image attached via `hdiutil`, instead of physical USB
//! hardware -- the macOS counterpart of `loop_device_write.rs` (which does
//! the same thing on Linux via `losetup`).
//!
//! Unlike the Linux path, attaching a raw disk image with `hdiutil attach
//! -nomount` is a normal, unprivileged macOS operation -- it goes through
//! DiskArbitration, not a root-only ioctl the way `losetup`/
//! `/dev/loop-control` does -- so these tests need no root and no separate
//! privileged CI job. They *do* still need the `test-overrides` feature
//! (see `argos-platform-macos`'s `Cargo.toml`) to exercise
//! `ARGOS_TEST_FORCE_REMOVABLE`, so they're still `#[ignore]`d and run
//! explicitly, matching the Linux test's shape. Run e.g.:
//!
//! ```sh
//! cargo test -p argos-privileged --features test-overrides \
//!     --test hdiutil_image_write -- --ignored --nocapture
//! ```
//!
//! Every test skips itself (rather than failing) when `hdiutil` isn't
//! available or attaching fails, so this is harmless to run anywhere.

#![cfg(target_os = "macos")]

use argos_core::progress::{CancelToken, NoopProgress};
use argos_privileged::protocol::{VerifyPlan, WritePlan};
use std::io::Write;
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
    /// skip cleanly.
    fn attach(size_bytes: u64) -> Option<Self> {
        let backing_file = tempfile::NamedTempFile::new().ok()?;
        backing_file.as_file().set_len(size_bytes).ok()?;

        // `-imagekey diskimage-class=CRawDiskImage` is required here: without
        // it, hdiutil identifies the image format from the backing file's
        // extension, and a `tempfile`-generated path has none (it fails with
        // "image not recognized" otherwise, discovered by actually running
        // this against a real file).
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
        // `hdiutil attach` prints one line per resulting device node (just
        // one, for an unpartitioned raw image); the whole-disk node is the
        // first whitespace-separated field of the first line, e.g.
        // "/dev/disk7          \tGUID_partition_scheme".
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

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn writes_and_verifies_against_a_real_hdiutil_image() {
    const DEVICE_SIZE: u64 = 8 * 1024 * 1024;
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };

    // ARGOS_TEST_FORCE_REMOVABLE makes MacOsPlatform::refresh() report this
    // hdiutil-attached image as a removable USB disk (and exempts it from
    // the "Virtual"-disk filter, since hdiutil images report
    // VirtualOrPhysical == "Virtual" the same as an APFS container) -- see
    // argos-platform-macos's test-overrides feature.
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let mut source = tempfile::NamedTempFile::new().unwrap();
    let image_contents: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    source.write_all(&image_contents).unwrap();
    source.flush().unwrap();

    let plan = WritePlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        image_path: source.path().to_path_buf(),
        image_size_bytes: image_contents.len() as u64,
        verify: true,
    };

    let hash = argos_privileged::execute(&plan, &NoopProgress, &CancelToken::new())
        .expect("write + verify against the hdiutil-attached image should succeed");
    assert_eq!(hash.len(), 64, "expected a hex SHA-256 digest");

    let written = std::fs::read(&image.device_node).unwrap();
    assert_eq!(&written[..image_contents.len()], &image_contents[..]);

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn refuses_when_the_plan_size_does_not_match_the_device_anymore() {
    const DEVICE_SIZE: u64 = 4 * 1024 * 1024;
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let mut source = tempfile::NamedTempFile::new().unwrap();
    source.write_all(&[0u8; 1024]).unwrap();
    source.flush().unwrap();

    // Claims a device size that doesn't match reality -- simulates the
    // TOCTOU case where a different, larger drive was confirmed by the user
    // but this path now points somewhere else.
    let plan = WritePlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE * 2,
        image_path: source.path().to_path_buf(),
        image_size_bytes: 1024,
        verify: false,
    };

    let err = argos_privileged::execute(&plan, &NoopProgress, &CancelToken::new()).unwrap_err();
    assert!(matches!(
        err,
        argos_core::error::ArgosError::DeviceNotFound(_)
    ));

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn verify_matches_a_prior_write_against_a_real_hdiutil_image() {
    const DEVICE_SIZE: u64 = 8 * 1024 * 1024;
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    let mut source = tempfile::NamedTempFile::new().unwrap();
    let image_contents: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 199) as u8).collect();
    source.write_all(&image_contents).unwrap();
    source.flush().unwrap();

    // Write once without argos-helper's own built-in verification, so the
    // standalone `argos verify` path below is doing real, independent work
    // rather than confirming something already checked moments earlier.
    let write_plan = WritePlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        image_path: source.path().to_path_buf(),
        image_size_bytes: image_contents.len() as u64,
        verify: false,
    };
    argos_privileged::execute(&write_plan, &NoopProgress, &CancelToken::new())
        .expect("the write itself should succeed");

    let verify_plan = VerifyPlan {
        device_path: image.device_node.clone(),
        iso_path: source.path().to_path_buf(),
        iso_size_bytes: image_contents.len() as u64,
    };
    let hash = argos_privileged::execute_verify(&verify_plan, &NoopProgress)
        .expect("verify against what was just written should succeed");
    assert_eq!(hash.len(), 64, "expected a hex SHA-256 digest");

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn verify_rejects_a_device_that_does_not_match_the_iso() {
    const DEVICE_SIZE: u64 = 4 * 1024 * 1024;
    let Some(image) = AttachedImage::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &image.device_node);

    // Write one set of bytes to the device...
    let mut written = tempfile::NamedTempFile::new().unwrap();
    written.write_all(&[0xAAu8; 1024]).unwrap();
    written.flush().unwrap();
    let write_plan = WritePlan {
        device_path: image.device_node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        image_path: written.path().to_path_buf(),
        image_size_bytes: 1024,
        verify: false,
    };
    argos_privileged::execute(&write_plan, &NoopProgress, &CancelToken::new())
        .expect("the write itself should succeed");

    // ...then ask to verify against a different ISO entirely -- simulates
    // pointing `argos verify` at the wrong image, or a device that was
    // reused for something else since it was written.
    let mut different = tempfile::NamedTempFile::new().unwrap();
    different.write_all(&[0xFFu8; 1024]).unwrap();
    different.flush().unwrap();
    let verify_plan = VerifyPlan {
        device_path: image.device_node.clone(),
        iso_path: different.path().to_path_buf(),
        iso_size_bytes: 1024,
    };

    let err = argos_privileged::execute_verify(&verify_plan, &NoopProgress).unwrap_err();
    assert!(matches!(
        err,
        argos_core::error::ArgosError::ChecksumMismatch { .. }
    ));

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}
