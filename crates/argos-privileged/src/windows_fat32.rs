//! The pure-Rust FAT32 Windows write path (phase 3 M3, backlog #43): creates
//! a single-partition GPT (via `gptman`), then formats and populates that
//! partition as FAT32 through `fatfs` over a [`crate::partition_io::PartitionWindow`]
//! -- writing directly into the partition's byte range of the open device
//! handle. Nothing here spawns a process, re-reads the partition table,
//! waits for partition device nodes, or mounts a filesystem: the whole
//! write is this process talking to one file descriptor.
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
//! which `PlatformOps` already provides on both. This is the *only* Windows
//! write path Argos has: the earlier two-partition UEFI:NTFS scheme
//! (backlog #27) needed `mkfs.ntfs`/`ntfs-3g` and was Linux-only for exactly
//! that reason, and was retired once this layout was validated on real
//! hardware from both hosts, on both firmwares (decision point M4.3; see
//! `docs/architecture.md`).

use crate::partition_io::{PartitionWindow, SizedDevice};
use crate::protocol::{
    validate_refreshed_device_for_windows_write, VerifyWindowsPlan, WindowsLayout, WriteWindowsPlan,
};
use argos_core::error::{ArgosError, Result};
use argos_core::image;
use argos_core::image::checksum::{copy_and_hash, sha256_stream};
use argos_core::image::wim;
use argos_core::image::windows::{IsoFileEntry, WindowsIso};
use argos_core::partition::windows::{
    chs_for_lba, fat32_bytes_per_cluster_for, WindowsFat32Plan, WindowsMbrPlan, CHS_HEADS,
    CHS_SECTORS_PER_TRACK, MBR_BOOTABLE_FLAG, MBR_FAT32_LBA_PARTITION_TYPE,
    MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID, SECTOR_SIZE,
};
use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_core::verify::{
    verify_windows_fat32_layout, verify_windows_file_hash, ObservedPartition,
};
use argos_platform::PlatformOps;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

/// FAT32's hard per-file ceiling: sizes are 32-bit, so 4GiB-1.
pub const FAT32_MAX_FILE_BYTES: u64 = u32::MAX as u64;

/// 16 cryptographically random bytes, read from `/dev/urandom` (present on
/// both Linux and macOS), dependency-free rather than pulling in a
/// `uuid`/`rand` crate just for this. Used for the GPT's disk GUID and each
/// partition's unique GUID.
pub(crate) fn random_guid() -> Result<[u8; 16]> {
    let mut buf = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(ArgosError::Io)?;
    Ok(buf)
}

/// What [`execute_write_windows_fat32`] returns on success.
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

    let layout = TargetLayout::for_layout(plan.layout, total_bytes_on_target(&actions));
    check_capacity(
        &plan.device_path,
        plan.expected_size_bytes,
        &plan.iso_path,
        &layout,
    )?;

    // Safe-open precondition (backlog #20), same as every other write path.
    progress.on_phase(Phase::Unmounting);
    platform.unmount(&device)?;

    // Exclusive: once format_volume makes the partition recognizable, macOS
    // would otherwise auto-mount it mid-copy and the write would die with
    // EBUSY. See partition_io::open_device_exclusive.
    let mut device_file =
        crate::partition_io::open_device_exclusive(&plan.device_path).map_err(ArgosError::Io)?;
    // SizedDevice, not the bare file: macOS device nodes can't answer
    // SEEK_END, which gptman needs to lay out a new GPT. See its doc
    // comment -- without it this panics before writing a byte, on any real
    // macOS disk.
    // Created here rather than plumbed in from the parent process: the copy
    // loop checks this token on every write, but nothing outside this process
    // can set it yet. Wiring a real source (SIGINT in the unprivileged parent,
    // forwarded down the helper's stdin) is backlog #35 -- and when it lands,
    // this line and this function's signature are the only things in the
    // FAT32 path that have to change.
    let cancel = CancelToken::new();

    let outcome = {
        // Buffered under SizedDevice: the filesystem's writes are tiny and
        // its seeks are mostly redundant, and against a USB device node each
        // one is a round trip. See BufferedDevice.
        let buffered =
            crate::partition_io::BufferedDevice::new(&mut device_file).map_err(ArgosError::Io)?;
        let mut sized = SizedDevice::new(buffered, device.size_bytes);
        let outcome = write_fat32_media(&mut sized, &layout, &iso, &actions, progress, &cancel)?;
        // The buffer must reach the medium before the handle is dropped.
        sized.flush().map_err(ArgosError::Io)?;
        outcome
    };
    // Not sync_all(): macOS device nodes reject F_FULLFSYNC. See
    // partition_io::sync_device.
    crate::partition_io::sync_device(&device_file).map_err(ArgosError::Io)?;
    Ok(outcome)
}

/// What [`execute_verify_windows_fat32`] returns on success: just a count,
/// since verification's whole point is confirming every file already
/// matches, so there's nothing left to report per file beyond that count.
#[derive(Debug)]
pub struct WindowsVerifyOutcome {
    pub files_verified: u64,
}

/// The FAT32 layout verification counterpart (M3.4): re-derives the expected
/// [`WindowsFat32Plan`] from the source ISO, confirms the real GPT matches
/// it, then reads the FAT32 filesystem back (read-only, still no mount) and
/// confirms every file's hash matches a fresh read of the source ISO.
pub fn execute_verify_windows_fat32(
    plan: &VerifyWindowsPlan,
    progress: &dyn ProgressSink,
) -> Result<WindowsVerifyOutcome> {
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
    let layout = TargetLayout::for_layout(plan.layout, total_bytes_on_target(&actions));

    // Unmount first, then open exclusively -- the same treatment the write
    // path gets, for two reasons: a freshly written stick has its FAT32
    // partition auto-mounted by macOS (so a plain open fails with EBUSY),
    // and reading the device under a live mount could serve the mounted
    // filesystem's cached view rather than what is actually on the medium,
    // which is precisely what verification must not do.
    progress.on_phase(Phase::Unmounting);
    platform.unmount(&device)?;
    let mut device_file =
        crate::partition_io::open_device_exclusive(&plan.device_path).map_err(ArgosError::Io)?;
    let buffered =
        crate::partition_io::BufferedDevice::new(&mut device_file).map_err(ArgosError::Io)?;
    let mut sized = SizedDevice::new(buffered, device.size_bytes);
    let files_verified = verify_fat32_media(&mut sized, &layout, &iso, &actions, progress)?;

    Ok(WindowsVerifyOutcome { files_verified })
}

/// Which partition scheme and boot records to put on the media. The
/// filesystem, the file copy and the hashing are identical either way --
/// only the table describing the partition, and whether boot records are
/// installed, differ.
#[derive(Debug, Clone, Copy)]
pub enum TargetLayout {
    /// GPT with a single Microsoft Basic Data partition; booted by UEFI
    /// firmware reading `efi/boot/bootx64.efi` off the FAT32 volume.
    Gpt(WindowsFat32Plan),
    /// MBR with a single active FAT32 partition, plus Argos's MBR boot code
    /// and FAT32 VBR, so a legacy BIOS can chain through to `bootmgr`.
    MbrBios(WindowsMbrPlan),
}

impl TargetLayout {
    /// The partition this layout describes, wherever it came from.
    pub fn region(&self) -> argos_core::partition::windows::PartitionRegion {
        match self {
            TargetLayout::Gpt(p) => p.windows_partition,
            TargetLayout::MbrBios(p) => p.windows_partition,
        }
    }

    /// The LBA the partition starts at -- what the BPB's hidden-sectors
    /// field has to record, whichever scheme is in play.
    pub fn start_lba(&self) -> Result<u32> {
        u32::try_from(self.region().start_offset_bytes / SECTOR_SIZE).map_err(|_| {
            ArgosError::Io(std::io::Error::other(
                "the partition starts beyond the 2TiB a 32-bit LBA can address",
            ))
        })
    }

    /// The smallest device this layout fits on.
    pub fn total_bytes_required(&self) -> u64 {
        match self {
            TargetLayout::Gpt(p) => p.total_bytes_required(),
            TargetLayout::MbrBios(p) => p.total_bytes_required(),
        }
    }

