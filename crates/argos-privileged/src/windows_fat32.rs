//! The pure-Rust FAT32 Windows write path (phase 3 M3, backlog #43): creates
//! a single-partition GPT (via `gptman`), then formats and populates that
//! partition as FAT32 through `fatfs` over a [`crate::partition_io::PartitionWindow`]
//! -- writing directly into the partition's byte range of the open device
//! handle. Unlike the NTFS path (`crate::windows`, W3), nothing here spawns a
//! process, re-reads the partition table, waits for partition device nodes,
//! or mounts a filesystem: the whole write is this process talking to one
//! file descriptor.
//!
//! The media boots because FAT32 is what UEFI firmware reads natively: the
//! ISO's own `efi/boot/bootx64.efi` (Microsoft-signed) is copied like any
//! other file, so no vendored boot image is involved. Files over FAT32's
//! 4GiB-1 limit (a real `install.wim` usually is) are refused with a clear
//! error until M2's WIM splitter slots into the copy pipeline.
//!
//! Linux-only for now, matching `crate::windows` -- but only by the explicit
//! gate at the top of each `execute_*` function, kept until M4 (#34) flips
//! it: everything below the gate is already platform-neutral.

use crate::partition_io::PartitionWindow;
use crate::protocol::{
    validate_refreshed_device_for_windows_write, VerifyWindowsPlan, WriteWindowsPlan,
};
use argos_core::error::{ArgosError, Result};
use argos_core::image::checksum::{copy_and_hash, sha256_stream};
use argos_core::image::windows::{IsoFileEntry, WindowsIso};
use argos_core::partition::windows::{
    WindowsFat32Plan, MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID, SECTOR_SIZE,
};
use argos_core::progress::{Phase, ProgressSink};
use argos_core::verify::{
    verify_windows_fat32_layout, verify_windows_file_hash, ObservedPartition,
};
use argos_core::{image, preflight};
use argos_platform::PlatformOps;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

/// FAT32's hard per-file ceiling: sizes are 32-bit, so 4GiB-1.
pub const FAT32_MAX_FILE_BYTES: u64 = u32::MAX as u64;

/// What [`execute_write_windows_fat32`] returns on success -- the FAT32
/// counterpart of [`crate::windows::WindowsWriteOutcome`], minus the boot
/// partition hash (this layout has no boot partition).
#[derive(Debug)]
pub struct Fat32WriteOutcome {
    pub files_copied: u64,
    pub bytes_copied: u64,
    /// `(path relative to the ISO root, SHA-256)` for every file copied,
    /// captured during the copy rather than by re-reading afterwards.
    pub file_hashes: Vec<(String, String)>,
}

pub fn execute_write_windows_fat32(
    plan: &WriteWindowsPlan,
    progress: &dyn ProgressSink,
) -> Result<Fat32WriteOutcome> {
    // The gate M4 (#34) lifts; see this module's doc comment.
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

    // Same never-trust-the-plan posture as the NTFS path: re-classify and
    // re-list the ISO here.
    if !image::windows::classify(&plan.iso_path)?.is_windows_installer_iso() {
        return Err(ArgosError::NotWindowsInstallerIso(plan.iso_path.clone()));
    }
    let iso = WindowsIso::open(&plan.iso_path)?;
    let files = iso.list_files()?;
    ensure_files_fit_fat32(&files)?;

    let layout = fat32_layout_for(&files);
    preflight::check_windows_fat32_capacity(
        &plan.device_path,
        plan.expected_size_bytes,
        &plan.iso_path,
        &layout,
    )?;

    // Safe-open precondition (backlog #20), same as every other write path.
    progress.on_phase(Phase::Unmounting);
    platform.unmount(&device)?;

    let mut device_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&plan.device_path)
        .map_err(ArgosError::Io)?;
    let outcome = write_fat32_media(&mut device_file, &layout, &iso, &files, progress)?;
    device_file.sync_all().map_err(ArgosError::Io)?;
    Ok(outcome)
}

