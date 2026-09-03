//! Phase 3 L3 (`docs/plan-linux-validation.md`): the stale-GPT scenario as
//! the *kernel* sees it, on a real block device.
//!
//! The defect (#59) cost this phase several rounds of lab testing. `mbrman`
//! writes sector 0 and nothing else, so a stick previously written with
//! `--layout fat32` kept its whole GPT -- primary header at LBA 1, entry
//! array behind it, backup header in the last sector, every CRC still
//! validating -- underneath an MBR whose first entry is a bootable FAT32
//! partition rather than the protective `0xEE` a GPT requires. The media
//! still booted, which is what made it so hard to localize; Windows simply
//! declined the volume a drive letter, and Setup reported a missing
//! installation source.
//!
//! `windows_fat32`'s own unit test already covers the byte-level fix over a
//! plain file. This is the half that file cannot reach: **Linux caches
//! partition tables per block device and re-probes them on demand**, so a
//! stale GPT can surface differently on a real device than in a file, and
//! the plan calls for confirming the fix under the kernel's own device
//! handling rather than only under ours.
//!
//! What that buys: `blkid` in probe mode is `libblkid`, the same code path
//! `lsblk`, udev and the desktop stack use to decide what a device carries.
//! It has no stake in our writer. A device just written as MBR that it still
//! reports as `gpt` is the exact failure signature the lab saw, and the one
//! `tools/mediadiff.py` was written to catch by hand.
//!
//! The device is detached and re-attached between writes, so every probe
//! reads the medium fresh rather than a cached table -- the loop-device
//! equivalent of unplugging the stick and plugging it back in. Unlike
//! `write_windows_fat32.rs`, this attaches **with** `--partscan`: having the
//! kernel actually parse the table is the point here.
//!
//! ```sh
//! sudo -E cargo test -p argos-privileged --features test-overrides \
//!     --test recycled_device_gpt -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Skips itself (rather than failing) when root, `losetup` or `blkid` is
//! missing.

#![cfg(target_os = "linux")]

use argos_core::image::windows::fixtures::udf_windows_installer_iso as windows_installer_iso;
use argos_core::partition::windows::FAT32_MIN_PARTITION_BYTES;
use argos_core::progress::{CancelToken, NoopProgress};
use argos_privileged::protocol::{WindowsLayout, WriteWindowsPlan};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

/// Room for the larger of the two layouts, plus alignment slack.
const DEVICE_SIZE: u64 = FAT32_MIN_PARTITION_BYTES + 8 * 1024 * 1024;

/// A backing file that can be attached and detached repeatedly, so each
/// probe sees the medium rather than a table the kernel already parsed.
struct RecyclableDevice {
    backing: tempfile::NamedTempFile,
    attached: Option<String>,
}

impl RecyclableDevice {
    fn new(size_bytes: u64) -> Option<Self> {
        let mut backing = tempfile::NamedTempFile::new().ok()?;
        backing.as_file_mut().set_len(size_bytes).ok()?;
        Some(Self {
            backing,
            attached: None,
        })
    }

    /// Attaches with `--partscan`, so the kernel parses whatever partition
    /// table is on the medium -- which is the behaviour under test.
    fn attach(&mut self) -> Option<String> {
        let output = Command::new("losetup")
            .args(["--find", "--show", "--partscan"])
            .arg(self.backing.path())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if path.is_empty() {
            return None;
        }
        self.attached = Some(path.clone());
        Some(path)
    }

    fn detach(&mut self) {
        if let Some(path) = self.attached.take() {
            let _ = Command::new("losetup").args(["--detach", &path]).status();
        }
    }

    /// Detach and attach again: the loop-device stand-in for unplugging the
    /// stick and plugging it back in.
    fn recycle(&mut self) -> Option<String> {
        self.detach();
        self.attach()
    }
}

impl Drop for RecyclableDevice {
    fn drop(&mut self) {
        self.detach();
    }
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() takes no arguments and has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// What `libblkid` thinks the device's partition table is: `gpt`, `dos`, or
/// nothing at all. The independent opinion this test exists to collect.
fn probed_partition_table(device: &str) -> Option<String> {
    let output = Command::new("blkid")
        .args(["-p", "-o", "value", "-s", "PTTYPE", device])
        .output()
        .ok()?;
    // blkid exits non-zero when it finds nothing, which is a legitimate
    // answer here rather than an error.
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn signature_at(device: &str, offset: SeekFrom) -> [u8; 8] {
    let mut file = std::fs::File::open(device).unwrap();
    file.seek(offset).unwrap();
    let mut signature = [0u8; 8];
    file.read_exact(&mut signature).unwrap();
    signature
}

fn write_media(device: &str, iso: &Path, layout: WindowsLayout) {
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", device);
    let plan = WriteWindowsPlan {
        device_path: device.to_string(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        iso_path: iso.to_path_buf(),
        layout,
    };
    let result = argos_privileged::windows_fat32::execute_write_windows_fat32(
        &plan,
        &NoopProgress,
        &CancelToken::new(),
    );
    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
    result.expect("writing the Windows installer media should succeed");
}

#[test]
#[ignore = "needs root, losetup and blkid; see module docs"]
fn recycling_a_device_from_gpt_to_mbr_leaves_the_kernel_seeing_only_an_mbr() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    if probed_partition_table("/dev/null").is_none() {
        eprintln!("skipping: blkid unavailable");
        return;
    }
    let Some(mut device) = RecyclableDevice::new(DEVICE_SIZE) else {
        eprintln!("skipping: could not create a backing file");
        return;
    };
    let Some(path) = device.attach() else {
        eprintln!("skipping: could not attach a loop device (needs losetup + root)");
        return;
    };

    let iso = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(iso.path(), windows_installer_iso(true, true)).unwrap();

    // First life: the GPT layout.
    write_media(&path, iso.path(), WindowsLayout::Fat32);
    let path = device.recycle().expect("re-attaching after the GPT write");

    // The premise. If a GPT write ever stops leaving a GPT the kernel
    // recognizes, the rest of this test stops testing anything.
    assert_eq!(
        probed_partition_table(&path).as_deref(),
        Some("gpt"),
        "the GPT layout should leave a table the kernel reads as GPT"
    );

    // Second life: the BIOS/MBR layout, onto the very same medium.
    write_media(&path, iso.path(), WindowsLayout::Fat32Bios);
    let path = device.recycle().expect("re-attaching after the MBR write");

    // The regression: the kernel must now see an MBR and nothing else. A
    // device just written as MBR still probing as `gpt` is precisely the
    // state Windows refuses to assign a drive letter to.
    assert_eq!(
        probed_partition_table(&path).as_deref(),
        Some("dos"),
        "the kernel still sees a GPT on media just written as MBR -- the #59 \
         hybrid state that boots but that Windows will not mount"
    );

    // And the same conclusion from the bytes themselves, so a future change
    // in libblkid's reporting cannot quietly turn this green.
    assert_ne!(
        &signature_at(&path, SeekFrom::Start(512)),
        b"EFI PART",
        "the primary GPT header survived the MBR write"
    );
    assert_ne!(
        &signature_at(&path, SeekFrom::End(-512)),
        b"EFI PART",
        "the backup GPT header survived the MBR write"
    );
}