    /// Builds the layout a given [`WindowsLayout`] asks for, from the total
    /// size the files will occupy on the target.
    pub fn for_layout(layout: WindowsLayout, files_total_bytes: u64) -> Self {
        match layout {
            WindowsLayout::Fat32Bios => {
                TargetLayout::MbrBios(WindowsMbrPlan::new(files_total_bytes))
            }
            WindowsLayout::Fat32 => TargetLayout::Gpt(WindowsFat32Plan::new(files_total_bytes)),
        }
    }
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

/// Bytes the planned copy will occupy on the target filesystem.
pub fn total_bytes_on_target(actions: &[CopyAction]) -> u64 {
    actions.iter().map(CopyAction::bytes_on_target).sum()
}

pub fn fat32_layout_for(actions: &[CopyAction]) -> WindowsFat32Plan {
    WindowsFat32Plan::new(total_bytes_on_target(actions))
}

/// Capacity preflight for whichever scheme was chosen -- the required size
/// differs, since MBR reserves nothing at the end of the device.
fn check_capacity(
    device_label: &str,
    device_size_bytes: u64,
    image_path: &std::path::Path,
    layout: &TargetLayout,
) -> Result<()> {
    let required = layout.total_bytes_required();
    if device_size_bytes < required {
        return Err(ArgosError::DeviceTooSmall(
            device_label.to_string(),
            image_path.to_path_buf(),
            device_size_bytes,
            required,
        ));
    }
    Ok(())
}

/// Partitions, formats, and populates `device` per `layout` -- everything
/// [`execute_write_windows_fat32`] does after its device/ISO validation.
/// Generic over the handle so unit tests can run it against a plain temp
/// file; no step in here knows or cares whether `device` is real hardware.
fn write_fat32_media<H: Read + Write + Seek>(
    device: &mut H,
    layout: &TargetLayout,
    iso: &WindowsIso,
    actions: &[CopyAction],
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Fat32WriteOutcome> {
    progress.on_phase(Phase::Partitioning);
    match layout {
        TargetLayout::Gpt(plan) => write_fat32_partition_table(device, plan)?,
        TargetLayout::MbrBios(plan) => {
            write_mbr_partition_table(device, plan)?;
            // The MBR's boot code goes into the bootstrap area the partition
            // table writer deliberately leaves alone.
            device.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
            device.write_all(MBR_BOOT_CODE).map_err(ArgosError::Io)?;
        }
    }

    progress.on_phase(Phase::FormattingFat32);
    let mut window = PartitionWindow::new(&mut *device, layout.region());
    fatfs::format_volume(
        &mut window,
        fatfs::FormatVolumeOptions::new()
            // Forced rather than size-derived: FAT16 media (what a small
            // volume would default to) is far less universally bootable,
            // and WindowsFat32Plan's size floor guarantees FAT32 is valid.
            .fat_type(fatfs::FatType::Fat32)
            // Sized from the volume: large clusters make the write cheap,
            // but a fixed large value would push a small volume below
            // FAT32's cluster minimum. See fat32_bytes_per_cluster_for.
            .bytes_per_cluster(fat32_bytes_per_cluster_for(layout.region().size_bytes))
            // fatfs writes a fixed 0x12345678 otherwise, so every volume
            // Argos ever wrote would share one identity. Windows keys
            // volumes off this serial; two media that claim to be the same
            // volume is not a state worth handing anyone.
            .volume_id(random_volume_id()?)
            // fatfs defaults to 32 sectors/track and 64 heads. Our own MBR
            // partition entry is built from 255x63 (see chs_for_lba), so
            // without this the two layers of one medium described the same
            // disk with two different geometries -- and neither matched the
            // 63/255 that Windows-made media carries.
            .sectors_per_track(CHS_SECTORS_PER_TRACK as u16)
            .heads(CHS_HEADS as u16)
            .volume_label(*b"ARGOS-WIN  "),
    )
    .map_err(ArgosError::Io)?;

    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).map_err(ArgosError::Io)?;
    let copy_result = copy_files_fat32(&fs, iso, actions, progress, cancel);
    // Unmount regardless of how the copy went -- it's what flushes the FAT
    // and FSInfo sectors -- but a copy error outranks an unmount error.
    let unmount_result = fs.unmount().map_err(ArgosError::Io);
    let copied = match copy_result {
        Ok(copied) => copied,
        Err(err) => {
            // A cancelled write stops with a mountable, plausible-looking
            // volume on the device. Destroy it, so the error's promise that
            // the media must be rewritten is enforced rather than merely
            // stated. Best-effort: the cancellation is what the caller needs
            // to hear about, so a failure to invalidate must not replace it.
            if matches!(err, ArgosError::Cancelled) {
                let mut window = PartitionWindow::new(&mut *device, layout.region());
                let _ = invalidate_fat32_volume(&mut window);
            }
            return Err(err);
        }
    };
    unmount_result?;

    // The filesystem is complete but not yet spec-conformant: fatfs puts
    // long-filename entries in front of `.` and `..`, and points `..` at the
    // root's cluster instead of zero. See repair_directory_entries.
    {
        let mut window = PartitionWindow::new(&mut *device, layout.region());
        repair_directory_entries(&mut window)?;
    }

    // Boot records last: the VBR install reads the BPB the format wrote, and
    // the bootmgr check needs the directory the copy just populated.
    {
        let start_lba = layout.start_lba()?;
        let mut window = PartitionWindow::new(&mut *device, layout.region());
        match layout {
            // UEFI firmware boots this volume by reading efi/boot/bootx64.efi
            // off it, so there is no boot code to install -- but the BPB still
            // has to say where on the disk the volume actually starts.
            TargetLayout::Gpt(_) => record_partition_start(&mut window, start_lba)?,
            TargetLayout::MbrBios(_) => {
                install_fat32_vbr(&mut window, start_lba)?;
                verify_bootmgr_reachable_by_the_vbr(&mut window)?;
            }
        }
    }

    Ok(copied)
}

/// FAT32 geometry, read back out of a formatted volume's BPB.
struct Fat32Geometry {
    sectors_per_cluster: u64,
    fat_start: u64,
    data_start: u64,
    root_cluster: u32,
}

impl Fat32Geometry {
    fn read<H: Read + Seek>(window: &mut H) -> Result<Self> {
        let mut boot = [0u8; 512];
        window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
        window.read_exact(&mut boot).map_err(ArgosError::Io)?;
        let u16at = |o: usize| u16::from_le_bytes([boot[o], boot[o + 1]]);
        let u32at = |o: usize| u32::from_le_bytes([boot[o], boot[o + 1], boot[o + 2], boot[o + 3]]);
        let reserved = u64::from(u16at(0x0E));
        let num_fats = u64::from(boot[0x10]);
        let sectors_per_fat = u64::from(u32at(0x24));
        Ok(Self {
            sectors_per_cluster: u64::from(boot[0x0D]),
            fat_start: reserved,
            data_start: reserved + num_fats * sectors_per_fat,
            root_cluster: u32at(0x2C),
        })
    }

    /// Volume-relative first sector of a cluster.
    fn cluster_lba(&self, cluster: u32) -> u64 {
        self.data_start + (u64::from(cluster) - 2) * self.sectors_per_cluster
    }