/// The FAT32 layout verification counterpart (M3.4): re-derives the expected
/// [`WindowsFat32Plan`] from the source ISO, confirms the real GPT matches
/// it, then reads the FAT32 filesystem back (read-only, still no mount) and
/// confirms every file's hash matches a fresh read of the source ISO.
pub fn execute_verify_windows_fat32(
    plan: &VerifyWindowsPlan,
    progress: &dyn ProgressSink,
) -> Result<crate::windows::WindowsVerifyOutcome> {
    if !cfg!(target_os = "linux") {
        return Err(ArgosError::NotImplemented(
            "Windows installer verify support (non-Linux)",
        ));
    }

    let platform = crate::platform_select::current_platform();
    platform
        .refresh(&plan.device_path, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(plan.device_path.clone()))?;

    if !image::windows::classify(&plan.iso_path)?.is_windows_installer_iso() {
        return Err(ArgosError::NotWindowsInstallerIso(plan.iso_path.clone()));
    }
    let iso = WindowsIso::open(&plan.iso_path)?;
    let files = iso.list_files()?;
    let layout = fat32_layout_for(&files);

    let mut device_file = File::open(&plan.device_path).map_err(ArgosError::Io)?;
    verify_fat32_media(&mut device_file, &layout, &iso, &files, progress)?;

    Ok(crate::windows::WindowsVerifyOutcome {
        files_verified: files.len() as u64,
    })
}

/// Refuses any file FAT32 cannot hold. Today that means a real Windows
/// ISO's `install.wim` is refused here -- deliberately, with an error that
/// names the file and the way out -- until M2's splitter turns that file
/// into `.swm` parts before it reaches the FAT32 copy.
fn ensure_files_fit_fat32(files: &[IsoFileEntry]) -> Result<()> {
    for entry in files {
        if entry.size > FAT32_MAX_FILE_BYTES {
            return Err(ArgosError::WindowsFileTooLargeForFat32 {
                path: entry.path.clone(),
                size_bytes: entry.size,
            });
        }
    }
    Ok(())
}

fn fat32_layout_for(files: &[IsoFileEntry]) -> WindowsFat32Plan {
    WindowsFat32Plan::new(files.iter().map(|f| f.size).sum())
}

/// Partitions, formats, and populates `device` per `layout` -- everything
/// [`execute_write_windows_fat32`] does after its device/ISO validation.
/// Generic over the handle so unit tests can run it against a plain temp
/// file; no step in here knows or cares whether `device` is real hardware.
fn write_fat32_media<H: Read + Write + Seek>(
    device: &mut H,
    layout: &WindowsFat32Plan,
    iso: &WindowsIso,
    files: &[IsoFileEntry],
    progress: &dyn ProgressSink,
) -> Result<Fat32WriteOutcome> {
    progress.on_phase(Phase::Partitioning);
    write_fat32_partition_table(device, layout)?;

    progress.on_phase(Phase::FormattingFat32);
    let mut window = PartitionWindow::new(&mut *device, layout.windows_partition);
    fatfs::format_volume(
        &mut window,
        fatfs::FormatVolumeOptions::new()
            // Forced rather than size-derived: FAT16 media (what a small
            // volume would default to) is far less universally bootable,
            // and WindowsFat32Plan's size floor guarantees FAT32 is valid.
            .fat_type(fatfs::FatType::Fat32)
            .volume_label(*b"ARGOS-WIN  "),
    )
    .map_err(ArgosError::Io)?;

    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).map_err(ArgosError::Io)?;
    let copy_result = copy_files_fat32(&fs, iso, files, progress);
    // Unmount regardless of how the copy went -- it's what flushes the FAT
    // and FSInfo sectors -- but a copy error outranks an unmount error.
    let unmount_result = fs.unmount().map_err(ArgosError::Io);
    let copied = copy_result?;
    unmount_result?;
    Ok(copied)
}

/// The read-back half: confirms the GPT matches `layout`, then per-file
/// hashes through a read-only `fatfs` against a fresh read of the ISO.
/// Same handle-generic posture as [`write_fat32_media`].
fn verify_fat32_media<H: Read + Write + Seek>(
    device: &mut H,
    layout: &WindowsFat32Plan,
    iso: &WindowsIso,
    files: &[IsoFileEntry],
    progress: &dyn ProgressSink,
) -> Result<()> {
    progress.on_phase(Phase::Verifying);
    let observed = read_observed_fat32_partition(device)?;
    verify_windows_fat32_layout(layout, observed)?;

    // Hash every source file first, then compare each against the device --
    // same two-pass shape (and the same phase for each pass) as the NTFS
    // path's verify_windows_files.
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
    let mut window = PartitionWindow::new(&mut *device, layout.windows_partition);
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).map_err(ArgosError::Io)?;
    let root = fs.root_dir();

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut bytes_done = 0u64;
    for (entry, expected_hash) in files.iter().zip(expected_hashes.iter()) {
        let dest = root.open_file(&entry.path).map_err(|e| {
            ArgosError::Io(std::io::Error::other(format!(
                "{} missing from the FAT32 partition: {e}",
                entry.path
            )))
        })?;
        let actual_hash = sha256_stream(dest, |chunk_done| {
            progress.on_progress(bytes_done + chunk_done, total_bytes);
        })
        .map_err(ArgosError::Io)?;
        verify_windows_file_hash(&entry.path, expected_hash, &actual_hash)?;
        bytes_done += entry.size;
    }
    Ok(())
}

