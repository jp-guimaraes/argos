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
//! other file, so no vendored boot image is involved.
//!
//! Files over FAT32's 4GiB-1 limit -- a real `install.wim` always is -- are
//! split on the fly into `.swm` parts by `argos_core::image::wim` (phase 3
//! M2.3, backlog #42): the ISO's UDF stream feeds the splitter, which feeds
//! `fatfs` directly, hashing each part in the same pass, so a 5GB
//! `install.wim` never lands anywhere whole. Anything oversized that *isn't*
//! a splittable WIM (or is a solid `.esd`) is still refused with a clear
//! error rather than truncated.
//!
//! **Runs on Linux and macOS** (phase 3 M4, backlog #34). Nothing in this
//! path is platform-specific: with no `mkfs`, no mount and no partition
//! device nodes, the only OS interaction left is the pre-write unmount,
//! which `PlatformOps` already provides on both. That is precisely why the
//! phase-3 plan replaced the NTFS layout -- the macFUSE/`ntfs-3g`
//! dependency #34 was originally scoped around simply has no counterpart
//! here. `--layout ntfs` keeps its Linux-only gate.

use crate::partition_io::{PartitionWindow, SizedDevice};
use crate::protocol::{
    validate_refreshed_device_for_windows_write, VerifyWindowsPlan, WriteWindowsPlan,
};
use argos_core::error::{ArgosError, Result};
use argos_core::image::checksum::{copy_and_hash, sha256_stream};
use argos_core::image::wim;
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
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
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
    let actions = plan_copy_actions(&iso, &files)?;

    let layout = fat32_layout_for(&actions);
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
    // SizedDevice, not the bare file: macOS device nodes can't answer
    // SEEK_END, which gptman needs to lay out a new GPT. See its doc
    // comment -- without it this panics before writing a byte, on any real
    // macOS disk.
    let outcome = {
        let mut sized = SizedDevice::new(&mut device_file, device.size_bytes);
        write_fat32_media(&mut sized, &layout, &iso, &actions, progress)?
    };
    // Not sync_all(): macOS device nodes reject F_FULLFSYNC. See
    // partition_io::sync_device.
    crate::partition_io::sync_device(&device_file).map_err(ArgosError::Io)?;
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
    let platform = crate::platform_select::current_platform();
    let device = platform
        .refresh(&plan.device_path, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(plan.device_path.clone()))?;

    if !image::windows::classify(&plan.iso_path)?.is_windows_installer_iso() {
        return Err(ArgosError::NotWindowsInstallerIso(plan.iso_path.clone()));
    }
    let iso = WindowsIso::open(&plan.iso_path)?;
    let files = iso.list_files()?;
    let actions = plan_copy_actions(&iso, &files)?;
    let layout = fat32_layout_for(&actions);

    // Opened read-write (not File::open) only so the same SizedDevice
    // wrapper the write path uses applies here too -- verify never writes,
    // and PartitionWindow's Write impl is simply never exercised.
    let mut device_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&plan.device_path)
        .map_err(ArgosError::Io)?;
    let mut sized = SizedDevice::new(&mut device_file, device.size_bytes);
    let files_verified = verify_fat32_media(&mut sized, &layout, &iso, &actions, progress)?;

    Ok(crate::windows::WindowsVerifyOutcome { files_verified })
}

/// Target size for one `.swm` part: comfortably under FAT32's 4GiB-1
/// ceiling, matching what Rufus and `wimlib-imagex split` use in practice.
/// Not the hard limit itself -- leaving headroom means a part that runs
/// slightly over its budget (one oversized resource) still fits FAT32.
pub const SWM_PART_TARGET_BYTES: u64 = 3_800 * 1024 * 1024;