    fn next_cluster<H: Read + Seek>(&self, window: &mut H, cluster: u32) -> Result<Option<u32>> {
        let byte = u64::from(cluster) * 4;
        let mut sector = [0u8; 512];
        window
            .seek(SeekFrom::Start(
                (self.fat_start + byte / SECTOR_SIZE) * SECTOR_SIZE,
            ))
            .map_err(ArgosError::Io)?;
        window.read_exact(&mut sector).map_err(ArgosError::Io)?;
        let at = (byte % SECTOR_SIZE) as usize;
        let entry =
            u32::from_le_bytes([sector[at], sector[at + 1], sector[at + 2], sector[at + 3]])
                & 0x0FFF_FFFF;
        Ok(if entry >= 0x0FFF_FFF8 {
            None
        } else {
            Some(entry)
        })
    }
}

/// Repairs the `.` and `..` entries `fatfs` writes, which violate the FAT
/// specification in two ways at once.
///
/// What `fatfs` produces for a directory:
///
/// ```text
/// [0] long-filename entry for "."     <- should not exist
/// [1] "."   attr 0x10  first = own cluster
/// [2] long-filename entry for ".."    <- should not exist
/// [3] ".."  attr 0x10  first = parent cluster
/// ```
///
/// Two rules broken. **`.` and `..` must be the first two entries** of a
/// directory -- `fatfs` pushes them to slots 1 and 3 behind long-filename
/// entries it should never generate for names that are already valid 8.3.
/// And **`..` must hold cluster 0 when the parent is the root**, not the
/// root's real cluster number.
///
/// The OS's own checker names both:
///
/// ```text
/// Warning: Item /sources does not appear to be a subdirectory
/// Warning: `..' entry in /sources has non-zero start cluster
/// ```
///
/// macOS mounts such a volume regardless. Windows does not: on real
/// hardware `diskpart` listed the partition as FAT32 with no drive letter,
/// and Setup -- unable to reach its own installation source -- reported that
/// a media driver was missing, a message with no visible connection to
/// directory layout.
///
/// Applied to every directory, recursively: the ordering is wrong in all of
/// them, while the cluster-zero rule applies only to those whose parent is
/// the root.
fn repair_directory_entries<H: Read + Write + Seek>(window: &mut H) -> Result<u32> {
    let geometry = Fat32Geometry::read(window)?;
    let root = geometry.root_cluster;
    let mut repaired = 0;
    // (cluster, parent is the root)
    let mut pending: Vec<(u32, bool)> = subdirectories_of(window, &geometry, root)?
        .into_iter()
        .map(|c| (c, true))
        .collect();

    while let Some((cluster, parent_is_root)) = pending.pop() {
        if repair_one_directory(window, &geometry, cluster, parent_is_root)? {
            repaired += 1;
        }
        for child in subdirectories_of(window, &geometry, cluster)? {
            pending.push((child, false));
        }
    }
    window.flush().map_err(ArgosError::Io)?;
    Ok(repaired)
}

/// First clusters of every subdirectory of the directory at `cluster`,
/// skipping `.`, `..`, long-filename fragments and volume labels.
fn subdirectories_of<H: Read + Seek>(
    window: &mut H,
    geometry: &Fat32Geometry,
    cluster: u32,
) -> Result<Vec<u32>> {
    let mut found = Vec::new();
    let mut current = Some(cluster);
    while let Some(this) = current {
        for sector_index in 0..geometry.sectors_per_cluster {
            let mut sector = [0u8; 512];
            window
                .seek(SeekFrom::Start(
                    (geometry.cluster_lba(this) + sector_index) * SECTOR_SIZE,
                ))
                .map_err(ArgosError::Io)?;
            window.read_exact(&mut sector).map_err(ArgosError::Io)?;
            for entry in sector.as_chunks::<32>().0 {
                match entry[0] {
                    0x00 => break,
                    0xE5 | b'.' => continue,
                    _ => {}
                }
                if entry[11] & 0x08 != 0 || entry[11] & 0x10 == 0 {
                    continue;
                }
                let first = (u32::from(u16::from_le_bytes([entry[0x14], entry[0x15]])) << 16)
                    | u32::from(u16::from_le_bytes([entry[0x1A], entry[0x1B]]));
                if first >= 2 {
                    found.push(first);
                }
            }
        }
        current = geometry.next_cluster(window, this)?;
    }
    Ok(found)
}

/// Rewrites one directory's first sector so `.` and `..` are entries 0 and
/// 1, dropping the long-filename entries `fatfs` put in front of them and
/// zeroing `..`'s cluster when the parent is the root. Returns whether
/// anything changed.
fn repair_one_directory<H: Read + Write + Seek>(
    window: &mut H,
    geometry: &Fat32Geometry,
    cluster: u32,
    parent_is_root: bool,
) -> Result<bool> {
    let lba = geometry.cluster_lba(cluster);
    let mut sector = [0u8; 512];
    window
        .seek(SeekFrom::Start(lba * SECTOR_SIZE))
        .map_err(ArgosError::Io)?;
    window.read_exact(&mut sector).map_err(ArgosError::Io)?;

    // Locate the real (non-long-filename) `.` and `..` entries.
    let mut dot = None;
    let mut dotdot = None;
    for (index, entry) in sector.as_chunks::<32>().0.iter().enumerate() {
        if entry[11] & 0x08 != 0 || entry[0] != b'.' {
            continue;
        }
        if &entry[..11] == b".          " {
            dot = Some((index, *entry));
        } else if &entry[..11] == b"..         " {
            dotdot = Some((index, *entry));
        }
    }
    let (Some((dot_index, dot_entry)), Some((dotdot_index, mut dotdot_entry))) = (dot, dotdot)
    else {
        // Not a directory laid out the way fatfs lays them out; leave it be
        // rather than guess.
        return Ok(false);
    };

    if parent_is_root {
        dotdot_entry[0x14] = 0;
        dotdot_entry[0x15] = 0;
        dotdot_entry[0x1A] = 0;
        dotdot_entry[0x1B] = 0;
    }

    let already_correct = dot_index == 0 && dotdot_index == 1 && sector[32..64] == dotdot_entry[..];
    if already_correct {
        return Ok(false);
    }

    sector[..32].copy_from_slice(&dot_entry);
    sector[32..64].copy_from_slice(&dotdot_entry);
    // Free whatever those two used to sit behind. Only slots up to the old
    // `..` are touched, so real file entries after it are left alone.
    for index in 2..=dotdot_index.max(dot_index) {
        sector[index * 32] = 0xE5;
    }

    window
        .seek(SeekFrom::Start(lba * SECTOR_SIZE))
        .map_err(ArgosError::Io)?;
    window.write_all(&sector).map_err(ArgosError::Io)?;
    Ok(true)
}

/// Confirms `bootmgr`'s directory entry sits in the **first cluster** of the
/// root directory -- the one place [`VBR_FAT32_CODE`] looks.
///
/// The VBR searches only that cluster: following the root directory's
/// cluster chain does not fit in the 420 bytes a boot sector leaves after
/// the BPB. That is a deliberate trade -- the complexity moves here, where
/// it is cheap and testable -- but it is only sound if this check actually
/// runs. Without it, media whose root directory happened to spill would
/// format and copy perfectly and then fail to boot with no diagnostic.
///
/// Scans the raw sectors exactly as the boot code does, matching the 8.3
/// short name `BOOTMGR` and skipping long-filename and volume-label entries,
/// so this validates what the VBR will really see rather than what a
/// higher-level directory listing reports.
fn verify_bootmgr_reachable_by_the_vbr<H: Read + Write + Seek>(window: &mut H) -> Result<()> {
    let mut boot_sector = [0u8; 512];
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    window
        .read_exact(&mut boot_sector)
        .map_err(ArgosError::Io)?;

    let u16at = |o: usize| u16::from_le_bytes([boot_sector[o], boot_sector[o + 1]]);
    let u32at = |o: usize| {
        u32::from_le_bytes([
            boot_sector[o],
            boot_sector[o + 1],
            boot_sector[o + 2],
            boot_sector[o + 3],
        ])
    };
    let sectors_per_cluster = u64::from(boot_sector[0x0D]);
    let reserved_sectors = u64::from(u16at(0x0E));
    let num_fats = u64::from(boot_sector[0x10]);
    let sectors_per_fat = u64::from(u32at(0x24));
    let root_cluster = u64::from(u32at(0x2C));

    // Volume-relative, because that is what this window addresses; the boot
    // code does the same arithmetic against absolute LBAs.
    let data_start = reserved_sectors + num_fats * sectors_per_fat;
    let root_start = data_start + (root_cluster - 2) * sectors_per_cluster;

    for sector_index in 0..sectors_per_cluster {
        let mut sector = [0u8; 512];
        window
            .seek(SeekFrom::Start((root_start + sector_index) * SECTOR_SIZE))
            .map_err(ArgosError::Io)?;
        window.read_exact(&mut sector).map_err(ArgosError::Io)?;

        for entry in sector.as_chunks::<32>().0 {
            match entry[0] {
                0x00 => break,    // no further entries anywhere
                0xE5 => continue, // deleted
                _ => {}
            }
            // 0x08 covers volume labels and long-filename fragments alike:
            // an LFN entry's attribute is 0x0F, which has 0x08 set.
            if entry[11] & 0x08 != 0 {
                continue;
            }
            if &entry[..11] == b"BOOTMGR    " {
                return Ok(());
            }
        }
    }

    Err(ArgosError::Io(std::io::Error::other(
        "bootmgr's directory entry is not in the root directory's first cluster, which is the \
         only place the BIOS boot record looks -- refusing to write media that would not boot",
    )))
}

/// The read-back half: confirms the GPT matches `layout`, then per-file
/// hashes through a read-only `fatfs` against a fresh read of the ISO.
/// Same handle-generic posture as [`write_fat32_media`].
fn verify_fat32_media<H: Read + Write + Seek>(
    device: &mut H,
    layout: &TargetLayout,
    iso: &WindowsIso,
    actions: &[CopyAction],
    progress: &dyn ProgressSink,
) -> Result<u64> {
    progress.on_phase(Phase::Verifying);
    match layout {
        TargetLayout::Gpt(plan) => {
            let observed = read_observed_fat32_partition(device)?;
            verify_windows_fat32_layout(plan, observed)?;
        }
        TargetLayout::MbrBios(plan) => verify_mbr_layout(device, plan)?,
    }

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
    let mut window = PartitionWindow::new(&mut *device, layout.region());
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
    // Clear the bootstrap area first. `gptman` writes the protective MBR
    // "starting at byte 446", so bytes 0..446 survive untouched -- and on a
    // stick that previously held, say, an isohybrid Linux ISO, that means a
    // stale bootloader stays behind. A legacy BIOS then finds and runs it,
    // producing errors from software that is no longer on the medium at all
    // ("isolinux.bin missing or corrupt", reported from real hardware). This
    // media is UEFI-only by design; leaving executable remnants that claim
    // otherwise is worse than leaving nothing.
    device.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    device
        .write_all(&[0u8; MBR_BOOTSTRAP_BYTES])
        .map_err(ArgosError::Io)?;

    let mut gpt = gptman::GPT::new_from(device, SECTOR_SIZE, random_guid()?)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;

    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID,
        unique_partition_guid: random_guid()?,
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

/// Builds and writes a single-partition **MBR** for BIOS-bootable media
/// (phase 3 M6.2, backlog #45): one FAT32 (LBA) partition marked active,
/// placed exactly where `layout` computed.
///
/// The counterpart to [`write_fat32_partition_table`], which writes a GPT for
/// the UEFI path. Everything downstream -- formatting and populating the
/// partition through a [`PartitionWindow`] -- is identical either way; only
/// the table describing the partition differs.
///
/// **This alone does not produce bootable media.** A legacy BIOS reads sector
/// 0's boot code, which must find the active partition and chain-load its
/// boot sector; and that partition's own boot sector must locate `bootmgr`.
/// Neither exists yet -- they are M6.3 and M6.4. `mbrman` exposes the 440-byte
/// bootstrap area as [`mbrman::MBRHeader::bootstrap_code`], so writing it will
/// slot in here rather than needing a separate pass over the device. Until
/// then this is deliberately not wired into any CLI path: it would build media
/// that partitions and formats correctly and then fails to boot with no
/// explanation.
/// A volume serial number, from the same `/dev/urandom` the GUIDs come
/// from. Real formatters derive one from the clock; either way the point is
/// that two volumes must not claim the same identity.
fn random_volume_id() -> Result<u32> {
    let bytes = random_guid()?;
    // Never zero: some tools treat an all-zero serial as "unset".
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) | 1)
}