/// Builds and writes the single-partition GPT: a protective MBR plus one
/// Microsoft Basic Data partition (see `WindowsFat32Plan`'s type-GUID
/// decision note), sized and placed exactly as `layout` computed.
fn write_fat32_partition_table<H: Read + Write + Seek>(
    device: &mut H,
    layout: &WindowsFat32Plan,
) -> Result<()> {
    let mut gpt = gptman::GPT::new_from(device, SECTOR_SIZE, crate::windows::random_guid()?)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;

    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID,
        unique_partition_guid: crate::windows::random_guid()?,
        starting_lba: layout.windows_partition.start_offset_bytes / SECTOR_SIZE,
        ending_lba: layout.windows_partition.end_offset_bytes() / SECTOR_SIZE - 1,
        attribute_bits: 0,
        partition_name: "ARGOS-WIN".into(),
    };

    gptman::GPT::write_protective_mbr_into(device, SECTOR_SIZE)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    gpt.write_into(device)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    device.flush().map_err(ArgosError::Io)?;
    Ok(())
}

/// Reads the real GPT off `device` and extracts partition 1. Uses
/// `GPT::find_from` (auto-detecting 512- vs 4096-byte sectors), the same
/// cheap defensiveness as the NTFS path's `read_observed_partitions`.
fn read_observed_fat32_partition<H: Read + Seek>(device: &mut H) -> Result<ObservedPartition> {
    let gpt = gptman::GPT::find_from(device)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    let entry = &gpt[1];
    if !entry.is_used() {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(
            "partition 1 is missing from the GPT".into(),
        ));
    }
    let sector_count = entry.ending_lba - entry.starting_lba + 1;
    Ok(ObservedPartition {
        partition_type_guid: entry.partition_type_guid,
        region: argos_core::partition::windows::PartitionRegion {
            start_offset_bytes: entry.starting_lba * gpt.sector_size,
            size_bytes: sector_count * gpt.sector_size,
        },
    })
}

