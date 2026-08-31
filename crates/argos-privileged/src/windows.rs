//! The UEFI:NTFS write path (backlog #27, W3): creates a real GPT (via
//! `gptman`), `dd`s the vendored UEFI:NTFS boot image as partition 1, then
//! formats, mounts, and copies the source ISO's file tree onto partition 2 as
//! NTFS. See `docs/architecture.md`'s phase 2 guiding decisions for why this
//! exists at all, and `CONTRIBUTING.md`'s scoped exception for why this
//! crate -- otherwise kept deliberately minimal -- reads the ISO's file tree
//! directly instead of leaving that to the unprivileged `argos` process.
//!
//! Linux only, for now: every step past the initial checks calls into
//! [`argos_platform::PlatformOps`], whose Windows-write methods are
//! unimplemented on every other backend (see `docs/architecture.md`), so
//! this fails fast and honestly rather than partitioning a disk it can't
//! finish setting up.

use crate::protocol::{
    validate_refreshed_device_for_windows_write, VerifyWindowsPlan, WriteWindowsPlan,
};
use argos_core::error::{ArgosError, Result};
use argos_core::image::checksum::sha256_stream;
use argos_core::image::windows::WindowsIso;
use argos_core::partition::windows::{
    PartitionRegion, WindowsPartitionPlan, EFI_SYSTEM_PARTITION_TYPE_GUID,
    MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID, SECTOR_SIZE,
};
use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_core::verify::{
    verify_windows_file_hash, verify_windows_partition_layout, ObservedPartition,
};
use argos_core::write::dd_mode;
use argos_core::{image, preflight};
use argos_platform::PlatformOps;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::Path;

/// The vendored UEFI:NTFS boot image, embedded at compile time so
/// `argos-helper` needs no separate data file alongside the binary. See
/// `assets/PROVENANCE.md` for what's in it and where it came from.
const UEFI_NTFS_IMAGE: &[u8] = include_bytes!("../assets/uefi-ntfs.img");

/// The vendored boot image's size, for building the same `WindowsPartitionPlan`
/// this module does -- exposed so `argos-cli` (backlog #27, W5) can show the
/// two-partition layout in its confirmation prompt before elevating, without
/// this crate's embedded asset needing to be duplicated or guessed at.
pub fn uefi_ntfs_image_size_bytes() -> u64 {
    UEFI_NTFS_IMAGE.len() as u64
}

/// What [`execute_write_windows_image`] returns on success: not a single
/// hash the way [`crate::execute`]'s DD-mode write returns one, since there
/// is no one meaningful whole-device hash for a two-partition layout. W4
/// ("Nova estratégia de verificação") is what a caller should use to
/// actually confirm a write, using `file_hashes` captured here.
#[derive(Debug)]
pub struct WindowsWriteOutcome {
    pub boot_partition_hash: String,
    pub files_copied: u64,
    pub bytes_copied: u64,
    /// `(path relative to the ISO root, SHA-256)` for every file copied,
    /// captured during the copy rather than by re-reading afterwards.
    pub file_hashes: Vec<(String, String)>,
}

