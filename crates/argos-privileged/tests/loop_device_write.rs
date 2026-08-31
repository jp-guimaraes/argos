//! Backlog E9: exercises the real `argos-helper` write+verify path against a
//! throwaway file-backed loop device instead of physical USB hardware.
//!
//! This is the only place in the project that calls `argos_privileged::execute`
//! against something that behaves like a real block device end to end
//! (re-validation through the real `LinuxPlatform`, an actual `open()` of a
//! `/dev/loopN` node, a real write + read-back).
//!
//! Requires root (loop device setup needs `CAP_SYS_ADMIN` / access to
//! `/dev/loop-control`) and the `losetup` binary, neither of which CI's
//! regular, unprivileged test job has. Run explicitly, e.g.:
//!
//! ```sh
//! sudo -E cargo test -p argos-privileged --features test-overrides \
//!     --test loop_device_write -- --ignored --nocapture
//! ```
//!
//! Every test skips itself (rather than failing) when the prerequisites
//! aren't met, so accidentally running this without `--ignored`+root on a
//! developer machine is harmless.

#![cfg(target_os = "linux")]

use argos_core::progress::{CancelToken, NoopProgress, ProgressSink};
use argos_core::write::dd_mode::BLOCK_SIZE;
use argos_privileged::protocol::{VerifyPlan, WritePlan};
use std::io::Write;
use std::process::Command;

struct LoopDevice {
    path: String,
    _backing_file: tempfile::NamedTempFile,
}

impl LoopDevice {
    /// Creates a `size_bytes`-large backing file and attaches it as a loop
    /// device. Returns `None` (never panics) when we can't -- e.g. not root,
    /// `losetup` missing -- so callers can skip cleanly.
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

/// Cancels `cancel` from inside `on_progress` -- `write_stream` calls that
/// right after landing a block and right before checking the token again for
/// the next one (backlog #35), so this reproduces "the user hit Ctrl+C while
/// a write was already in flight" deterministically, without a real timer or
/// a second thread racing the write loop.
struct CancelOnFirstProgress {
    cancel: CancelToken,
}

impl ProgressSink for CancelOnFirstProgress {
    fn on_progress(&self, _bytes_done: u64, _bytes_total: u64) {
        self.cancel.cancel();
    }
}

#[test]
#[ignore = "needs root and losetup; see module docs"]
fn writes_and_verifies_against_a_real_loop_device() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    const DEVICE_SIZE: u64 = 8 * 1024 * 1024;
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };

    // ARGOS_TEST_FORCE_REMOVABLE makes LinuxPlatform::refresh() report this
    // loop device as a removable USB disk, which it otherwise never would be
    // -- see argos-platform-linux's test-overrides feature.
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let mut image = tempfile::NamedTempFile::new().unwrap();
    let image_contents: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    image.write_all(&image_contents).unwrap();
    image.flush().unwrap();

    let plan = WritePlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        image_path: image.path().to_path_buf(),
        image_size_bytes: image_contents.len() as u64,
        verify: true,
    };

    let hash = argos_privileged::execute(&plan, &NoopProgress, &CancelToken::new())
        .expect("write + verify against the loop device should succeed");
    assert_eq!(hash.len(), 64, "expected a hex SHA-256 digest");

    let written = std::fs::read(&loop_device.path).unwrap();
    assert_eq!(&written[..image_contents.len()], &image_contents[..]);

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs root and losetup; see module docs"]
fn refuses_when_the_plan_size_does_not_match_the_device_anymore() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    const DEVICE_SIZE: u64 = 4 * 1024 * 1024;
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let mut image = tempfile::NamedTempFile::new().unwrap();
    image.write_all(&[0u8; 1024]).unwrap();
    image.flush().unwrap();

    // Claims a device size that doesn't match reality -- simulates the
    // TOCTOU case where a different, larger drive was confirmed by the user
    // but this path now points somewhere else.
    let plan = WritePlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE * 2,
        image_path: image.path().to_path_buf(),
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
#[ignore = "needs root and losetup; see module docs"]
fn verify_matches_a_prior_write_against_a_real_loop_device() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    const DEVICE_SIZE: u64 = 8 * 1024 * 1024;
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    let mut source = tempfile::NamedTempFile::new().unwrap();
    let image_contents: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 199) as u8).collect();
    source.write_all(&image_contents).unwrap();
    source.flush().unwrap();

    // Write once without argos-helper's own built-in verification, so the
    // standalone `argos verify` path below is doing real, independent work
    // rather than confirming something already checked moments earlier.
    let write_plan = WritePlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        image_path: source.path().to_path_buf(),
        image_size_bytes: image_contents.len() as u64,
        verify: false,
    };
    argos_privileged::execute(&write_plan, &NoopProgress, &CancelToken::new())
        .expect("the write itself should succeed");

    let verify_plan = VerifyPlan {
        device_path: loop_device.path.clone(),
        iso_path: source.path().to_path_buf(),
        iso_size_bytes: image_contents.len() as u64,
    };
    let hash = argos_privileged::execute_verify(&verify_plan, &NoopProgress)
        .expect("verify against what was just written should succeed");
    assert_eq!(hash.len(), 64, "expected a hex SHA-256 digest");

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}