/// What to do with one file from the ISO when populating the FAT32
/// partition (phase 3 M2.3, backlog #42).
///
/// Public because `argos-cli` builds the very same plan before elevating,
/// to size the partition and show the layout in its confirmation prompt.
/// It is deliberately the *same* function, not a parallel copy: an earlier
/// version had the CLI reimplement "does every file fit FAT32?", which
/// silently went stale the moment the splitter landed and made `argos
/// write --layout fat32` refuse real Windows media the helper could
/// handle perfectly well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyAction {
    /// Copy the ISO file through verbatim, under its own name.
    Direct { path: String, size: u64 },
    /// Split this WIM into `.swm` parts (it doesn't fit FAT32 whole).
    /// `part_paths` are the destination names, part 1 first.
    SplitWim {
        source_path: String,
        part_paths: Vec<String>,
        part_sizes: Vec<u64>,
    },
}

impl CopyAction {
    /// Bytes this action will occupy on the FAT32 filesystem.
    pub fn bytes_on_target(&self) -> u64 {
        match self {
            CopyAction::Direct { size, .. } => *size,
            CopyAction::SplitWim { part_sizes, .. } => part_sizes.iter().sum(),
        }
    }
}

/// `sources/install.wim` -> `sources/install.swm`, `sources/install2.swm`,
/// ... -- the naming Windows Setup looks for, and what `wimsplit` produces.
/// Case is taken from the source path so media that uses uppercase names
/// keeps them.
pub fn swm_part_path(source_path: &str, part_number: u16) -> String {
    let stem = source_path
        .strip_suffix(".wim")
        .or_else(|| source_path.strip_suffix(".WIM"))
        .unwrap_or(source_path);
    let ext = if source_path.ends_with(".WIM") {
        "SWM"
    } else {
        "swm"
    };
    if part_number == 1 {
        format!("{stem}.{ext}")
    } else {
        format!("{stem}{part_number}.{ext}")
    }
}

/// Decides, for every file in the ISO, whether it can be copied whole or
/// must be split -- and refuses cleanly if it can be neither.
///
/// A file over FAT32's limit is only splittable if it's a WIM this module's
/// splitter accepts (`image::wim`): anything else (or a solid `.esd`, whose
/// LZMS blocks can't be split at resource boundaries) still fails with
/// [`ArgosError::WindowsFileTooLargeForFat32`], now carrying the splitter's
/// own explanation.
pub fn plan_copy_actions(iso: &WindowsIso, files: &[IsoFileEntry]) -> Result<Vec<CopyAction>> {
    let mut actions = Vec::with_capacity(files.len());
    for entry in files {
        if entry.size <= FAT32_MAX_FILE_BYTES {
            actions.push(CopyAction::Direct {
                path: entry.path.clone(),
                size: entry.size,
            });
            continue;
        }

        let too_large = || ArgosError::WindowsFileTooLargeForFat32 {
            path: entry.path.clone(),
            size_bytes: entry.size,
        };

        let mut reader = iso
            .open_file_seekable(&entry.path)
            .map_err(ArgosError::Io)?
            .ok_or_else(too_large)?;
        let image = wim::WimImage::open(&mut reader).map_err(|err| {
            // The splitter's message (e.g. the solid-.esd refusal) is the
            // useful part; keep it rather than a generic "too large".
            ArgosError::Io(std::io::Error::other(format!(
                "{} is {} bytes, over FAT32's limit, and cannot be split: {err}",
                entry.path, entry.size
            )))
        })?;

        let part_sizes = wim::plan_part_sizes(&image, SWM_PART_TARGET_BYTES);
        if part_sizes.iter().any(|&size| size > FAT32_MAX_FILE_BYTES) {
            // One resource alone exceeds FAT32's per-file limit -- nothing
            // any split can do about it (resources are never divided).
            return Err(too_large());
        }
        let part_paths = (1..=part_sizes.len() as u16)
            .map(|n| swm_part_path(&entry.path, n))
            .collect();
        actions.push(CopyAction::SplitWim {
            source_path: entry.path.clone(),
            part_paths,
            part_sizes,
        });
    }
    Ok(actions)
}

pub fn fat32_layout_for(actions: &[CopyAction]) -> WindowsFat32Plan {
    WindowsFat32Plan::new(actions.iter().map(CopyAction::bytes_on_target).sum())
}

