//! The Linux half of the FAT conformance checks -- the counterpart to
//! `fat32_conformance.rs`, which does the same job on macOS with
//! `fsck_msdos`. Same structure, two things Linux adds.
//!
//! **`fsck.vfat` is the stricter reader.** `fsck_msdos` exiting 0 is a
//! weaker statement than it looks: dosfstools names every rule it thinks was
//! broken, so a defect macOS tolerates quietly shows up here as text. The
//! directory defects that kept Windows from mounting our media (#56) read
//! like this before `repair_directory_entries`:
//!
//! ```text
//! Expected a valid '.' entry in this slot.
//! Expected a valid '..' entry in this slot.
//! Bad short file name (..).
//! /  and /sources/FSCK0000.000 share clusters.
//! ```
//!
//! **The kernel's vfat driver can check content, which no checker does.**
//! `fsck` validates structure; it never asks whether the files we meant to
//! write are the files that are there. Our own verify pass reads the medium
//! back through the same `fatfs` that wrote it, so it agrees with itself by
//! construction -- the blind spot `iso_listing_check.rs` closes on the UDF
//! side by counting against the OS's own driver. This does the FAT32
//! equivalent: mount read-only, walk the tree, and compare paths, sizes and
//! bytes against the copy plan.
//!
//! The `fsck` checks need no privileges -- the partition is extracted to its
//! own file rather than attached as a device. The mount check needs root.
//! Each test skips itself when its prerequisite is missing.

#![cfg(target_os = "linux")]

use argos_core::image::windows::fixtures::udf_windows_installer_iso;
use argos_core::image::windows::WindowsIso;
use argos_core::progress::NoopProgress;
use argos_privileged::protocol::WindowsLayout;
use argos_privileged::windows_fat32::{
    plan_copy_actions, total_bytes_on_target, CopyAction, TargetLayout,
};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Media produced by the real write path, alongside what the copy plan says
/// should be on it.
struct Media {
    /// Kept alive: dropping it deletes everything below.
    dir: tempfile::TempDir,
    /// The whole disk image, partition table included.
    image: PathBuf,
    /// The source ISO the medium was written from.
    iso: PathBuf,
    /// What the write path decided to do with each of the ISO's files.
    actions: Vec<CopyAction>,
    partition_offset: u64,
    partition_size: u64,
}

fn write_media(kind: WindowsLayout) -> Media {
    let dir = tempfile::tempdir().unwrap();
    let iso_path = dir.path().join("fixture.iso");
    std::fs::write(&iso_path, udf_windows_installer_iso(true, true)).unwrap();
    let iso = WindowsIso::open(&iso_path).unwrap();
    let files = iso.list_files().unwrap();
    let actions = plan_copy_actions(&iso, &files).unwrap();
    let layout = TargetLayout::for_layout(kind, total_bytes_on_target(&actions));

    let image = dir.path().join("media.img");
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&image)
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

    let region = layout.region();
    Media {
        dir,
        image,
        iso: iso_path,
        actions,
        partition_offset: region.start_offset_bytes,
        partition_size: region.size_bytes,
    }
}

impl Media {
    /// Copies the partition out to its own file, since `fsck.vfat` takes a
    /// filesystem rather than a disk -- and doing it this way needs no loop
    /// device, and so no privileges.
    fn extract_partition(&self) -> PathBuf {
        let out = self.dir.path().join("partition.img");
        let mut src = std::fs::File::open(&self.image).unwrap();
        src.seek(SeekFrom::Start(self.partition_offset)).unwrap();
        let mut dst = std::fs::File::create(&out).unwrap();
        let copied = std::io::copy(&mut src.take(self.partition_size), &mut dst).unwrap();
        assert_eq!(
            copied, self.partition_size,
            "the image is shorter than its own partition"
        );
        dst.flush().unwrap();
        out
    }

    /// What the copy plan intends to put on the filesystem, as lowercased
    /// path -> size. Built from [`CopyAction`] rather than the ISO listing,
    /// so a split `install.wim` is expected as its `.swm` parts -- which is
    /// what actually lands on FAT32.
    fn expected_tree(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for action in &self.actions {
            match action {
                CopyAction::Direct { path, size } => {
                    out.insert(path.to_lowercase(), *size);
                }
                CopyAction::SplitWim {
                    part_paths,
                    part_sizes,
                    ..
                } => {
                    for (path, size) in part_paths.iter().zip(part_sizes) {
                        out.insert(path.to_lowercase(), *size);
                    }
                }
            }
        }
        out
    }
}

/// Writes media for `kind` and runs `fsck.vfat` over it. `None` means the
/// tool isn't installed, which is a skip rather than a failure -- the same
/// shape as the macOS file's `media_passes_fsck`.
fn media_passes_fsck(kind: WindowsLayout) -> Option<(bool, String)> {
    let media = write_media(kind);
    let partition = media.extract_partition();
    // `-n` answers no to every repair prompt: report, never modify.
    let check = Command::new("fsck.vfat")
        .arg("-n")
        .arg(&partition)
        .output()
        .ok()?;
    let mut report = String::from_utf8_lossy(&check.stdout).into_owned();
    report.push_str(&String::from_utf8_lossy(&check.stderr));
    Some((check.status.success(), report))
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() takes no arguments and has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Unmounts on the way out, so a failed assertion doesn't leave the loop
/// device and mountpoint behind.
struct Mounted {
    point: PathBuf,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.point).status();
    }
}