pub fn execute_write_windows_image(
    plan: &WriteWindowsPlan,
    progress: &dyn ProgressSink,
) -> Result<WindowsWriteOutcome> {
    // Fails fast and honestly (matching what W5's CLI wiring will surface)
    // rather than partitioning a disk on a platform this write path cannot
    // finish setting up -- see this module's doc comment.
    if !cfg!(target_os = "linux") {
        return Err(ArgosError::NotImplemented(
            "Windows installer write support (non-Linux)",
        ));
    }

    let platform = crate::platform_select::current_platform();

    let refreshed = platform.refresh(&plan.device_path, plan.expected_serial.as_deref())?;
    validate_refreshed_device_for_windows_write(plan, refreshed.as_ref())?;
    let device = refreshed.expect(
        "validate_refreshed_device_for_windows_write already returned Ok, so refreshed must be Some",
    );

    // Never trust the plan's idea of what's on the ISO -- re-classify and
    // re-list it here, the same never-trust-the-caller posture the device
    // re-validation above applies to the device.
    if !image::windows::classify(&plan.iso_path)?.is_windows_installer_iso() {
        return Err(ArgosError::NotWindowsInstallerIso(plan.iso_path.clone()));
    }
    let iso = WindowsIso::open(&plan.iso_path)?;
    let files = iso.list_files()?;
    let files_total_size_bytes: u64 = files.iter().map(|f| f.size).sum();

    let layout = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE.len() as u64, files_total_size_bytes);
    preflight::check_windows_capacity(
        &plan.device_path,
        plan.expected_size_bytes,
        &plan.iso_path,
        &layout,
    )?;

    // Safe-open precondition (backlog #20), same as the DD-mode write path.
    progress.on_phase(Phase::Unmounting);
    platform.unmount(&device)?;

    progress.on_phase(Phase::Partitioning);
    write_partition_table(&plan.device_path, &layout)?;
    platform.reread_partition_table(&device)?;

    progress.on_phase(Phase::Writing);
    let boot_partition_path = linux_partition_device_path(&plan.device_path, 1);
    wait_for_path(&boot_partition_path)?;
    let boot_partition_hash = write_boot_partition(&boot_partition_path, progress)?;

    progress.on_phase(Phase::FormattingNtfs);
    let windows_partition_path = linux_partition_device_path(&plan.device_path, 2);
    wait_for_path(&windows_partition_path)?;
    format_ntfs(&windows_partition_path)?;

    progress.on_phase(Phase::Mounting);
    let mountpoint = platform.mount_ntfs_partition(&device, 2)?;

    let copy_result = copy_files(&iso, &files, &mountpoint, progress);

    // Always try to unmount, even if the copy failed partway through --
    // leaving the mount dangling helps no one, and unmount's own error (if
    // any) is secondary to whatever the copy already failed with.
    let unmount_result = platform.unmount_path(&mountpoint);
    let _ = fs::remove_dir(&mountpoint);

    let copied = copy_result?;
    unmount_result?;

    Ok(WindowsWriteOutcome {
        boot_partition_hash,
        files_copied: copied.files_copied,
        bytes_copied: copied.bytes_copied,
        file_hashes: copied.file_hashes,
    })
}

/// What [`execute_verify_windows_image`] returns on success (backlog #27,
/// W4): just a count, unlike [`WindowsWriteOutcome`] -- verification's whole
/// point is confirming every file already matches, so there's nothing left
/// to report per file beyond that count.
#[derive(Debug)]
pub struct WindowsVerifyOutcome {
    pub files_verified: u64,
}

/// The UEFI:NTFS write path's verification counterpart (backlog #27, W4):
/// re-derives the expected `WindowsPartitionPlan` from the source ISO (never
/// trusting the plan, same posture as the write path), confirms the real GPT
/// on `plan.device_path` matches it
/// ([`verify_windows_partition_layout`]), confirms the boot partition's
/// bytes match the vendored image, then mounts the NTFS partition and
/// confirms every file's hash matches a fresh read of the source ISO.
///
/// Deliberately not a reuse of [`argos_core::verify::verify_written_image`]:
/// that assumes one whole-device hash, and a two-partition layout has no
/// single meaningful hash like that to compare against.
pub fn execute_verify_windows_image(
    plan: &VerifyWindowsPlan,
    progress: &dyn ProgressSink,
) -> Result<WindowsVerifyOutcome> {
    if !cfg!(target_os = "linux") {
        return Err(ArgosError::NotImplemented(
            "Windows installer verify support (non-Linux)",
        ));
    }

    let platform = crate::platform_select::current_platform();
    let device = platform
        .refresh(&plan.device_path, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(plan.device_path.clone()))?;

    if !image::windows::classify(&plan.iso_path)?.is_windows_installer_iso() {
        return Err(ArgosError::NotWindowsInstallerIso(plan.iso_path.clone()));
    }
    let iso = WindowsIso::open(&plan.iso_path)?;
    let files = iso.list_files()?;
    let files_total_size_bytes: u64 = files.iter().map(|f| f.size).sum();
    let layout = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE.len() as u64, files_total_size_bytes);

    progress.on_phase(Phase::Verifying);
    let (boot_observed, windows_observed) = read_observed_partitions(&plan.device_path)?;
    verify_windows_partition_layout(&layout, boot_observed, windows_observed)?;

    let boot_partition_path = linux_partition_device_path(&plan.device_path, 1);
    verify_boot_partition(&boot_partition_path)?;

    progress.on_phase(Phase::Mounting);
    let mountpoint = platform.mount_ntfs_partition(&device, 2)?;

    let verify_result = verify_windows_files(&iso, &files, &mountpoint, progress);

    // Same "always try to unmount" posture as the write path.
    let unmount_result = platform.unmount_path(&mountpoint);
    let _ = fs::remove_dir(&mountpoint);

    verify_result?;
    unmount_result?;

    Ok(WindowsVerifyOutcome {
        files_verified: files.len() as u64,
    })
}