/// Copies every listed file into the FAT32 filesystem, creating parent
/// directories as needed and hashing each file as it's copied.
fn copy_files_fat32<H: Read + Write + Seek>(
    fs: &fatfs::FileSystem<H>,
    iso: &WindowsIso,
    files: &[IsoFileEntry],
    progress: &dyn ProgressSink,
) -> Result<Fat32WriteOutcome> {
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

        // `fatfs`'s create_file/create_dir only auto-handle the *final*
        // path component, so walk the parents one component at a time
        // (create_dir opens an already-existing directory rather than
        // failing, which is exactly what repeated prefixes need).
        let mut dir = fs.root_dir();
        let mut components: Vec<&str> = entry.path.split('/').collect();
        let file_name = components
            .pop()
            .expect("IsoFileEntry paths are never empty");
        for component in components {
            dir = dir.create_dir(component).map_err(ArgosError::Io)?;
        }
        let mut dest = dir.create_file(file_name).map_err(ArgosError::Io)?;

        // One pass: every byte read from the ISO is both hashed and
        // written, same as the NTFS path's copy_files.
        let hash = copy_and_hash(source, &mut dest, |chunk_done| {
            progress.on_progress(bytes_done + chunk_done, total_bytes);
        })
        .map_err(ArgosError::Io)?;
        dest.flush().map_err(ArgosError::Io)?;

        bytes_done += entry.size;
        hashes.push((entry.path.clone(), hash));
    }

    Ok(Fat32WriteOutcome {
        files_copied: files.len() as u64,
        bytes_copied: bytes_done,
        file_hashes: hashes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::image::windows::fixtures;
    use argos_core::progress::NoopProgress;
    use std::io::Cursor;

    /// Opens a synthetic UDF Windows-installer ISO (the same fixture builder
    /// the NTFS path's tests use) and lists its files.
    fn synthetic_iso() -> (WindowsIso, Vec<IsoFileEntry>, tempfile::NamedTempFile) {
        let iso_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            iso_file.path(),
            fixtures::udf_windows_installer_iso(true, true),
        )
        .unwrap();
        let iso = WindowsIso::open(iso_file.path()).unwrap();
        let files = iso.list_files().unwrap();
        assert!(!files.is_empty());
        (iso, files, iso_file)
    }

    /// A sparse temp file big enough for the layout -- the "device".
    fn device_file_for(layout: &WindowsFat32Plan) -> std::fs::File {
        let file = tempfile::tempfile().unwrap();
        file.set_len(layout.total_bytes_required()).unwrap();
        file
    }

    #[test]
    fn write_then_verify_round_trips_on_a_plain_file() {
        let (iso, files, _guard) = synthetic_iso();
        let layout = fat32_layout_for(&files);
        let mut device = device_file_for(&layout);

        let outcome = write_fat32_media(&mut device, &layout, &iso, &files, &NoopProgress).unwrap();
        assert_eq!(outcome.files_copied, files.len() as u64);
        assert_eq!(
            outcome.bytes_copied,
            files.iter().map(|f| f.size).sum::<u64>()
        );

        verify_fat32_media(&mut device, &layout, &iso, &files, &NoopProgress).unwrap();
    }

    #[test]
    fn the_written_gpt_has_exactly_one_basic_data_partition_where_planned() {
        let (iso, files, _guard) = synthetic_iso();
        let layout = fat32_layout_for(&files);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &files, &NoopProgress).unwrap();

        let observed = read_observed_fat32_partition(&mut device).unwrap();
        assert_eq!(
            observed.partition_type_guid,
            MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID
        );
        assert_eq!(
            observed.region.start_offset_bytes,
            layout.windows_partition.start_offset_bytes
        );
        assert_eq!(
            observed.region.size_bytes,
            layout.windows_partition.size_bytes
        );

        let gpt = {
            device.seek(SeekFrom::Start(0)).unwrap();
            gptman::GPT::find_from(&mut device).unwrap()
        };
        assert!(!gpt[2].is_used(), "the FAT32 layout is single-partition");
    }

    #[test]
    fn the_filesystem_is_actually_fat32_not_a_smaller_fat() {
        let (iso, files, _guard) = synthetic_iso();
        let layout = fat32_layout_for(&files);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &files, &NoopProgress).unwrap();

        let window = PartitionWindow::new(&mut device, layout.windows_partition);
        let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
    }

    #[test]
    fn verify_fails_when_a_written_file_is_corrupted() {
        let (iso, files, _guard) = synthetic_iso();
        let layout = fat32_layout_for(&files);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &files, &NoopProgress).unwrap();

        // Corrupt the first byte of the largest file's content by finding it
        // through the filesystem itself (window + fatfs, read-write).
        let largest = files.iter().max_by_key(|f| f.size).unwrap();
        {
            let window = PartitionWindow::new(&mut device, layout.windows_partition);
            let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();
            {
                let mut file = fs.root_dir().open_file(&largest.path).unwrap();
                let mut first = [0u8; 1];
                file.read_exact(&mut first).unwrap();
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(&[first[0] ^ 0xFF]).unwrap();
                file.flush().unwrap();
            }
            fs.unmount().unwrap();
        }

        let err =
            verify_fat32_media(&mut device, &layout, &iso, &files, &NoopProgress).unwrap_err();
        assert!(
            matches!(err, ArgosError::WindowsFileMismatch { ref path, .. } if *path == largest.path)
        );
    }

    #[test]
    fn verify_fails_when_the_device_has_no_gpt_at_all() {
        let (iso, files, _guard) = synthetic_iso();
        let layout = fat32_layout_for(&files);
        let mut blank = Cursor::new(vec![0u8; 4 * 1024 * 1024]);
        assert!(verify_fat32_media(&mut blank, &layout, &iso, &files, &NoopProgress).is_err());
    }

    #[test]
    fn a_file_over_4gib_is_refused_with_the_dedicated_error() {
        let files = vec![IsoFileEntry {
            path: "sources/install.wim".into(),
            size: FAT32_MAX_FILE_BYTES + 1,
        }];
        let err = ensure_files_fit_fat32(&files).unwrap_err();
        assert!(
            matches!(err, ArgosError::WindowsFileTooLargeForFat32 { ref path, size_bytes } if path == "sources/install.wim" && size_bytes == FAT32_MAX_FILE_BYTES + 1)
        );
    }

    #[test]
    fn files_at_exactly_the_fat32_limit_are_allowed() {
        let files = vec![IsoFileEntry {
            path: "sources/install.swm".into(),
            size: FAT32_MAX_FILE_BYTES,
        }];
        assert!(ensure_files_fit_fat32(&files).is_ok());
    }
}
