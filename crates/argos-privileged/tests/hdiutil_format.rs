//! `argos format` against a real macOS block device: a raw disk image
//! attached with `hdiutil attach -nomount`, made to look like a stick a
//! hybrid ISO was DD'd onto, then formatted through the helper's own
//! `execute_format`. Afterwards the image is detached and re-attached, so
//! what is checked is what DiskArbitration and `fsck_msdos` make of it on a
//! fresh look -- the same thing a user replugging the stick would get.
//!
//! Same shape and same requirements as `hdiutil_image_write.rs`:
//!
//! ```sh
//! cargo test -p argos-privileged --features test-overrides \
//!     --test hdiutil_format -- --ignored --nocapture
//! ```
//!
//! Set `ARGOS_TEST_ISO` to a real hybrid ISO (e.g. an Alpine one) to dirty
//! the device with it; otherwise a synthetic stand-in carrying the same
//! signatures (an MBR with boot code, a GPT header at LBA 1 and at the end
//! of the device, an ISO9660 descriptor at 32 KiB) is used.

#![cfg(target_os = "macos")]

use argos_core::progress::NoopProgress;
use argos_privileged::protocol::FormatPlan;
use std::io::{Seek, SeekFrom, Write};
use std::process::Command;

const MIB: u64 = 1024 * 1024;

/// Attaches `path` raw, unmounted; returns hdiutil's stdout, whose first
/// field is the whole-disk node and whose later lines list partitions.
///
/// `-readwrite` is not optional here, unlike in `hdiutil_image_write.rs`:
/// an image carrying an ISO9660 descriptor -- exactly what this test plants
/// -- is attached read-only by default, and every open for writing then
/// fails with a bare EACCES.
fn attach(path: &std::path::Path) -> Option<String> {
    let output = Command::new("hdiutil")
        .args([
            "attach",
            "-readwrite",
            "-nomount",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
        ])
        .arg(path)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn whole_disk(attach_output: &str) -> Option<String> {
    Some(
        attach_output
            .lines()
            .next()?
            .split_whitespace()
            .next()?
            .to_string(),
    )
}

fn detach(node: &str) {
    let _ = Command::new("hdiutil").args(["detach", node]).status();
}

fn dirty(file: &mut std::fs::File, size: u64) {
    if let Some(iso) = std::env::var("ARGOS_TEST_ISO")
        .ok()
        .filter(|v| !v.is_empty())
    {
        let bytes = std::fs::read(&iso).expect("ARGOS_TEST_ISO must be readable");
        assert!(
            (bytes.len() as u64) < size,
            "the ISO must fit the test device"
        );
        file.write_all(&bytes).unwrap();
    } else {
        let mut head = vec![0u8; 64 * 1024];
        head[..440].fill(0xEB);
        head[0x1BE] = 0x80;
        head[0x1BE + 4] = 0x17;
        head[510] = 0x55;
        head[511] = 0xAA;
        head[512..520].copy_from_slice(b"EFI PART");
        head[32 * 1024] = 1;
        head[32 * 1024 + 1..32 * 1024 + 6].copy_from_slice(b"CD001");
        file.write_all(&head).unwrap();
    }
    file.seek(SeekFrom::Start(size - 512)).unwrap();
    file.write_all(b"EFI PART").unwrap();
    file.flush().unwrap();
}

#[test]
#[ignore = "needs hdiutil and the test-overrides feature; see module docs"]
fn a_dirty_stick_comes_back_as_one_fat32_volume_macos_accepts() {
    const DEVICE_SIZE: u64 = 600 * MIB;
    let backing = tempfile::NamedTempFile::new().unwrap();
    backing.as_file().set_len(DEVICE_SIZE).unwrap();
    dirty(&mut backing.reopen().unwrap(), DEVICE_SIZE);

    let Some(first) = attach(backing.path()) else {
        eprintln!("skipping: could not attach a disk image (needs hdiutil)");
        return;
    };
    let node = whole_disk(&first).unwrap();
    std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", &node);

    let plan = FormatPlan {
        device_path: node.clone(),
        expected_serial: None,
        expected_size_bytes: DEVICE_SIZE,
        label: "lab3".into(),
        eject: false,
    };
    let result = argos_privileged::format::execute_format(&plan, &NoopProgress);
    std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
    detach(&node);
    let outcome = result.expect("formatting the hdiutil-attached image should succeed");
    assert_eq!(outcome.label, "LAB3");
    assert_eq!(outcome.partition_bytes, DEVICE_SIZE - MIB);

    // A fresh look, the way a replugged stick gets one.
    let second = attach(backing.path()).expect("re-attaching the formatted image");
    let node = whole_disk(&second).unwrap();
    eprintln!("{second}");
    let partitions: Vec<&str> = second.lines().skip(1).collect();
    let fsck = Command::new("fsck_msdos")
        .args(["-n", &format!("{node}s1")])
        .output();
    detach(&node);

    assert!(
        second
            .lines()
            .next()
            .unwrap()
            .contains("FDisk_partition_scheme"),
        "expected an MBR, hdiutil saw: {second}"
    );
    assert_eq!(
        partitions.len(),
        1,
        "expected exactly one partition: {second}"
    );
    // hdiutil's name for MBR type 0x0C (FAT32, LBA).
    assert!(
        partitions[0].contains("Windows_FAT_32"),
        "expected a FAT32 partition: {second}"
    );
    let fsck = fsck.expect("fsck_msdos runs");
    assert!(
        fsck.status.success(),
        "fsck_msdos found problems:\n{}{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
}