/// Reads the real GPT off `device_path` and extracts partitions 1 and 2 as
/// plain [`ObservedPartition`] values. `GPT::find_from` (rather than
/// `read_from` at a fixed sector size) auto-detects 512- vs 4096-byte
/// sectors, a cheap bit of defensiveness `write_partition_table` doesn't
/// need (it's the one that decided the sector size in the first place).
fn read_observed_partitions(device_path: &str) -> Result<(ObservedPartition, ObservedPartition)> {
    let mut device = File::open(device_path).map_err(ArgosError::Io)?;
    let gpt = gptman::GPT::find_from(&mut device)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    Ok((observed_partition(&gpt, 1)?, observed_partition(&gpt, 2)?))
}

fn observed_partition(gpt: &gptman::GPT, partition_number: u32) -> Result<ObservedPartition> {
    let entry = &gpt[partition_number];
    if !entry.is_used() {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(format!(
            "partition {partition_number} is missing from the GPT"
        )));
    }
    let sector_count = entry.ending_lba - entry.starting_lba + 1;
    Ok(ObservedPartition {
        partition_type_guid: entry.partition_type_guid,
        region: PartitionRegion {
            start_offset_bytes: entry.starting_lba * gpt.sector_size,
            size_bytes: sector_count * gpt.sector_size,
        },
    })
}

/// Confirms partition 1's actual bytes match the vendored UEFI:NTFS image --
/// the same "counterfeit media silently drops bytes" concern
/// `verify_written_image` exists for on the DD-mode path, applied to just
/// this one partition instead of the whole device.
fn verify_boot_partition(partition_path: &str) -> Result<()> {
    let mut partition = File::open(partition_path).map_err(ArgosError::Io)?;
    let mut actual = vec![0u8; UEFI_NTFS_IMAGE.len()];
    partition.read_exact(&mut actual).map_err(ArgosError::Io)?;

    let actual_hash = sha256_stream(Cursor::new(actual), |_| {}).map_err(ArgosError::Io)?;
    let expected_hash =
        sha256_stream(Cursor::new(UEFI_NTFS_IMAGE), |_| {}).map_err(ArgosError::Io)?;
    if actual_hash != expected_hash {
        return Err(ArgosError::ChecksumMismatch {
            expected: expected_hash,
            actual: actual_hash,
        });
    }
    Ok(())
}

/// Confirms every file `image::windows::WindowsIso` lists matches, byte for
/// byte, what's actually on the mounted NTFS partition. Hashes every source
/// file first (the `Checksumming` phase, no progress reported -- mirroring
/// `execute_verify`'s DD-mode equivalent, which hashes the source ISO the
/// same quiet way), then re-reads and compares each one from `mountpoint`
/// (the `Verifying` phase, progress reported), rather than interleaving the
/// two and reporting some hybrid of both passes' progress.
fn verify_windows_files(
    iso: &WindowsIso,
    files: &[image::windows::IsoFileEntry],
    mountpoint: &Path,
    progress: &dyn ProgressSink,
) -> Result<()> {
    progress.on_phase(Phase::Checksumming);
    let mut expected_hashes = Vec::with_capacity(files.len());
    for entry in files {
        let source = iso
            .open_file(&entry.path)
            .map_err(ArgosError::Io)?
            .ok_or_else(|| {
                ArgosError::Io(std::io::Error::other(format!(
                    "{} listed but could not be reopened",
                    entry.path
                )))
            })?;
        expected_hashes.push(sha256_stream(source, |_| {}).map_err(ArgosError::Io)?);
    }

    progress.on_phase(Phase::Verifying);
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut bytes_done = 0u64;
    for (entry, expected_hash) in files.iter().zip(expected_hashes.iter()) {
        let dest = File::open(mountpoint.join(&entry.path)).map_err(ArgosError::Io)?;
        let actual_hash = sha256_stream(dest, |chunk_done| {
            progress.on_progress(bytes_done + chunk_done, total_bytes);
        })
        .map_err(ArgosError::Io)?;

        verify_windows_file_hash(&entry.path, expected_hash, &actual_hash)?;
        bytes_done += entry.size;
    }
    Ok(())
}