/// Bytes of sector 0 reserved for boot code, before the disk signature at
/// 440 and the partition table at 446.
const MBR_BOOTSTRAP_BYTES: usize = 440;

/// Argos's MBR boot code (phase 3 M6.3, backlog #45), assembled from
/// `asm/mbr.asm`. Occupies the 440-byte bootstrap area that
/// [`write_mbr_partition_table`] leaves untouched.
const MBR_BOOT_CODE: &[u8] = include_bytes!("../asm/mbr.bin");

/// Argos's FAT32 volume boot record (phase 3 M6.4, backlog #45), assembled
/// from `asm/vbr_fat32.asm`. Installed into the partition's first sector so
/// a legacy BIOS can chain from the MBR through to `bootmgr`.
const VBR_FAT32_CODE: &[u8] = include_bytes!("../asm/vbr_fat32.bin");

/// Where the BPB `fatfs` writes ends and our boot code begins. Bytes 0..3
/// are the jump to that code, 3..90 the BPB itself.
const VBR_CODE_OFFSET: usize = 90;

/// BPB offset of "hidden sectors": the number of sectors before this volume
/// on the disk, i.e. the partition's start LBA.
const BPB_HIDDEN_SECTORS_OFFSET: usize = 0x1C;

/// BPB offset of "bytes per sector".
const BPB_BYTES_PER_SECTOR_OFFSET: usize = 0x0B;

/// BPB offset of the sector holding FAT32's backup copy of the boot sector
/// (6 in practice).
const BPB_BACKUP_BOOT_SECTOR_OFFSET: usize = 0x32;

/// Reads the volume's boot sector, hands it to `edit`, and writes the result
/// back to both the boot sector and FAT32's backup copy of it.
///
/// Keeping the two in step is not optional. Windows' own tools update both,
/// chkdsk compares them, and a recovery that fell back to a stale copy would
/// produce media that had booted once and then stopped. Cheap to keep
/// consistent; expensive to debug if not.
fn edit_boot_sector<H, F>(window: &mut H, edit: F) -> Result<()>
where
    H: Read + Write + Seek,
    F: FnOnce(&mut [u8; 512]) -> Result<()>,
{
    let mut sector = [0u8; 512];
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    window.read_exact(&mut sector).map_err(ArgosError::Io)?;

    edit(&mut sector)?;

    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    window.write_all(&sector).map_err(ArgosError::Io)?;

    let backup_sector = u16::from_le_bytes([
        sector[BPB_BACKUP_BOOT_SECTOR_OFFSET],
        sector[BPB_BACKUP_BOOT_SECTOR_OFFSET + 1],
    ]);
    if backup_sector != 0 && backup_sector != 0xFFFF {
        window
            .seek(SeekFrom::Start(u64::from(backup_sector) * SECTOR_SIZE))
            .map_err(ArgosError::Io)?;
        window.write_all(&sector).map_err(ArgosError::Io)?;
    }

    window.flush().map_err(ArgosError::Io)?;
    Ok(())
}

/// Writes the partition's start LBA into the BPB's hidden-sectors field.
///
/// `fatfs` formats through a [`PartitionWindow`], so it only ever sees a
/// volume starting at offset 0 and writes 0 here -- correct from its point of
/// view, and untrue on the disk. Two separate consumers care:
///
/// * Our FAT32 VBR, on the MBR/BIOS path. INT 13h takes absolute disk LBAs,
///   so a boot record that trusts a zero here reads from the start of the
///   *disk* instead of the start of the partition. Found exactly that way, by
///   a boot that produced no output at all.
/// * Windows, on every path. This is why the call is not inside
///   `install_fat32_vbr` any more: the GPT/UEFI path installs no boot code and
///   so used to skip the patch entirely, shipping volumes that claimed to
///   begin at sector 0 of the disk. A Rufus-written FAT32 stick that WinPE
///   mounts without complaint carries the real offset here; ours carried 0.
fn record_partition_start<H: Read + Write + Seek>(
    window: &mut H,
    partition_start_lba: u32,
) -> Result<()> {
    edit_boot_sector(window, |sector| {
        sector[BPB_HIDDEN_SECTORS_OFFSET..BPB_HIDDEN_SECTORS_OFFSET + 4]
            .copy_from_slice(&partition_start_lba.to_le_bytes());
        Ok(())
    })
}

/// Installs [`VBR_FAT32_CODE`] into an already-formatted FAT32 partition,
/// **preserving the BPB** and recording the partition's start in it.
///
/// Only bytes 0..3 (the jump) and 90..512 (code and signature) are replaced.
/// The boot code reads its geometry -- cluster size, FAT location, root
/// directory cluster -- out of the BPB at runtime rather than assuming any, so
/// overwriting the BPB would leave it computing addresses from zeros.
fn install_fat32_vbr<H: Read + Write + Seek>(
    window: &mut H,
    partition_start_lba: u32,
) -> Result<()> {
    edit_boot_sector(window, |sector| {
        // The boot code's addressing arithmetic is written for 512-byte
        // sectors; refuse rather than produce media that miscomputes every LBA.
        let bytes_per_sector = u16::from_le_bytes([
            sector[BPB_BYTES_PER_SECTOR_OFFSET],
            sector[BPB_BYTES_PER_SECTOR_OFFSET + 1],
        ]);
        if u64::from(bytes_per_sector) != SECTOR_SIZE {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "the filesystem reports {bytes_per_sector}-byte sectors; the boot record requires {SECTOR_SIZE}"
            ))));
        }

        sector[..3].copy_from_slice(&VBR_FAT32_CODE[..3]);
        sector[VBR_CODE_OFFSET..].copy_from_slice(&VBR_FAT32_CODE[VBR_CODE_OFFSET..]);
        sector[BPB_HIDDEN_SECTORS_OFFSET..BPB_HIDDEN_SECTORS_OFFSET + 4]
            .copy_from_slice(&partition_start_lba.to_le_bytes());
        Ok(())
    })
}

/// Test-only re-export of [`write_fat32_media`], so the QEMU boot-chain test
/// can assert against media the product's own write path produced rather
/// than an approximation of it assembled by the test.
///
/// Cancellation is deliberately not exposed here: an integration test that
/// wants it builds the token itself and calls `write_fat32_media`, which the
/// in-module tests do.
pub fn write_fat32_media_for_test<H: Read + Write + Seek>(
    device: &mut H,
    layout: &TargetLayout,
    iso: &WindowsIso,
    actions: &[CopyAction],
    progress: &dyn ProgressSink,
) -> Result<Fat32WriteOutcome> {
    write_fat32_media(device, layout, iso, actions, progress, &CancelToken::new())
}

/// Test-only re-export of [`install_fat32_vbr`], so the QEMU boot-chain test
/// installs the boot record exactly the way a real write would.
pub fn install_fat32_vbr_for_test<H: Read + Write + Seek>(
    window: &mut H,
    partition_start_lba: u32,
) -> Result<()> {
    install_fat32_vbr(window, partition_start_lba)
}

/// Test-only re-export of [`write_mbr_partition_table`], so the QEMU
/// boot-chain test (M6.5) builds its disk image with the very same
/// partition-table code a real write would use -- testing a hand-rolled
/// approximation of it would prove nothing about the real thing.
pub fn write_mbr_partition_table_for_test<H: Read + Write + Seek>(
    device: &mut H,
    layout: &WindowsMbrPlan,
) -> Result<()> {
    write_mbr_partition_table(device, layout)
}

/// How many sectors a GPT occupies at each end of a disk: one header plus the
/// 32 sectors that hold 128 entries of 128 bytes.
const GPT_SECTORS_PER_COPY: u64 = 33;

/// Erases any GPT the device carried before this MBR write.
///
/// `mbrman` writes sector 0 and nothing else, so a stick previously written
/// with the GPT layout keeps its primary header at LBA 1, its entry array
/// behind that, and its backup header in the device's last sector -- all with
/// CRCs that still validate. What is left is a disk that contradicts itself: a
/// structurally valid GPT whose sector 0 is not the protective 0xEE entry a
/// GPT requires, but a real bootable FAT32 entry.
///
/// Windows does not hand a volume a drive letter off a disk in that state. The
/// symptom is nasty because the media still *boots*: our MBR and VBR chain
/// through to `bootmgr`, WinPE starts, and only then does Setup report that it
/// cannot find the installation source.
///
/// Found on real hardware, and it is also why emulation never reproduced the
/// failure -- the QEMU harness builds its media in a freshly truncated file,
/// which has no stale GPT to leave behind, while a lab stick gets recycled
/// from one layout to the other.
fn erase_any_gpt<H: Read + Write + Seek>(device: &mut H) -> Result<()> {
    let device_size = device.seek(SeekFrom::End(0)).map_err(ArgosError::Io)?;
    let zeros = vec![0u8; (GPT_SECTORS_PER_COPY * SECTOR_SIZE) as usize];

    // The primary copy: LBA 1 through 33. Our partition starts at LBA 2048, so
    // this never reaches the filesystem.
    device
        .seek(SeekFrom::Start(SECTOR_SIZE))
        .map_err(ArgosError::Io)?;
    device.write_all(&zeros).map_err(ArgosError::Io)?;

    // The backup copy lives in the last sectors of the *device*, past the end
    // of a partition that is sized to its contents.
    if device_size >= (GPT_SECTORS_PER_COPY + 1) * SECTOR_SIZE {
        device
            .seek(SeekFrom::End(-((zeros.len()) as i64)))
            .map_err(ArgosError::Io)?;
        device.write_all(&zeros).map_err(ArgosError::Io)?;
    }

    device.flush().map_err(ArgosError::Io)?;
    Ok(())
}