/// Partitions, formats, and populates `device` per `layout` -- everything
/// [`execute_write_windows_fat32`] does after its device/ISO validation.
/// Generic over the handle so unit tests can run it against a plain temp
/// file; no step in here knows or cares whether `device` is real hardware.
fn write_fat32_media<H: Read + Write + Seek>(
    device: &mut H,
    layout: &WindowsFat32Plan,
    iso: &WindowsIso,
    actions: &[CopyAction],
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
    let copy_result = copy_files_fat32(&fs, iso, actions, progress);
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
    actions: &[CopyAction],
    progress: &dyn ProgressSink,
) -> Result<u64> {
    progress.on_phase(Phase::Verifying);
    let observed = read_observed_fat32_partition(device)?;
    verify_windows_fat32_layout(layout, observed)?;

    // Hash what *should* be on the device first, then compare each file
    // against what actually is -- same two-pass shape (and the same phase
    // per pass) as the NTFS path's verify_windows_files. For a split WIM
    // that means re-running the splitter over the ISO's WIM into a hashing
    // sink: the splitter is deterministic, so the parts it produces now
    // must hash identically to the ones the write produced.
    progress.on_phase(Phase::Checksumming);
    let mut expected: Vec<(String, String)> = Vec::new();
    for action in actions {
        match action {
            CopyAction::Direct { path, .. } => {
                let source = open_iso_file(iso, path)?;
                expected.push((
                    path.clone(),
                    sha256_stream(source, |_| {}).map_err(ArgosError::Io)?,
                ));
            }
            CopyAction::SplitWim {
                source_path,
                part_paths,
                ..
            } => {
                for (path, hash) in split_wim_part_hashes(iso, source_path, part_paths, |_| {})? {
                    expected.push((path, hash));
                }
            }
        }
    }

    progress.on_phase(Phase::Verifying);
    let mut window = PartitionWindow::new(&mut *device, layout.windows_partition);
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).map_err(ArgosError::Io)?;
    let root = fs.root_dir();

    let total_bytes: u64 = actions.iter().map(CopyAction::bytes_on_target).sum();
    let mut bytes_done = 0u64;
    for (path, expected_hash) in &expected {
        let dest = root.open_file(path).map_err(|e| {
            ArgosError::Io(std::io::Error::other(format!(
                "{path} missing from the FAT32 partition: {e}"
            )))
        })?;
        let mut this_file = 0u64;
        let actual_hash = sha256_stream(dest, |chunk_done| {
            this_file = chunk_done;
            progress.on_progress(bytes_done + chunk_done, total_bytes);
        })
        .map_err(ArgosError::Io)?;
        verify_windows_file_hash(path, expected_hash, &actual_hash)?;
        bytes_done += this_file;
    }
    Ok(expected.len() as u64)
}

/// Opens one ISO file or fails with the same message every caller here
/// wants ("listed but could not be reopened").
fn open_iso_file<'a>(iso: &'a WindowsIso, path: &str) -> Result<Box<dyn Read + 'a>> {
    iso.open_file(path).map_err(ArgosError::Io)?.ok_or_else(|| {
        ArgosError::Io(std::io::Error::other(format!(
            "{path} listed but could not be reopened"
        )))
    })
}

/// Runs the WIM splitter over `source_path` inside the ISO, hashing each
/// `.swm` part instead of writing it anywhere. Used by the verify path to
/// recompute what the write should have produced (the splitter is
/// deterministic), so no split state has to survive between the two runs.
fn split_wim_part_hashes(
    iso: &WindowsIso,
    source_path: &str,
    part_paths: &[String],
    mut on_bytes: impl FnMut(u64),
) -> Result<Vec<(String, String)>> {
    let mut reader = iso
        .open_file_seekable(source_path)
        .map_err(ArgosError::Io)?
        .ok_or_else(|| {
            ArgosError::Io(std::io::Error::other(format!(
                "{source_path} listed but could not be reopened for splitting"
            )))
        })?;
    let image = wim::WimImage::open(&mut reader).map_err(ArgosError::Io)?;

    let hashers = std::cell::RefCell::new(Vec::<Sha256>::new());
    wim::split(
        &mut reader,
        &image,
        SWM_PART_TARGET_BYTES,
        |_| {
            hashers.borrow_mut().push(Sha256::new());
            Ok(HashingSink(&hashers))
        },
        &mut on_bytes,
    )
    .map_err(ArgosError::Io)?;

    let digests: Vec<String> = hashers
        .into_inner()
        .into_iter()
        .map(|h| format!("{:x}", h.finalize()))
        .collect();
    if digests.len() != part_paths.len() {
        return Err(ArgosError::Io(std::io::Error::other(format!(
            "splitting {source_path} produced {} parts, expected {}",
            digests.len(),
            part_paths.len()
        ))));
    }
    Ok(part_paths.iter().cloned().zip(digests).collect())
}