#[test]
#[ignore = "needs root and losetup; see module docs"]
fn verify_rejects_a_device_that_does_not_match_the_iso() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    const DEVICE_SIZE: u64 = 4 * 1024 * 1024;
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    // Write one set of bytes to the device...
    let mut written = tempfile::NamedTempFile::new().unwrap();
    written.write_all(&[0xAAu8; 1024]).unwrap();
    written.flush().unwrap();
    let write_plan = WritePlan {
        device_path: loop_device.path.clone(),
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
        device_path: loop_device.path.clone(),
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

/// Backlog #35: the same `execute` this whole module already exercises
/// end-to-end against a real loop device, but with the `CancelToken` --
/// which `main.rs`'s stdin-watcher thread would otherwise flip on a real
/// `SIGINT` -- flipped from *inside* the write itself. Proof that a
/// cancellation requested mid-write actually reaches
/// `dd_mode::write_stream`'s per-block check (rather than, say, only being
/// observed before the write starts), and that `execute` propagates
/// `ArgosError::Cancelled` rather than swallowing or downgrading it.
#[test]
#[ignore = "needs root and losetup; see module docs"]
fn cancelling_mid_write_stops_cleanly_with_a_partial_write() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    const DEVICE_SIZE: u64 = 16 * 1024 * 1024;
    let Some(loop_device) = LoopDevice::attach(DEVICE_SIZE) else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &loop_device.path);

    // More than one BLOCK_SIZE, so there's a second block left uncopied when
    // the first one's progress callback cancels.
    let image_size = BLOCK_SIZE * 2 + 1024;
    let mut image = tempfile::NamedTempFile::new().unwrap();
    let image_contents: Vec<u8> = (0..image_size).map(|i| (i % 233) as u8).collect();
    image.write_all(&image_contents).unwrap();
    image.flush().unwrap();

    let plan = WritePlan {
        device_path: loop_device.path.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        image_path: image.path().to_path_buf(),
        image_size_bytes: image_contents.len() as u64,
        verify: false,
    };

    let cancel = CancelToken::new();
    let progress = CancelOnFirstProgress {
        cancel: cancel.clone(),
    };

    let err = argos_privileged::execute(&plan, &progress, &cancel).unwrap_err();
    assert!(
        matches!(err, argos_core::error::ArgosError::Cancelled),
        "expected Cancelled, got {err:?}"
    );

    // The first block landed before the cancel took effect; the rest of the
    // image did not.
    let written = std::fs::read(&loop_device.path).unwrap();
    assert_eq!(&written[..BLOCK_SIZE], &image_contents[..BLOCK_SIZE]);
    assert_ne!(
        &written[BLOCK_SIZE..image_size],
        &image_contents[BLOCK_SIZE..]
    );

    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
}