fn write_mbr_partition_table<H: Read + Write + Seek>(
    device: &mut H,
    layout: &WindowsMbrPlan,
) -> Result<()> {
    let (starting_lba, sectors) = layout.partition_sectors().ok_or_else(|| {
        ArgosError::Io(std::io::Error::other(
            "the partition is larger than an MBR entry's 32-bit LBA fields can describe (>2TiB)",
        ))
    })?;

    erase_any_gpt(device)?;

    let sector_size = u32::try_from(SECTOR_SIZE).expect("SECTOR_SIZE is 512");
    // A random disk signature, like the GPT path's GUIDs: Windows uses it to
    // identify the disk, and a fixed value would make two Argos-written
    // sticks collide in its registry.
    let signature = random_guid()?;
    let mut mbr = mbrman::MBR::new_from(
        device,
        sector_size,
        [signature[0], signature[1], signature[2], signature[3]],
    )
    .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;

    // CHS is obsolete for addressing -- our boot code and every modern
    // firmware use the LBA fields below -- but the bytes are not optional.
    // **Windows validates them.** Zeroed CHS produces a table a BIOS boots
    // and that Linux and macOS mount, while Windows silently declines to
    // mount the volume, surfacing as install media that starts Setup and
    // then reports a missing media driver.
    let (start_c, start_h, start_s) = chs_for_lba(starting_lba);
    let (end_c, end_h, end_s) = chs_for_lba(starting_lba.saturating_add(sectors).saturating_sub(1));
    mbr[1] = mbrman::MBRPartitionEntry {
        boot: mbrman::BOOT_ACTIVE,
        first_chs: mbrman::CHS::new(start_c, start_h, start_s),
        sys: MBR_FAT32_LBA_PARTITION_TYPE,
        last_chs: mbrman::CHS::new(end_c, end_h, end_s),
        starting_lba,
        sectors,
    };

    mbr.write_into(device)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))?;
    device.flush().map_err(ArgosError::Io)?;
    Ok(())
}

/// The MBR counterpart of [`verify_windows_fat32_layout`]: reads sector 0
/// back and confirms it describes the partition the plan asked for, and that
/// the boot code is actually there.
///
/// Checks the boot signature, the active flag, the FAT32 type byte and the
/// LBA fields by raw offset -- the same way the tests do, and for the same
/// reason: this must reflect what a BIOS will read, not what the library
/// that wrote it would report back.
fn verify_mbr_layout<H: Read + Seek>(device: &mut H, plan: &WindowsMbrPlan) -> Result<()> {
    let mut sector = [0u8; 512];
    device.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    device.read_exact(&mut sector).map_err(ArgosError::Io)?;

    let mismatch = |what: &str| ArgosError::WindowsPartitionLayoutMismatch(what.to_string());

    if sector[510..512] != [0x55, 0xAA] {
        return Err(mismatch("sector 0 has no boot signature"));
    }
    if sector[..MBR_BOOT_CODE.len()] != *MBR_BOOT_CODE {
        return Err(mismatch("the MBR boot code is missing or does not match"));
    }

    // A GPT surviving underneath an MBR is the hybrid state erase_any_gpt
    // exists to prevent; media in it boots and then gets no drive letter.
    let mut lba1 = [0u8; 512];
    device
        .seek(SeekFrom::Start(SECTOR_SIZE))
        .map_err(ArgosError::Io)?;
    device.read_exact(&mut lba1).map_err(ArgosError::Io)?;
    if &lba1[..8] == b"EFI PART" {
        return Err(mismatch(
            "a GPT header is still present at LBA 1, so the disk claims both schemes",
        ));
    }

    let entry = &sector[0x1BE..0x1BE + 16];
    if entry[0] != MBR_BOOTABLE_FLAG {
        return Err(mismatch("partition 1 is not marked active"));
    }
    if entry[4] != MBR_FAT32_LBA_PARTITION_TYPE {
        return Err(mismatch("partition 1 is not a FAT32 (LBA) partition"));
    }

    let (expected_start, expected_sectors) = plan.partition_sectors().ok_or_else(|| {
        mismatch("the expected partition is larger than an MBR entry can describe")
    })?;
    let start = u32::from_le_bytes(entry[8..12].try_into().expect("4 bytes"));
    let sectors = u32::from_le_bytes(entry[12..16].try_into().expect("4 bytes"));
    if start != expected_start {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(format!(
            "partition 1 starts at LBA {start}, expected {expected_start}"
        )));
    }
    if sectors < expected_sectors {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(format!(
            "partition 1 is {sectors} sectors, expected at least {expected_sectors}"
        )));
    }
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
/// The payload a cancelled copy carries inside an `io::Error`.
///
/// Cancellation has to travel out through `copy_and_hash` and `wim::split`,
/// which speak `io::Result` and know nothing about [`CancelToken`]. Carrying a
/// dedicated type rather than a message means the other end tells a cancelled
/// write apart from a real I/O failure by `downcast`, not by string matching.
#[derive(Debug)]
struct CopyCancelled;

impl std::fmt::Display for CopyCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the write was cancelled")
    }
}

impl std::error::Error for CopyCancelled {}

fn cancelled_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, CopyCancelled)
}

/// Maps an error out of the copy back into the domain: a cancellation
/// surfaces as [`ArgosError::Cancelled`], anything else stays an I/O error.
fn copy_error(err: std::io::Error) -> ArgosError {
    if err
        .get_ref()
        .is_some_and(|inner| inner.is::<CopyCancelled>())
    {
        ArgosError::Cancelled
    } else {
        ArgosError::Io(err)
    }
}

/// Wraps a writer so it refuses to keep going once `cancel` is set.
///
/// Checking on every `write` puts the granularity at one buffer of
/// `copy_and_hash`'s copy loop, which is the same responsiveness
/// `write::dd_mode` has had since v1 -- and it applies to a 4GB `.swm` part
/// as much as to a 128-byte `autorun.inf`.
struct CancellableWriter<'a, W> {
    inner: W,
    cancel: &'a CancelToken,
}

impl<W: Write> Write for CancellableWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Destroys the FAT32 volume's boot sector and its backup.
///
/// A cancelled write leaves media that **mounts**. The partition table and
/// the filesystem are written before the copy starts, so an interrupted write
/// yields a structurally valid FAT32 volume, labelled `ARGOS-WIN`, that lists
/// files and looks plausible -- while missing files, or holding a truncated
/// `install.swm`. That is unlike DD mode, where partial media is obviously
/// broken and nobody is fooled.
///
/// [`ArgosError::Cancelled`] already promises the device "must be rewritten
/// before use". This is what makes the promise true: with both boot sectors
/// gone, no operating system will mount the volume, and a half-written stick
/// cannot be mistaken for a good one on a lab bench.
fn invalidate_fat32_volume<H: Read + Write + Seek>(window: &mut H) -> Result<()> {
    let mut sector = [0u8; 512];
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    window.read_exact(&mut sector).map_err(ArgosError::Io)?;

    // Read where the backup lives before destroying the field that says so.
    let backup_sector = u16::from_le_bytes([
        sector[BPB_BACKUP_BOOT_SECTOR_OFFSET],
        sector[BPB_BACKUP_BOOT_SECTOR_OFFSET + 1],
    ]);

    let zeros = [0u8; 512];
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    window.write_all(&zeros).map_err(ArgosError::Io)?;
    if backup_sector != 0 && backup_sector != 0xFFFF {
        window
            .seek(SeekFrom::Start(u64::from(backup_sector) * SECTOR_SIZE))
            .map_err(ArgosError::Io)?;
        window.write_all(&zeros).map_err(ArgosError::Io)?;
    }
    window.flush().map_err(ArgosError::Io)?;
    Ok(())
}