/// A `Write` sink that only hashes, feeding whichever hasher was most
/// recently pushed -- `wim::split` writes each part to completion before
/// asking for the next, so "the last one" is always the current part.
struct HashingSink<'a>(&'a std::cell::RefCell<Vec<Sha256>>);

impl Write for HashingSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .borrow_mut()
            .last_mut()
            .expect("a hasher is pushed before the sink is handed out")
            .update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
    actions: &[CopyAction],
    progress: &dyn ProgressSink,
) -> Result<Fat32WriteOutcome> {
    progress.on_phase(Phase::CopyingFiles);

    let total_bytes: u64 = actions.iter().map(CopyAction::bytes_on_target).sum();
    let mut bytes_done = 0u64;
    let mut hashes = Vec::new();
    let mut files_copied = 0u64;

    for action in actions {
        match action {
            CopyAction::Direct { path, size } => {
                let source = open_iso_file(iso, path)?;
                let mut dest = create_file_at(fs, path)?;
                // One pass: every byte read from the ISO is both hashed and
                // written, same as the NTFS path's copy_files.
                let hash = copy_and_hash(source, &mut dest, |chunk_done| {
                    progress.on_progress(bytes_done + chunk_done, total_bytes);
                })
                .map_err(ArgosError::Io)?;
                dest.flush().map_err(ArgosError::Io)?;
                bytes_done += size;
                files_copied += 1;
                hashes.push((path.clone(), hash));
            }
            CopyAction::SplitWim {
                source_path,
                part_paths,
                part_sizes,
            } => {
                let written = split_wim_onto_fat32(
                    fs,
                    iso,
                    source_path,
                    part_paths,
                    part_sizes,
                    bytes_done,
                    total_bytes,
                    progress,
                )?;
                bytes_done += part_sizes.iter().sum::<u64>();
                files_copied += part_paths.len() as u64;
                hashes.extend(written);
            }
        }
    }

    Ok(Fat32WriteOutcome {
        files_copied,
        bytes_copied: bytes_done,
        file_hashes: hashes,
    })
}

/// Creates a file at a `/`-separated path inside the FAT32 filesystem,
/// creating parent directories as needed.
///
/// `fatfs`'s create_file/create_dir only auto-handle the *final* path
/// component, so parents are walked one component at a time (create_dir
/// opens an already-existing directory rather than failing, which is
/// exactly what repeated prefixes need).
fn create_file_at<'a, H: Read + Write + Seek>(
    fs: &'a fatfs::FileSystem<H>,
    path: &str,
) -> Result<fatfs::File<'a, H>> {
    let mut dir = fs.root_dir();
    let mut components: Vec<&str> = path.split('/').collect();
    let file_name = components.pop().expect("ISO paths are never empty");
    for component in components {
        dir = dir.create_dir(component).map_err(ArgosError::Io)?;
    }
    dir.create_file(file_name).map_err(ArgosError::Io)
}