/// What [`copy_files`] copied -- folded into a full [`WindowsWriteOutcome`]
/// by its caller, once the boot partition's own hash is available too.
struct CopiedFiles {
    files_copied: u64,
    bytes_copied: u64,
    file_hashes: Vec<(String, String)>,
}

/// Builds and writes the GPT itself: a protective MBR, partition 1 (EFI
/// System Partition, for the UEFI:NTFS boot image) and partition 2
/// (Microsoft Basic Data, for the NTFS Windows files), sized and placed
/// exactly as `layout` computed.
fn write_partition_table(device_path: &str, layout: &WindowsPartitionPlan) -> Result<()> {
    let mut device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)
        .map_err(ArgosError::Io)?;

    let mut gpt = gptman::GPT::new_from(&mut device, SECTOR_SIZE, random_guid()?)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;

    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: EFI_SYSTEM_PARTITION_TYPE_GUID,
        unique_partition_guid: random_guid()?,
        starting_lba: layout.boot_partition.start_offset_bytes / SECTOR_SIZE,
        ending_lba: layout.boot_partition.end_offset_bytes() / SECTOR_SIZE - 1,
        attribute_bits: 0,
        partition_name: "ARGOS-BOOT".into(),
    };
    gpt[2] = gptman::GPTPartitionEntry {
        partition_type_guid: MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID,
        unique_partition_guid: random_guid()?,
        starting_lba: layout.windows_partition.start_offset_bytes / SECTOR_SIZE,
        ending_lba: layout.windows_partition.end_offset_bytes() / SECTOR_SIZE - 1,
        attribute_bits: 0,
        partition_name: "ARGOS-WIN".into(),
    };

    gptman::GPT::write_protective_mbr_into(&mut device, SECTOR_SIZE)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    gpt.write_into(&mut device)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    device.flush().map_err(ArgosError::Io)?;
    Ok(())
}

/// 16 cryptographically random bytes, read from `/dev/urandom` -- Linux
/// only (matching the rest of this write path), and dependency-free rather
/// than pulling in a `uuid`/`rand` crate just for this.
fn random_guid() -> Result<[u8; 16]> {
    let mut buf = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(ArgosError::Io)?;
    Ok(buf)
}