fn copy_files_fat32<H: Read + Write + Seek>(
    fs: &fatfs::FileSystem<H>,
    iso: &WindowsIso,
    actions: &[CopyAction],
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Fat32WriteOutcome> {
    progress.on_phase(Phase::CopyingFiles);

    let total_bytes: u64 = actions.iter().map(CopyAction::bytes_on_target).sum();
    let mut bytes_done = 0u64;
    let mut hashes = Vec::new();
    let mut files_copied = 0u64;

    for action in actions {
        // Checked here as well as inside CancellableWriter: a long run of
        // tiny files would otherwise sit between writes for a while, and a
        // cancellation asked for before the first byte should not have to
        // wait for one.
        if cancel.is_cancelled() {
            return Err(ArgosError::Cancelled);
        }
        match action {
            CopyAction::Direct { path, size } => {
                let source = open_iso_file(iso, path)?;
                let mut dest = CancellableWriter {
                    inner: create_file_at(fs, path)?,
                    cancel,
                };
                // One pass: every byte read from the ISO is both hashed and
                // written, same as the NTFS path's copy_files.
                let hash = copy_and_hash(source, &mut dest, |chunk_done| {
                    progress.on_progress(bytes_done + chunk_done, total_bytes);
                })
                .map_err(copy_error)?;
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
                    cancel,
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
    cancel: &CancelToken,
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
            cancel: &'a CancelToken,
        }
        impl<H: Read + Write + Seek> Write for PartSink<'_, H> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                // The parts are the big files -- one of them runs to 4GB --
                // so this is the check that makes cancelling a Windows write
                // feel immediate rather than eventual.
                if self.cancel.is_cancelled() {
                    return Err(cancelled_error());
                }
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
                        cancel,
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
        .map_err(copy_error)?;

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
    fn device_file_for(layout: &TargetLayout) -> std::fs::File {
        let file = tempfile::tempfile().unwrap();
        file.set_len(layout.total_bytes_required()).unwrap();
        file
    }

    /// Real-hardware regression guard: `mediadiff.py` found the backup boot
    /// sector (and its dirty-flag byte, offset 0x41) diverging from the
    /// primary on a physical stick written with `--layout fat32-bios`.
    /// `installing_the_vbr_keeps_the_backup_boot_sector_in_sync` above
    /// exercises `install_fat32_vbr` right after formatting, skipping the
    /// steps in between (copying files, `fs.unmount()`) and, crucially,
    /// the `BufferedDevice` wrapper `execute_write_windows_fat32` always
    /// uses in production -- this test goes through the exact same
    /// `write_fat32_media` call, over `BufferedDevice`, with real file
    /// copying, to close that gap. It passes deterministically (5/5 runs)
    /// on this software path, so the divergence seen on hardware did not
    /// reproduce here; kept as a standing check in case a future change
    /// reintroduces it.
    #[test]
    fn backup_boot_sector_matches_the_primary_after_a_buffered_mbr_write() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let plan = WindowsMbrPlan::new(actions.iter().map(CopyAction::bytes_on_target).sum());
        let layout = TargetLayout::MbrBios(plan);
        let file = device_file_for(&layout);
        let mut buffered = crate::partition_io::BufferedDevice::new(file).unwrap();

        write_fat32_media(
            &mut buffered,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();
        buffered.flush().unwrap();
        let mut file = buffered.into_inner();

        let region = layout.region();
        let mut primary = [0u8; 512];
        file.seek(SeekFrom::Start(region.start_offset_bytes))
            .unwrap();
        file.read_exact(&mut primary).unwrap();
        let mut backup = [0u8; 512];
        file.seek(SeekFrom::Start(region.start_offset_bytes + 6 * 512))
            .unwrap();
        file.read_exact(&mut backup).unwrap();

        assert_eq!(
            primary[0x41], 0x00,
            "the filesystem-dirty flag should be clear after a clean unmount"
        );
        assert_eq!(
            primary, backup,
            "primary and backup boot sectors diverged after a buffered write"
        );
    }

    #[test]
    fn write_then_verify_round_trips_on_a_plain_file() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);

        let outcome = write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();
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
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        let observed = read_observed_fat32_partition(&mut device).unwrap();
        assert_eq!(
            observed.partition_type_guid,
            MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID
        );
        assert_eq!(
            observed.region.start_offset_bytes,
            layout.region().start_offset_bytes
        );
        assert_eq!(observed.region.size_bytes, layout.region().size_bytes);

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
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        let window = PartitionWindow::new(&mut device, layout.region());
        let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
    }

    /// The GPT/UEFI path installs no boot code, and for a long time that meant
    /// it also skipped the hidden-sectors patch that lives with the VBR
    /// install -- so it shipped volumes whose BPB claimed they began at sector
    /// 0 of the disk. Found by dumping a Rufus-written FAT32 stick that WinPE
    /// mounts without complaint (2048 there) beside one of ours (0).
    #[test]
    fn the_gpt_write_records_the_partition_start_in_the_bpb() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        let expected = layout.start_lba().unwrap();
        assert_ne!(
            expected, 0,
            "the test is vacuous if the partition starts at 0"
        );

        let mut window = PartitionWindow::new(&mut device, layout.region());
        let mut boot = [0u8; 512];
        window.seek(SeekFrom::Start(0)).unwrap();
        window.read_exact(&mut boot).unwrap();
        assert_eq!(
            u32::from_le_bytes(
                boot[BPB_HIDDEN_SECTORS_OFFSET..BPB_HIDDEN_SECTORS_OFFSET + 4]
                    .try_into()
                    .unwrap()
            ),
            expected,
        );

        // And the backup copy has to agree, or chkdsk sees a volume that
        // contradicts itself.
        let backup = u16::from_le_bytes([
            boot[BPB_BACKUP_BOOT_SECTOR_OFFSET],
            boot[BPB_BACKUP_BOOT_SECTOR_OFFSET + 1],
        ]);
        assert_ne!(backup, 0);
        let mut copy = [0u8; 512];
        window
            .seek(SeekFrom::Start(u64::from(backup) * SECTOR_SIZE))
            .unwrap();
        window.read_exact(&mut copy).unwrap();
        assert_eq!(copy, boot);
    }

    /// The BPB and the MBR partition entry describe the same disk, so they had
    /// better agree on its geometry. fatfs defaults to 32x64; chs_for_lba uses
    /// 255x63; Windows-made media carries 63/255.
    /// Where FAT32 keeps its spare boot sector. Production code reads this
    /// out of the BPB rather than assuming it; the test has to know it,
    /// because by the time it looks the BPB is deliberately gone.
    const FAT32_BACKUP_BOOT_SECTOR: u64 = 6;

    /// Cancels the write the first time it is told about progress, so the
    /// cancellation lands in the middle of a real copy rather than before it.
    struct CancelOnFirstProgress {
        cancel: CancelToken,
    }

    impl ProgressSink for CancelOnFirstProgress {
        fn on_phase(&self, _phase: Phase) {}
        fn on_progress(&self, _done: u64, _total: u64) {
            self.cancel.cancel();
        }
    }

    #[test]
    fn a_write_cancelled_mid_copy_reports_cancelled() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);

        let cancel = CancelToken::new();
        let progress = CancelOnFirstProgress {
            cancel: cancel.clone(),
        };
        let err = write_fat32_media(&mut device, &layout, &iso, &actions, &progress, &cancel)
            .expect_err("a cancelled write must not report success");

        assert!(
            matches!(err, ArgosError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
    }

    #[test]
    fn a_write_cancelled_before_it_starts_reports_cancelled() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);

        let cancel = CancelToken::new();
        cancel.cancel();
        let err = write_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress, &cancel)
            .expect_err("a cancelled write must not report success");

        assert!(matches!(err, ArgosError::Cancelled), "got {err:?}");
    }

    /// The reason cancellation needed more than an early return. Unlike DD
    /// mode, where an interrupted write leaves obviously broken media, the
    /// FAT32 path writes the partition table and formats the volume *before*
    /// copying, so stopping mid-copy would otherwise leave a mountable,
    /// correctly-labelled volume that merely happens to be missing files.
    /// `ArgosError::Cancelled` promises the device must be rewritten; this is
    /// what makes that true.
    #[test]
    fn a_cancelled_write_leaves_media_that_will_not_mount() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);

        // The premise: the same write, uncancelled, does leave a mountable
        // volume. Without this the assertion below could pass for the wrong
        // reason -- a write that never got as far as formatting.
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();
        {
            let window = PartitionWindow::new(&mut device, layout.region());
            fatfs::FileSystem::new(window, fatfs::FsOptions::new())
                .expect("a completed write must leave a mountable volume");
        }

        let cancel = CancelToken::new();
        let progress = CancelOnFirstProgress {
            cancel: cancel.clone(),
        };
        let err = write_fat32_media(&mut device, &layout, &iso, &actions, &progress, &cancel)
            .expect_err("the write was cancelled");
        assert!(matches!(err, ArgosError::Cancelled), "got {err:?}");

        let window = PartitionWindow::new(&mut device, layout.region());
        assert!(
            fatfs::FileSystem::new(window, fatfs::FsOptions::new()).is_err(),
            "a cancelled write must leave a volume no operating system will mount"
        );

        // Both copies, not just the primary: a recovery tool that fell back to
        // the backup boot sector would otherwise resurrect the half-written
        // volume.
        let mut window = PartitionWindow::new(&mut device, layout.region());
        let mut backup = [0u8; 512];
        window
            .seek(SeekFrom::Start(FAT32_BACKUP_BOOT_SECTOR * SECTOR_SIZE))
            .unwrap();
        window.read_exact(&mut backup).unwrap();
        assert_eq!(
            backup, [0u8; 512],
            "the backup boot sector must be destroyed too"
        );
    }

    /// A stick recycled from `--layout fat32` to `--layout fat32-bios` used to
    /// keep its entire GPT -- primary header at LBA 1, entry array behind it,
    /// backup header in the device's last sector, all CRCs still valid --
    /// underneath an MBR whose first entry is a bootable FAT32 partition
    /// rather than the protective 0xEE a GPT requires.
    ///
    /// Media in that state boots: the MBR and VBR chain through to `bootmgr`
    /// and WinPE starts. Windows then declines to give the volume a drive
    /// letter, and Setup reports it cannot find the installation source.
    ///
    /// Emulation never caught it because the QEMU harness builds its media in
    /// a freshly truncated file. Only a recycled device reproduces it, which is
    /// what this test is.
    #[test]
    fn writing_the_mbr_layout_erases_a_gpt_left_by_a_previous_write() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let gpt_layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mbr_plan = WindowsMbrPlan::new(actions.iter().map(CopyAction::bytes_on_target).sum());
        let mbr_layout = TargetLayout::MbrBios(mbr_plan);

        let mut device = tempfile::tempfile().unwrap();
        device
            .set_len(
                gpt_layout
                    .total_bytes_required()
                    .max(mbr_layout.total_bytes_required()),
            )
            .unwrap();

        write_fat32_media(
            &mut device,
            &gpt_layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        // The premise: the first write really does leave a GPT behind. If this
        // ever stops holding, the test below stops testing anything.
        let mut lba1 = [0u8; 512];
        device.seek(SeekFrom::Start(SECTOR_SIZE)).unwrap();
        device.read_exact(&mut lba1).unwrap();
        assert_eq!(&lba1[..8], b"EFI PART");

        write_fat32_media(
            &mut device,
            &mbr_layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        device.seek(SeekFrom::Start(SECTOR_SIZE)).unwrap();
        device.read_exact(&mut lba1).unwrap();
        assert_ne!(
            &lba1[..8],
            b"EFI PART",
            "the MBR write left the primary GPT header behind"
        );

        let mut last = [0u8; 512];
        device.seek(SeekFrom::End(-(SECTOR_SIZE as i64))).unwrap();
        device.read_exact(&mut last).unwrap();
        assert_ne!(
            &last[..8],
            b"EFI PART",
            "the MBR write left the backup GPT header behind"
        );

        // And the layout check now refuses media in the hybrid state.
        verify_mbr_layout(&mut device, &mbr_plan).unwrap();
    }

    #[test]
    fn the_bpb_geometry_matches_the_one_the_partition_entries_are_built_from() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        for layout in [
            TargetLayout::Gpt(fat32_layout_for(&actions)),
            TargetLayout::MbrBios(WindowsMbrPlan::new(
                actions.iter().map(CopyAction::bytes_on_target).sum::<u64>(),
            )),
        ] {
            let mut device = device_file_for(&layout);
            write_fat32_media(
                &mut device,
                &layout,
                &iso,
                &actions,
                &NoopProgress,
                &CancelToken::new(),
            )
            .unwrap();

            let mut window = PartitionWindow::new(&mut device, layout.region());
            let mut boot = [0u8; 512];
            window.seek(SeekFrom::Start(0)).unwrap();
            window.read_exact(&mut boot).unwrap();

            assert_eq!(
                u16::from_le_bytes(boot[0x18..0x1A].try_into().unwrap()),
                CHS_SECTORS_PER_TRACK as u16,
                "sectors per track"
            );
            assert_eq!(
                u16::from_le_bytes(boot[0x1A..0x1C].try_into().unwrap()),
                CHS_HEADS as u16,
                "heads"
            );
        }
    }

    #[test]
    fn verify_fails_when_a_written_file_is_corrupted() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        // Corrupt the first byte of the largest file's content by finding it
        // through the filesystem itself (window + fatfs, read-write).
        let largest = files.iter().max_by_key(|f| f.size).unwrap();
        {
            let window = PartitionWindow::new(&mut device, layout.region());
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
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut blank = Cursor::new(vec![0u8; 4 * 1024 * 1024]);
        assert!(verify_fat32_media(&mut blank, &layout, &iso, &actions, &NoopProgress).is_err());
    }

    /// Every volume must have its own serial: fatfs writes a fixed
    /// 0x12345678, so without this every stick Argos ever wrote would claim
    /// the same identity, and Windows keys volumes off exactly this field.
    #[test]
    fn each_written_volume_gets_its_own_serial_number() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));

        let mut serials = Vec::new();
        for _ in 0..2 {
            let mut device = device_file_for(&layout);
            write_fat32_media(
                &mut device,
                &layout,
                &iso,
                &actions,
                &NoopProgress,
                &CancelToken::new(),
            )
            .unwrap();
            let mut boot = [0u8; 512];
            device
                .seek(SeekFrom::Start(layout.region().start_offset_bytes))
                .unwrap();
            device.read_exact(&mut boot).unwrap();
            serials.push(u32::from_le_bytes(boot[0x43..0x47].try_into().unwrap()));
        }
        assert_ne!(serials[0], serials[1], "two writes shared a volume serial");
        assert_ne!(
            serials[0], 0x1234_5678,
            "fatfs's placeholder serial was kept"
        );
        assert_ne!(serials[0], 0, "an all-zero serial reads as unset");
    }

    /// Regression guard for a bug that was invisible outside an actual boot:
    /// `fatfs` formats through a PartitionWindow, so it writes 0 into the
    /// BPB's hidden-sectors field -- correct for a volume that begins at
    /// offset 0, and fatal for a boot record, which addresses the disk with
    /// absolute LBAs. The first QEMU run of the full chain reported exactly
    /// this: geometry computed, bootmgr not found, because the search ran
    /// against the start of the disk instead of the partition.
    #[test]
    fn installing_the_vbr_patches_the_partition_start_into_the_bpb() {
        let layout = WindowsMbrPlan::new(600_000_000);
        let (start_lba, _) = layout.partition_sectors().unwrap();
        let mut device = tempfile::tempfile().unwrap();
        device.set_len(layout.total_bytes_required()).unwrap();

        let mut window = PartitionWindow::new(&mut device, layout.windows_partition);
        fatfs::format_volume(
            &mut window,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"ARGOS-WIN  "),
        )
        .unwrap();

        // What fatfs leaves behind, and why the boot record cannot trust it.
        let mut before = [0u8; 512];
        window.seek(SeekFrom::Start(0)).unwrap();
        window.read_exact(&mut before).unwrap();
        assert_eq!(
            u32::from_le_bytes(before[0x1C..0x20].try_into().unwrap()),
            0,
            "fatfs is expected to write 0 here; if it ever stops, this test's premise changed"
        );

        install_fat32_vbr(&mut window, start_lba).unwrap();

        let mut after = [0u8; 512];
        window.seek(SeekFrom::Start(0)).unwrap();
        window.read_exact(&mut after).unwrap();
        assert_eq!(
            u32::from_le_bytes(after[0x1C..0x20].try_into().unwrap()),
            start_lba,
            "the boot record needs the partition's absolute start LBA here"
        );

        // The rest of the BPB must be exactly as fatfs left it: the boot code
        // reads its geometry from these fields at runtime.
        assert_eq!(&after[3..0x1C], &before[3..0x1C]);
        assert_eq!(&after[0x20..90], &before[0x20..90]);

        // And the sector must still be a boot sector.
        assert_eq!(&after[510..], &[0x55, 0xAA]);
    }

    /// FAT32's backup boot sector must match the primary. A stale copy makes
    /// the volume self-contradictory -- the backup would still say the volume
    /// starts at disk sector 0 and carry no boot code -- and anything that
    /// fell back to it would get media that had booted once and then stopped.
    #[test]
    fn installing_the_vbr_keeps_the_backup_boot_sector_in_sync() {
        let layout = WindowsMbrPlan::new(600_000_000);
        let (start_lba, _) = layout.partition_sectors().unwrap();
        let mut device = tempfile::tempfile().unwrap();
        device.set_len(layout.total_bytes_required()).unwrap();
        let mut window = PartitionWindow::new(&mut device, layout.windows_partition);
        fatfs::format_volume(
            &mut window,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"ARGOS-WIN  "),
        )
        .unwrap();

        install_fat32_vbr(&mut window, start_lba).unwrap();

        let mut primary = [0u8; 512];
        window.seek(SeekFrom::Start(0)).unwrap();
        window.read_exact(&mut primary).unwrap();

        let backup_sector = u16::from_le_bytes([primary[0x32], primary[0x33]]);
        assert_eq!(backup_sector, 6, "FAT32 puts its backup boot sector at 6");

        let mut backup = [0u8; 512];
        window
            .seek(SeekFrom::Start(u64::from(backup_sector) * 512))
            .unwrap();
        window.read_exact(&mut backup).unwrap();

        assert_eq!(
            primary, backup,
            "the backup boot sector must be a copy of the primary, boot code and \
             patched hidden-sectors field included"
        );
    }

    #[test]
    fn installing_the_vbr_refuses_a_filesystem_that_is_not_512_byte_sectored() {
        let layout = WindowsMbrPlan::new(600_000_000);
        let mut device = tempfile::tempfile().unwrap();
        device.set_len(layout.total_bytes_required()).unwrap();
        let mut window = PartitionWindow::new(&mut device, layout.windows_partition);
        fatfs::format_volume(
            &mut window,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"ARGOS-WIN  "),
        )
        .unwrap();

        // Claim 4096-byte sectors; the boot code's arithmetic assumes 512.
        window.seek(SeekFrom::Start(0x0B)).unwrap();
        window.write_all(&4096u16.to_le_bytes()).unwrap();

        let err = install_fat32_vbr(&mut window, 2048).unwrap_err();
        assert!(err.to_string().contains("4096"), "got: {err}");
    }

    /// A GPT write must not leave a previous bootloader executable in sector
    /// 0. `gptman` only writes from byte 446 on, so without clearing it, a
    /// stick that once held an isohybrid Linux ISO keeps its bootloader --
    /// and a legacy BIOS runs it, reporting errors about files that are not
    /// on the medium any more. Seen on real hardware as "isolinux.bin
    /// missing or corrupt" from media Argos had since rewritten.
    #[test]
    fn the_gpt_write_clears_a_previous_bootloader_from_sector_zero() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);

        // Stand in for a leftover bootloader: recognisable bytes across the
        // whole bootstrap area.
        device.seek(SeekFrom::Start(0)).unwrap();
        device.write_all(&[0xE9u8; 440]).unwrap();

        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        let mut bootstrap = [0u8; 440];
        device.seek(SeekFrom::Start(0)).unwrap();
        device.read_exact(&mut bootstrap).unwrap();
        assert!(
            bootstrap.iter().all(|&b| b == 0),
            "a previous bootloader survived a GPT write; a legacy BIOS would execute it"
        );

        // The protective MBR itself must still be intact.
        let mut signature = [0u8; 2];
        device.seek(SeekFrom::Start(510)).unwrap();
        device.read_exact(&mut signature).unwrap();
        assert_eq!(signature, [0x55, 0xAA]);
    }

    /// Reads sector 0 back and decodes the MBR fields by hand, from raw
    /// offsets, rather than through `mbrman` -- a table that only round-trips
    /// through the library that wrote it proves nothing about whether a BIOS
    /// will accept it.
    #[test]
    fn the_written_mbr_has_one_active_fat32_partition_where_planned() {
        let layout = WindowsMbrPlan::new(600_000_000);
        let mut device = tempfile::tempfile().unwrap();
        device.set_len(layout.total_bytes_required()).unwrap();
        write_mbr_partition_table(&mut device, &layout).unwrap();

        let mut sector0 = [0u8; 512];
        device.seek(SeekFrom::Start(0)).unwrap();
        device.read_exact(&mut sector0).unwrap();

        // The boot signature a BIOS checks before executing anything.
        assert_eq!(&sector0[510..512], &[0x55, 0xAA]);

        // Partition entry 1 lives at 0x1BE and is 16 bytes.
        let entry = &sector0[0x1BE..0x1BE + 16];
        assert_eq!(entry[0], 0x80, "partition must be marked active/bootable");
        assert_eq!(entry[4], 0x0C, "partition type must be FAT32 (LBA)");

        // CHS must be filled in, not left zero: Windows validates these and
        // refuses to mount a volume whose entry has an all-zero geometry,
        // even though the LBA fields below are what actually get used.
        assert_ne!(
            &entry[1..4],
            &[0u8, 0, 0],
            "first_chs is zeroed; Windows will not mount this volume"
        );
        assert_ne!(
            &entry[5..8],
            &[0u8, 0, 0],
            "last_chs is zeroed; Windows will not mount this volume"
        );

        let lba_start = u32::from_le_bytes(entry[8..12].try_into().unwrap());
        let sector_count = u32::from_le_bytes(entry[12..16].try_into().unwrap());
        let (expected_start, expected_count) = layout.partition_sectors().unwrap();
        assert_eq!(lba_start, expected_start);
        assert_eq!(sector_count, expected_count);

        // Exactly one partition: entries 2-4 must be entirely zero, or a
        // BIOS could find a second "active" entry and boot the wrong thing.
        for i in 1..4 {
            let other = &sector0[0x1BE + i * 16..0x1BE + (i + 1) * 16];
            assert!(
                other.iter().all(|&b| b == 0),
                "entry {} is not empty",
                i + 1
            );
        }
    }

    /// Two writes must not produce the same disk signature: Windows keys its
    /// registry off it, so identical signatures make two Argos-written sticks
    /// collide on the same machine.
    #[test]
    fn each_written_mbr_gets_its_own_disk_signature() {
        let layout = WindowsMbrPlan::new(600_000_000);
        let mut signatures = Vec::new();
        for _ in 0..2 {
            let mut device = tempfile::tempfile().unwrap();
            device.set_len(layout.total_bytes_required()).unwrap();
            write_mbr_partition_table(&mut device, &layout).unwrap();
            let mut sig = [0u8; 4];
            device.seek(SeekFrom::Start(0x1B8)).unwrap();
            device.read_exact(&mut sig).unwrap();
            signatures.push(sig);
        }
        assert_ne!(signatures[0], signatures[1]);
        assert_ne!(
            signatures[0], [0u8; 4],
            "an all-zero signature is not valid"
        );
    }

    /// The bootstrap area is still empty at M6.2 -- boot code is M6.3/M6.4.
    /// Pinned so that when it stops being zero, it is because someone wrote
    /// boot code on purpose.
    #[test]
    fn the_bootstrap_area_is_still_empty_until_m6_3() {
        let layout = WindowsMbrPlan::new(600_000_000);
        let mut device = tempfile::tempfile().unwrap();
        device.set_len(layout.total_bytes_required()).unwrap();
        write_mbr_partition_table(&mut device, &layout).unwrap();

        let mut bootstrap = [0u8; 440];
        device.seek(SeekFrom::Start(0)).unwrap();
        device.read_exact(&mut bootstrap).unwrap();
        assert!(
            bootstrap.iter().all(|&b| b == 0),
            "sector 0 carries boot code, but M6.3 has not landed -- \
             media written now would partition correctly and then not boot"
        );
    }

    /// The MBR path must produce a filesystem the same way the GPT path does:
    /// the partition table is the only difference between them.
    #[test]
    fn an_mbr_partitioned_device_formats_and_populates_like_a_gpt_one() {
        let (iso, files, _guard) = synthetic_iso();
        let actions = plan_copy_actions(&iso, &files).unwrap();
        let layout =
            WindowsMbrPlan::new(actions.iter().map(CopyAction::bytes_on_target).sum::<u64>());
        let mut device = tempfile::tempfile().unwrap();
        device.set_len(layout.total_bytes_required()).unwrap();

        write_mbr_partition_table(&mut device, &layout).unwrap();

        let mut window = PartitionWindow::new(&mut device, layout.windows_partition);
        fatfs::format_volume(
            &mut window,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"ARGOS-WIN  "),
        )
        .unwrap();
        window.seek(SeekFrom::Start(0)).unwrap();
        let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
        let copied =
            copy_files_fat32(&fs, &iso, &actions, &NoopProgress, &CancelToken::new()).unwrap();
        assert_eq!(copied.files_copied, files.len() as u64);
        fs.unmount().unwrap();
    }

    /// The BIOS path end to end at the unit level: MBR scheme, boot code in
    /// the bootstrap area, VBR installed, and `bootmgr` where the boot code
    /// will look. The QEMU test proves it boots; this proves the writer puts
    /// every piece in place without needing an emulator.
    #[test]
    fn the_bios_layout_writes_boot_records_and_a_reachable_bootmgr() {
        let (iso, files, _guard) = synthetic_iso();
        let mut actions = plan_copy_actions(&iso, &files).unwrap();
        // The synthetic ISO has no bootmgr file; add one, since the BIOS
        // path refuses media without it -- which is the point.
        actions.push(CopyAction::Direct {
            path: "bootmgr".into(),
            size: 0,
        });
        let layout =
            TargetLayout::for_layout(WindowsLayout::Fat32Bios, total_bytes_on_target(&actions));
        let mut device = device_file_for(&layout);

        // `bootmgr` must come from somewhere the copy can read; the fixture
        // ISO has `bootmgr` at its root, so the Direct action resolves.
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        let mut sector0 = [0u8; 512];
        device.seek(SeekFrom::Start(0)).unwrap();
        device.read_exact(&mut sector0).unwrap();
        assert_eq!(
            &sector0[..MBR_BOOT_CODE.len()],
            MBR_BOOT_CODE,
            "MBR boot code"
        );
        assert_eq!(sector0[0x1BE], MBR_BOOTABLE_FLAG, "partition marked active");

        let mut vbr = [0u8; 512];
        device
            .seek(SeekFrom::Start(layout.region().start_offset_bytes))
            .unwrap();
        device.read_exact(&mut vbr).unwrap();
        assert_eq!(&vbr[90..], &VBR_FAT32_CODE[90..], "VBR code installed");
        assert_eq!(
            u32::from_le_bytes(vbr[0x1C..0x20].try_into().unwrap()),
            layout.region().start_offset_bytes as u32 / 512,
            "hidden sectors patched"
        );

        // And verify accepts what write produced.
        verify_fat32_media(&mut device, &layout, &iso, &actions, &NoopProgress).unwrap();
    }

    /// The guarantee the VBR's single-cluster search depends on. Media whose
    /// root directory does not carry `bootmgr` must be refused, not written.
    #[test]
    fn the_bios_layout_refuses_media_without_a_reachable_bootmgr() {
        let (iso, files, _guard) = synthetic_iso();
        // No bootmgr in the copy plan at all.
        let actions: Vec<CopyAction> = plan_copy_actions(&iso, &files)
            .unwrap()
            .into_iter()
            .filter(|a| !matches!(a, CopyAction::Direct { path, .. } if path.eq_ignore_ascii_case("bootmgr")))
            .collect();
        let layout =
            TargetLayout::for_layout(WindowsLayout::Fat32Bios, total_bytes_on_target(&actions));
        let mut device = device_file_for(&layout);

        let err = write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .expect_err("media with no bootmgr in the root directory must be refused");
        assert!(
            err.to_string().contains("bootmgr"),
            "the refusal should name what is missing; got: {err}"
        );
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
        let layout = TargetLayout::Gpt(fat32_layout_for(&actions));
        let mut device = device_file_for(&layout);
        write_fat32_media(
            &mut device,
            &layout,
            &iso,
            &actions,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        // Write a synthetic multi-resource WIM into the FAT32 filesystem by
        // splitting it, exactly as split_wim_onto_fat32 does, and read the
        // parts back with our own header parser.
        let window = PartitionWindow::new(&mut device, layout.region());
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