/// Streams one oversized WIM out of the ISO and straight into `.swm` parts
/// on the FAT32 filesystem (phase 3 M2.3): UDF stream -> splitter -> FAT32
/// writer, hashing each part in the same pass, never materializing a part
/// in memory.
#[allow(clippy::too_many_arguments)]
fn split_wim_onto_fat32<H: Read + Write + Seek>(
    fs: &fatfs::FileSystem<H>,
    iso: &WindowsIso,
    source_path: &str,
    part_paths: &[String],
    part_sizes: &[u64],
    bytes_done_before: u64,
    total_bytes: u64,
    progress: &dyn ProgressSink,
) -> Result<Vec<(String, String)>> {
    let mut reader = iso
        .open_file_seekable(source_path)
        .map_err(ArgosError::Io)?
        .ok_or_else(|| {
            ArgosError::Io(std::io::Error::other(format!(
                "{source_path} listed but could not be reopened for splitting"
            )))
        })?;
    let image = wim::WimImage::open(&mut reader).map_err(ArgosError::Io)?;

    // Each part is written to completion before the next is requested, so
    // one "current part" slot is all the bookkeeping this needs.
    let mut hashes: Vec<(String, String)> = Vec::with_capacity(part_paths.len());
    let mut part_index = 0usize;

    {
        // A sink that writes into the current .swm file while hashing it.
        struct PartSink<'a, H: Read + Write + Seek> {
            file: fatfs::File<'a, H>,
            hasher: Sha256,
        }
        impl<H: Read + Write + Seek> Write for PartSink<'_, H> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let n = self.file.write(buf)?;
                self.hasher.update(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.file.flush()
            }
        }

        // `wim::split` hands each sink out by value and drops it before
        // asking for the next, so the finished hash has to be collected on
        // drop -- shared through a RefCell the loop below drains.
        let finished = std::cell::RefCell::new(Vec::<String>::new());

        struct Finishing<'a, H: Read + Write + Seek> {
            sink: Option<PartSink<'a, H>>,
            finished: &'a std::cell::RefCell<Vec<String>>,
        }
        impl<H: Read + Write + Seek> Write for Finishing<'_, H> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.sink
                    .as_mut()
                    .expect("sink is only taken on drop")
                    .write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.sink
                    .as_mut()
                    .expect("sink is only taken on drop")
                    .flush()
            }
        }
        impl<H: Read + Write + Seek> Drop for Finishing<'_, H> {
            fn drop(&mut self) {
                if let Some(mut sink) = self.sink.take() {
                    let _ = sink.file.flush();
                    self.finished
                        .borrow_mut()
                        .push(format!("{:x}", sink.hasher.finalize()));
                }
            }
        }

        let mut resource_bytes_seen = 0u64;
        wim::split(
            &mut reader,
            &image,
            SWM_PART_TARGET_BYTES,
            |part_number| {
                let path = part_paths
                    .get(part_number as usize - 1)
                    .ok_or_else(|| {
                        std::io::Error::other(format!(
                            "splitting {source_path} produced more parts than planned"
                        ))
                    })?
                    .clone();
                let file =
                    create_file_at(fs, &path).map_err(|e| std::io::Error::other(e.to_string()))?;
                Ok(Finishing {
                    sink: Some(PartSink {
                        file,
                        hasher: Sha256::new(),
                    }),
                    finished: &finished,
                })
            },
            |copied| {
                // Resource bytes are the bulk of a part; reporting them
                // against the on-target total is close enough for a
                // progress bar and stays monotonic.
                resource_bytes_seen = copied;
                progress.on_progress(bytes_done_before + copied, total_bytes);
            },
        )
        .map_err(ArgosError::Io)?;

        for digest in finished.into_inner() {
            let path = part_paths
                .get(part_index)
                .expect("part count was checked against the plan")
                .clone();
            hashes.push((path, digest));
            part_index += 1;
        }
    }

    if hashes.len() != part_paths.len() {
        return Err(ArgosError::Io(std::io::Error::other(format!(
            "splitting {source_path} produced {} parts, expected {}",
            hashes.len(),
            part_paths.len()
        ))));
    }
    debug_assert_eq!(part_sizes.len(), part_paths.len());
    Ok(hashes)
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
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = fat32_layout_for(&actions);
        let mut device = device_file_for(&layout);

        let outcome =
            write_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();
        assert_eq!(outcome.files_copied, files.len() as u64);
        assert_eq!(
            outcome.bytes_copied,
            files.iter().map(|f| f.size).sum::<u64>()
        );

        verify_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();
    }

    #[test]
    fn the_written_gpt_has_exactly_one_basic_data_partition_where_planned() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = fat32_layout_for(&actions);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();

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
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = fat32_layout_for(&actions);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();

        let window = PartitionWindow::new(&mut device, layout.windows_partition);
        let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
    }

    #[test]
    fn verify_fails_when_a_written_file_is_corrupted() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = fat32_layout_for(&actions);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();

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
            verify_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap_err();
        assert!(
            matches!(err, ArgosError::WindowsFileMismatch { ref path, .. } if *path == largest.path)
        );
    }

    #[test]
    fn verify_fails_when_the_device_has_no_gpt_at_all() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = fat32_layout_for(&actions);
        let mut blank = Cursor::new(vec![0u8; 4 * 1024 * 1024]);
        assert!(verify_fat32_media(&mut blank, &layout, &iso, &actions, &NoopProgress).is_err());
    }

    #[test]
    fn files_within_the_fat32_limit_are_planned_as_direct_copies() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        assert_eq!(actions.len(), files.len());
        assert!(actions
            .iter()
            .all(|a| matches!(a, CopyAction::Direct { .. })));
        assert_eq!(
            actions.iter().map(CopyAction::bytes_on_target).sum::<u64>(),
            files.iter().map(|f| f.size).sum::<u64>()
        );
    }

    #[test]
    fn swm_part_paths_follow_windows_setups_naming() {
        assert_eq!(
            swm_part_path("sources/install.wim", 1),
            "sources/install.swm"
        );
        assert_eq!(
            swm_part_path("sources/install.wim", 2),
            "sources/install2.swm"
        );
        assert_eq!(
            swm_part_path("sources/install.wim", 13),
            "sources/install13.swm"
        );
        // Uppercase media keeps its case.
        assert_eq!(
            swm_part_path("SOURCES/INSTALL.WIM", 2),
            "SOURCES/INSTALL2.SWM"
        );
    }

    /// A file too big for FAT32 that *isn't* a splittable WIM has no way
    /// out, and must say so rather than being silently truncated or split.
    #[test]
    fn an_oversized_non_wim_file_is_refused() {
        // The synthetic ISO's files are small, so drive the check directly
        // with a listing that claims an oversized non-WIM file.
        let (iso, _files, _guard) = synthetic_iso();
        let files = vec![IsoFileEntry {
            path: "sources/huge.dat".into(),
            size: FAT32_MAX_FILE_BYTES + 1,
        }];
        let err = plan_copy_actions(&iso, &files).unwrap_err();
        assert!(
            matches!(
                err,
                ArgosError::WindowsFileTooLargeForFat32 { ref path, size_bytes }
                    if path == "sources/huge.dat" && size_bytes == FAT32_MAX_FILE_BYTES + 1
            ),
            "got: {err}"
        );
    }

    #[test]
    fn files_at_exactly_the_fat32_limit_are_copied_whole() {
        let (iso, _files, _guard) = synthetic_iso();
        let files = vec![IsoFileEntry {
            path: "sources/install.swm".into(),
            size: FAT32_MAX_FILE_BYTES,
        }];
        let actions = plan_copy_actions(&iso, &files).unwrap();
        assert!(matches!(actions[0], CopyAction::Direct { .. }));
    }

    /// End-to-end for M2.3, with the 4GiB threshold lowered so a synthetic
    /// WIM can exercise it: an oversized WIM is split into `.swm` parts on
    /// the FAT32 filesystem, and each part is a real WIM part wimlib-style
    /// numbering agrees with. (The real-size path is covered by
    /// `argos-core`'s wimlib oracle harness.)
    #[test]
    fn an_oversized_wim_is_split_into_swm_parts_on_the_filesystem() {
        use argos_core::image::wim::{WimHeader, WimImage, HEADER_SIZE};

        // Build a WIM big enough that SWM_PART_TARGET_BYTES would be silly
        // to test against, then split it directly at a small limit through
        // the same code path the copy uses.
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = fat32_layout_for(&actions);
        let mut device = device_file_for(&layout);
        write_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();

        // Write a synthetic multi-resource WIM into the FAT32 filesystem by
        // splitting it, exactly as split_wim_onto_fat32 does, and read the
        // parts back with our own header parser.
        let window = PartitionWindow::new(&mut device, layout.windows_partition);
        let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();

        let wim_bytes = tiny_multi_resource_wim();
        let mut source = Cursor::new(wim_bytes);
        let image = WimImage::open(&mut source).unwrap();
        let part_sizes = argos_core::image::wim::plan_part_sizes(&image, 900);
        assert!(
            part_sizes.len() > 1,
            "the fixture should need several parts"
        );
        let part_paths: Vec<String> = (1..=part_sizes.len() as u16)
            .map(|n| swm_part_path("sources/install.wim", n))
            .collect();

        argos_core::image::wim::split(
            &mut source,
            &image,
            900,
            |part_number| {
                let path = &part_paths[part_number as usize - 1];
                create_file_at(&fs, path).map_err(|e| std::io::Error::other(e.to_string()))
            },
            |_| {},
        )
        .unwrap();

        // Every part exists on the filesystem and parses as the part it
        // claims to be.
        for (idx, path) in part_paths.iter().enumerate() {
            let mut file = fs.root_dir().open_file(path).unwrap();
            let mut header_bytes = [0u8; HEADER_SIZE];
            file.read_exact(&mut header_bytes).unwrap();
            let header = WimHeader::parse(&header_bytes).unwrap();
            assert_eq!(header.part_number, idx as u16 + 1);
            assert_eq!(header.total_parts, part_paths.len() as u16);
            assert_ne!(
                header.flags & argos_core::image::wim::FLAG_HEADER_SPANNED,
                0
            );
        }
        fs.unmount().unwrap();
    }

    /// A minimal but structurally valid WIM with several file resources --
    /// enough for the splitter to have real boundaries to choose between.
    fn tiny_multi_resource_wim() -> Vec<u8> {
        use argos_core::image::wim::{
            LookupEntry, ResourceHeader, WimHeader, HEADER_SIZE, LOOKUP_ENTRY_SIZE,
            RESHDR_FLAG_METADATA, WIM_VERSION,
        };

        let resources: Vec<(Vec<u8>, bool)> = vec![
            (vec![0xEE; 100], true),
            (vec![0x11; 400], false),
            (vec![0x22; 400], false),
            (vec![0x33; 400], false),
        ];
        let mut body = Vec::new();
        let mut entries = Vec::new();
        let mut cursor = HEADER_SIZE as u64;
        for (i, (content, is_metadata)) in resources.iter().enumerate() {
            entries.push(LookupEntry {
                resource: ResourceHeader {
                    size_in_wim: content.len() as u64,
                    flags: if *is_metadata {
                        RESHDR_FLAG_METADATA
                    } else {
                        0
                    },
                    offset: cursor,
                    original_size: content.len() as u64,
                },
                part_number: 1,
                ref_count: 1,
                sha1: [i as u8 + 1; 20],
            });
            body.extend_from_slice(content);
            cursor += content.len() as u64;
        }
        let table_offset = cursor;
        let table_len = LOOKUP_ENTRY_SIZE * entries.len() as u64;
        let xml: Vec<u8> = vec![0xFF, 0xFE];

        let header = WimHeader {
            version: WIM_VERSION,
            flags: 0,
            compression_size: 32768,
            guid: [0x5A; 16],
            part_number: 1,
            total_parts: 1,
            image_count: 1,
            offset_table: ResourceHeader {
                size_in_wim: table_len,
                flags: 0,
                offset: table_offset,
                original_size: table_len,
            },
            xml_data: ResourceHeader {
                size_in_wim: xml.len() as u64,
                flags: 0,
                offset: table_offset + table_len,
                original_size: xml.len() as u64,
            },
            boot_metadata: entries[0].resource,
            boot_index: 1,
            integrity: ResourceHeader::default(),
        };

        let mut out = Vec::new();
        out.extend_from_slice(&header.serialize());
        out.extend_from_slice(&body);
        for e in &entries {
            out.extend_from_slice(&e.serialize());
        }
        out.extend_from_slice(&xml);
        out
    }
}