/// Waits (up to 10s, polling every 100ms) for `path` to become a real,
/// openable block device. Partition device nodes (`/dev/sdb1`, ...) aren't
/// guaranteed to be openable the instant `PlatformOps::reread_partition_table`
/// returns -- on a udev-managed system, the kernel's own partition-table
/// reread and udev actually creating/settling the corresponding `/dev`
/// entries are separate, asynchronous steps, and the ioctl only guarantees
/// the first one *started*.
///
/// Deliberately tries an actual [`File::open`], not just [`Path::exists`]:
/// observed for real in CI, a stale or not-yet-settled partition node can
/// exist as a `/dev` entry while the kernel still answers `ENXIO` ("No such
/// device or address") to an open -- a bare existence check races straight
/// through that window and fails the write outright instead of retrying.
fn wait_for_path(path: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match File::open(path) {
            Ok(_) => return Ok(()),
            Err(err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(ArgosError::Io(std::io::Error::other(format!(
                        "{path} did not become a usable block device after the partition table was reread: {err}"
                    ))));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// The Linux partition-device-path convention (`/dev/sdb` + 1 ->
/// `/dev/sdb1`, `/dev/nvme0n1` + 1 -> `/dev/nvme0n1p1`), duplicated from
/// `argos-platform-linux::mounts::partition_device_path` rather than
/// depended on: that crate's helper is private, and this is the one other
/// place (besides mounting, which already goes through `PlatformOps`) that
/// needs to turn a whole-disk path into a specific partition's path, to
/// open it directly for the boot-partition `dd` and `mkfs.ntfs` below.
fn linux_partition_device_path(whole_disk: &str, partition_number: u32) -> String {
    if whole_disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{whole_disk}p{partition_number}")
    } else {
        format!("{whole_disk}{partition_number}")
    }
}

/// `dd`s the vendored UEFI:NTFS image onto partition 1, verbatim -- see this
/// module's top doc comment for why that's all partition 1 ever needs.
/// Returns its SHA-256, for whatever future verification wants it.
fn write_boot_partition(partition_path: &str, progress: &dyn ProgressSink) -> Result<String> {
    let mut partition = OpenOptions::new()
        .write(true)
        .open(partition_path)
        .map_err(ArgosError::Io)?;
    let cancel = CancelToken::new();
    let hash = dd_mode::write_stream(
        Cursor::new(UEFI_NTFS_IMAGE),
        &mut partition,
        UEFI_NTFS_IMAGE.len() as u64,
        progress,
        &cancel,
    )?;
    partition.flush().map_err(ArgosError::Io)?;
    Ok(hash)
}

/// Formats partition 2 as NTFS via the external `mkfs.ntfs` -- the one
/// relaxation of "no shelling out" `docs/architecture.md`'s phase 2 guiding
/// decisions call for, alongside `ntfs-3g` for mounting.
fn format_ntfs(partition_path: &str) -> Result<()> {
    // -Q: quick format (skip zeroing every sector -- the partition is about
    // to be filled with the Windows install's own files anyway).
    // -F: force, since this partition never had a filesystem to confirm
    // overwriting -- gptman just created it moments ago.
    let status = std::process::Command::new("mkfs.ntfs")
        .args(["-Q", "-F", partition_path])
        .status()
        .map_err(ArgosError::Io)?;
    if !status.success() {
        return Err(ArgosError::Io(std::io::Error::other(format!(
            "mkfs.ntfs -Q -F {partition_path} exited with {status}"
        ))));
    }
    Ok(())
}

/// Copies every file `image::windows::WindowsIso` lists into `mountpoint`,
/// hashing each one as it's copied.
fn copy_files(
    iso: &WindowsIso,
    files: &[image::windows::IsoFileEntry],
    mountpoint: &Path,
    progress: &dyn ProgressSink,
) -> Result<CopiedFiles> {
    progress.on_phase(Phase::CopyingFiles);

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut bytes_done = 0u64;
    let mut hashes = Vec::with_capacity(files.len());

    for entry in files {
        let source = iso
            .open_file(&entry.path)
            .map_err(ArgosError::Io)?
            .ok_or_else(|| {
                ArgosError::Io(std::io::Error::other(format!(
                    "{} listed but could not be reopened",
                    entry.path
                )))
            })?;

        let dest_path = mountpoint.join(&entry.path);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).map_err(ArgosError::Io)?;
        }
        let mut dest = File::create(&dest_path).map_err(ArgosError::Io)?;

        // One pass: every byte read from the ISO is both hashed and written
        // to `dest`, rather than reading each file twice.
        let hash = argos_core::image::checksum::copy_and_hash(source, &mut dest, |chunk_done| {
            progress.on_progress(bytes_done + chunk_done, total_bytes);
        })
        .map_err(ArgosError::Io)?;
        dest.flush().map_err(ArgosError::Io)?;

        bytes_done += entry.size;
        hashes.push((entry.path.clone(), hash));
    }

    Ok(CopiedFiles {
        files_copied: files.len() as u64,
        bytes_copied: bytes_done,
        file_hashes: hashes,
    })
}