/// Mounts the partition read-only through the kernel's own vfat driver.
/// `mount`'s `offset`/`sizelimit` options set up the loop device, so no
/// separate `losetup --partscan` step is needed.
fn mount_readonly(media: &Media, point: &Path) -> Option<Mounted> {
    let status = Command::new("mount")
        .args(["-t", "vfat", "-o"])
        .arg(format!(
            "loop,ro,offset={},sizelimit={}",
            media.partition_offset, media.partition_size
        ))
        .arg(&media.image)
        .arg(point)
        .status()
        .ok()?;
    status.success().then(|| Mounted {
        point: point.to_path_buf(),
    })
}

/// Every regular file under `root`, as lowercased relative path -> size.
/// Lowercased because FAT is case-insensitive: whether the driver hands back
/// the long name as written or the uppercase 8.3 short name is a question
/// about name presentation, not about whether the file arrived.
fn walk(root: &Path, prefix: &str, out: &mut BTreeMap<String, u64>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let meta = entry.metadata().unwrap();
        if meta.is_dir() {
            walk(&entry.path(), &path, out);
        } else {
            out.insert(path, meta.len());
        }
    }
}

#[test]
#[ignore = "needs fsck.vfat; see module docs"]
fn gpt_media_is_a_conformant_fat32_filesystem() {
    let Some((passed, report)) = media_passes_fsck(WindowsLayout::Fat32) else {
        eprintln!("skipping: fsck.vfat unavailable");
        return;
    };
    eprintln!("{report}");
    assert!(
        passed,
        "fsck.vfat rejected media written with the GPT layout"
    );
}

#[test]
#[ignore = "needs fsck.vfat; see module docs"]
fn bios_media_is_a_conformant_fat32_filesystem() {
    let Some((passed, report)) = media_passes_fsck(WindowsLayout::Fat32Bios) else {
        eprintln!("skipping: fsck.vfat unavailable");
        return;
    };
    eprintln!("{report}");
    assert!(
        passed,
        "fsck.vfat rejected media written with the BIOS layout"
    );
}

/// The defects by the words `fsck.vfat` uses for them, so a regression says
/// which rule broke rather than only that the checker was unhappy. The macOS
/// file asserts the same two defects through `fsck_msdos`'s wording.
#[test]
#[ignore = "needs fsck.vfat; see module docs"]
fn the_directory_defects_that_kept_windows_from_mounting_stay_fixed() {
    let Some((_, report)) = media_passes_fsck(WindowsLayout::Fat32Bios) else {
        eprintln!("skipping: fsck.vfat unavailable");
        return;
    };
    // `.` and `..` must be a directory's first two entries; fatfs wrote
    // long-name entries in front of them.
    assert!(
        !report.contains("Expected a valid '.' entry"),
        "`.` is not a directory's first entry again:\n{report}"
    );
    assert!(
        !report.contains("Expected a valid '..' entry"),
        "`..` is not a directory's second entry again:\n{report}"
    );
    // `..` must hold 0 when the parent is the root; pointing it at the
    // root's real cluster reads to fsck as clusters shared with the root.
    assert!(
        !report.contains("share clusters"),
        "a top-level directory's `..` points at the root's cluster again, not zero:\n{report}"
    );
    assert!(
        !report.contains("Bad short file name"),
        "a directory entry carries a name FAT does not allow:\n{report}"
    );
}

/// The content cross-check `fsck` cannot do: does an independent
/// implementation see the files the plan says were written, at the right
/// sizes, holding the right bytes?
#[test]
#[ignore = "needs root (mount); see module docs"]
fn the_kernel_vfat_driver_reads_back_exactly_what_the_plan_wrote() {
    if !running_as_root() {
        eprintln!("skipping: needs root to mount");
        return;
    }
    let media = write_media(WindowsLayout::Fat32);
    let point = tempfile::tempdir().unwrap();
    let Some(_mounted) = mount_readonly(&media, point.path()) else {
        eprintln!("skipping: mounting the partition failed");
        return;
    };

    let mut seen = BTreeMap::new();
    walk(point.path(), "", &mut seen);
    assert_eq!(
        seen,
        media.expected_tree(),
        "the kernel's vfat driver and the copy plan disagree about the medium's contents"
    );

    // Sizes matching while bytes differ is precisely the failure a
    // same-implementation read-back cannot catch. Only files copied through
    // verbatim are compared this way: a split WIM's `.swm` parts have no
    // counterpart in the ISO to compare against, and the tree check above
    // already covers their names and sizes.
    let iso = WindowsIso::open(&media.iso).unwrap();
    for action in &media.actions {
        let CopyAction::Direct { path, .. } = action else {
            continue;
        };
        let mut expected = Vec::new();
        iso.open_file(path)
            .unwrap()
            .expect("a Direct action always names a file the ISO has")
            .read_to_end(&mut expected)
            .unwrap();
        let actual = std::fs::read(point.path().join(path.to_lowercase())).unwrap();
        assert_eq!(
            actual, expected,
            "{path} reads back differently through the kernel's vfat driver"
        );
    }
}
