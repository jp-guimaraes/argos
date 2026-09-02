//! Checks media Argos writes against the operating system's own FAT
//! consistency checker -- an implementation with no stake in ours.
//!
//! Why this earns a place in the suite: our own code round-trips happily
//! through filesystems it produced, and macOS mounts them, so neither
//! notices a structural defect. `fsck_msdos` does, and Windows does. On real
//! hardware the symptom was a volume `diskpart` listed as FAT32 with no
//! drive letter, and Setup reporting a missing media driver -- nothing that
//! points at directory layout.
//!
//! What it caught, in media that otherwise worked everywhere we could test:
//!
//! ```text
//! Warning: Item /sources does not appear to be a subdirectory
//! Warning: `..' entry in /sources has non-zero start cluster
//! ```
//!
//! `fatfs` writes long-filename entries in front of `.` and `..`, which the
//! specification requires to be a directory's first two entries, and points
//! `..` at the root's cluster where the specification requires zero.
//!
//! macOS only; `fsck_msdos` is where the check lives. Skips itself if the
//! tool or `hdiutil` is missing.

#![cfg(target_os = "macos")]

use argos_core::image::windows::fixtures::udf_windows_installer_iso;
use argos_core::progress::NoopProgress;
use argos_privileged::protocol::WindowsLayout;
use argos_privileged::windows_fat32::{plan_copy_actions, total_bytes_on_target, TargetLayout};
use std::process::Command;

fn media_passes_fsck(kind: WindowsLayout) -> Option<(bool, String)> {
    let dir = tempfile::tempdir().unwrap();
    let iso_path = dir.path().join("fixture.iso");
    std::fs::write(&iso_path, udf_windows_installer_iso(true, true)).unwrap();
    let iso = argos_core::image::windows::WindowsIso::open(&iso_path).unwrap();
    let files = iso.list_files().unwrap();
    let actions = plan_copy_actions(&iso, &files).unwrap();
    let layout = TargetLayout::for_layout(kind, total_bytes_on_target(&actions));

    let img = dir.path().join("media.img");
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&img)
        .unwrap();
    disk.set_len(layout.total_bytes_required()).unwrap();
    argos_privileged::windows_fat32::write_fat32_media_for_test(
        &mut disk,
        &layout,
        &iso,
        &actions,
        &NoopProgress,
    )
    .unwrap();
    drop(disk);

    let attach = Command::new("hdiutil")
        .args([
            "attach",
            "-nomount",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
        ])
        .arg(&img)
        .output()
        .ok()?;
    if !attach.status.success() {
        return None;
    }
    let device = String::from_utf8_lossy(&attach.stdout)
        .split_whitespace()
        .next()?
        .to_string();

    // `-n` answers "no" to every repair prompt: report, never modify.
    let check = Command::new("fsck_msdos")
        .arg("-n")
        .arg(format!("{device}s1"))
        .output()
        .ok();
    let _ = Command::new("hdiutil")
        .args(["detach", &device, "-force"])
        .output();

    let check = check?;
    Some((
        check.status.success(),
        String::from_utf8_lossy(&check.stdout).into_owned(),
    ))
}

#[test]
#[ignore = "needs hdiutil and fsck_msdos; see module docs"]
fn gpt_media_is_a_conformant_fat32_filesystem() {
    let Some((passed, report)) = media_passes_fsck(WindowsLayout::Fat32) else {
        eprintln!("skipping: hdiutil/fsck_msdos unavailable");
        return;
    };
    eprintln!("{report}");
    assert!(
        passed,
        "fsck_msdos rejected media written with the GPT layout"
    );
}

#[test]
#[ignore = "needs hdiutil and fsck_msdos; see module docs"]
fn bios_media_is_a_conformant_fat32_filesystem() {
    let Some((passed, report)) = media_passes_fsck(WindowsLayout::Fat32Bios) else {
        eprintln!("skipping: hdiutil/fsck_msdos unavailable");
        return;
    };
    eprintln!("{report}");
    assert!(
        passed,
        "fsck_msdos rejected media written with the BIOS layout"
    );
}

/// The two defects by name, so a regression says which rule broke rather
/// than only that the checker was unhappy.
#[test]
#[ignore = "needs hdiutil and fsck_msdos; see module docs"]
fn the_directory_defects_that_kept_windows_from_mounting_stay_fixed() {
    let Some((_, report)) = media_passes_fsck(WindowsLayout::Fat32Bios) else {
        eprintln!("skipping: hdiutil/fsck_msdos unavailable");
        return;
    };
    assert!(
        !report.contains("does not appear to be a subdirectory"),
        "`.` and `..` are not the first two entries of a directory again:\n{report}"
    );
    assert!(
        !report.contains("non-zero start cluster"),
        "a top-level directory's `..` points at the root's cluster again, not zero:\n{report}"
    );
}
