//! Sizes and lays out the two-partition scheme the UEFI:NTFS write path (W3)
//! needs: a protective MBR + GPT, a small FAT-formatted boot partition (an
//! exact `dd` of the vendored `uefi-ntfs.img` -- a 1.44 MB FAT12 image, see
//! `crates/argos-privileged/assets/PROVENANCE.md` -- not FAT32; small enough
//! that FAT12 is what it naturally formats as) and a large NTFS partition
//! holding the extracted Windows files. Everything here is integer
//! arithmetic over sizes the caller already knows -- no disk, no image, no
//! privilege. W3 turns the resulting [`WindowsPartitionPlan`] into a real GPT
//! (via `gptman`) and real filesystems; this module only decides where each
//! partition starts and how big it needs to be.

/// Every LBA-addressed size in this module is in 512-byte sectors, regardless
/// of the device's real physical sector size -- GPT itself is defined in
/// terms of "logical blocks" this way, and 512 is the universal safe choice
/// (a 4Kn-native device still exposes a 512-byte logical view). All fields on
/// [`WindowsPartitionPlan`] are byte offsets/sizes, already sector-rounded.
pub const SECTOR_SIZE: u64 = 512;

/// Partition starts are aligned to a 1 MiB boundary, the convention modern
/// partitioning tools (Windows Setup, Rufus, `parted`) all follow -- it keeps
/// every partition aligned to any real-world physical/erase-block size
/// (512e, 4Kn, SSD/eMMC erase blocks) without needing to detect one.
pub const ALIGNMENT_BYTES: u64 = 1024 * 1024;

/// One partition entry in the GPT partition array (UEFI spec: 128 bytes,
/// regardless of how much of it a given entry actually uses).
const GPT_PARTITION_ENTRY_SIZE_BYTES: u64 = 128;

/// The number of entries `gptman` (and every other GPT implementation) writes
/// in the partition array, whether or not they're all used -- fixed by the
/// UEFI spec's common convention, not by how many partitions we actually put
/// in it (two).
const GPT_PARTITION_ENTRIES: u64 = 128;

/// Bytes reserved for the *primary* GPT structures at the very start of the
/// device: the protective MBR (LBA 0), the GPT header (LBA 1), and the
/// partition entry array (LBA 2 onward).
const PRIMARY_GPT_OVERHEAD_BYTES: u64 =
    SECTOR_SIZE + SECTOR_SIZE + GPT_PARTITION_ENTRIES * GPT_PARTITION_ENTRY_SIZE_BYTES;

/// Bytes reserved for the *backup* GPT structures at the very end of the
/// device: the partition entry array followed by the backup GPT header (the
/// mirror image of [`PRIMARY_GPT_OVERHEAD_BYTES`], minus the protective MBR,
/// which has no backup copy).
const BACKUP_GPT_OVERHEAD_BYTES: u64 =
    GPT_PARTITION_ENTRIES * GPT_PARTITION_ENTRY_SIZE_BYTES + SECTOR_SIZE;

/// GPT partition type GUID for an EFI System Partition
/// (`C12A7328-F81F-11D2-BA4B-00A0C93EC93B`), used for the boot partition.
/// GPT stores GUIDs in Microsoft's mixed-endian layout (the first three
/// fields little-endian, the last two big-endian, i.e. `Uuid::to_bytes_le`
/// on the canonical string form) -- this is already in that on-disk byte
/// order, ready to hand straight to `gptman::GPTPartitionEntry`.
pub const EFI_SYSTEM_PARTITION_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// GPT partition type GUID for a Microsoft Basic Data Partition
/// (`EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`), used for the NTFS partition --
/// same mixed-endian on-disk byte order as
/// [`EFI_SYSTEM_PARTITION_TYPE_GUID`] above.
pub const MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

/// Extra space reserved on top of the raw byte count of the extracted Windows
/// files when sizing the NTFS partition. NTFS itself has overhead ($MFT
/// reservation, cluster slack across the tens of thousands of small files a
/// Windows image contains, the journal, boot files) that doesn't show up in a
/// simple sum of file sizes. This is a deliberately generous, uncalibrated
/// margin -- W6's real-hardware pass against a real Windows ISO is what
/// should tell us whether it needs adjusting; overshooting it costs a bit of
/// USB stick capacity, undershooting it fails the write outright.
pub const NTFS_OVERHEAD_MARGIN_BYTES: u64 = 100 * 1024 * 1024;

/// Extra space reserved on top of the raw byte count of the Windows files
/// when sizing the FAT32 partition (phase 3 M3, backlog #43): FAT tables
/// (two copies), per-file cluster slack across the tens of thousands of
/// small files a Windows image contains, and the root-directory tree. Same
/// deliberately generous, uncalibrated posture as
/// [`NTFS_OVERHEAD_MARGIN_BYTES`]; M5's real-hardware pass is what should
/// calibrate it.
pub const FAT32_OVERHEAD_MARGIN_BYTES: u64 = 100 * 1024 * 1024;

/// The smallest FAT32 partition the plan will ever lay out. FAT32 requires
/// at least 65525 clusters, so *forcing* FAT32 (rather than letting the
/// formatter fall back to FAT16, which UEFI firmware support for is far
/// less universal) needs a floor: 512 MiB clears the minimum comfortably at
/// every sane cluster size the formatter might pick. Real Windows media is
/// >4 GiB so this floor only ever binds in tests.
pub const FAT32_MIN_PARTITION_BYTES: u64 = 512 * 1024 * 1024;

/// Rounds `value` up to the next multiple of `boundary` (`value` itself if
/// already a multiple).
fn align_up(value: u64, boundary: u64) -> u64 {
    let remainder = value % boundary;
    if remainder == 0 {
        value
    } else {
        value + (boundary - remainder)
    }
}

/// One partition's placement on the device, in bytes from LBA 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionRegion {
    pub start_offset_bytes: u64,
    pub size_bytes: u64,
}

impl PartitionRegion {
    pub fn end_offset_bytes(&self) -> u64 {
        self.start_offset_bytes + self.size_bytes
    }
}

/// The full two-partition layout for a UEFI:NTFS Windows installer write:
/// partition 1 is the FAT32 UEFI:NTFS boot partition (a verbatim `dd` of the
/// vendored image), partition 2 is the NTFS partition holding the Windows
/// installer's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsPartitionPlan {
    pub boot_partition: PartitionRegion,
    pub windows_partition: PartitionRegion,
}

impl WindowsPartitionPlan {
    /// Lays out both partitions from the two sizes that actually vary per
    /// write: `uefi_ntfs_image_size_bytes` (the vendored boot image -- fixed
    /// per Argos release, but not hardcoded here since W3 is what actually
    /// vendors it) and `windows_files_total_size_bytes` (the sum of every
    /// file `image::windows::WindowsIso::list_files` reports for this
    /// particular ISO).
    pub fn new(uefi_ntfs_image_size_bytes: u64, windows_files_total_size_bytes: u64) -> Self {
        let boot_start = align_up(PRIMARY_GPT_OVERHEAD_BYTES, ALIGNMENT_BYTES);
        let boot_size = align_up(uefi_ntfs_image_size_bytes, SECTOR_SIZE);

        let windows_start = align_up(boot_start + boot_size, ALIGNMENT_BYTES);
        let windows_size = align_up(
            windows_files_total_size_bytes + NTFS_OVERHEAD_MARGIN_BYTES,
            SECTOR_SIZE,
        );

        Self {
            boot_partition: PartitionRegion {
                start_offset_bytes: boot_start,
                size_bytes: boot_size,
            },
            windows_partition: PartitionRegion {
                start_offset_bytes: windows_start,
                size_bytes: windows_size,
            },
        }
    }

    /// The smallest device size (in bytes) this plan fits on: both partitions
    /// plus the backup GPT structures that must follow the last one. This --
    /// not the raw ISO size -- is what the capacity preflight check
    /// (`preflight::check_windows_capacity`) compares a candidate device
    /// against.
    pub fn total_bytes_required(&self) -> u64 {
        align_up(
            self.windows_partition.end_offset_bytes() + BACKUP_GPT_OVERHEAD_BYTES,
            SECTOR_SIZE,
        )
    }
}

/// MBR partition type for "FAT32 with LBA addressing" (`0x0C`), the type
/// Windows Setup and Rufus both use for FAT32 install media. The non-LBA
/// FAT32 type (`0x0B`) exists for CHS-addressed disks that no USB stick this
/// tool targets has been for decades; using it would only limit addressing.
pub const MBR_FAT32_LBA_PARTITION_TYPE: u8 = 0x0C;

/// The MBR partition entry's "active"/bootable status byte. A legacy BIOS's
/// MBR boot code scans the four entries for exactly this value to decide
/// which partition's boot sector to load, so the FAT32 partition must carry
/// it or nothing boots -- there is no other signal.
pub const MBR_BOOTABLE_FLAG: u8 = 0x80;

/// Bytes at the very start of the device reserved for the MBR itself: one
/// 512-byte sector holding boot code (0..=445), the four 16-byte partition
/// entries (446..=509) and the `0x55AA` signature (510..=511).
///
/// Far smaller than [`PRIMARY_GPT_OVERHEAD_BYTES`], but the partition start
/// is 1 MiB-aligned regardless, so in practice this changes nothing about
/// where data lands -- it only means MBR media has no *trailing* reserved
/// region, unlike GPT's backup header.
const MBR_OVERHEAD_BYTES: u64 = SECTOR_SIZE;

/// FAT32's defining minimum: a volume with fewer clusters than this is a
/// FAT16 volume by definition, whatever its boot sector claims.
pub const FAT32_MIN_CLUSTERS: u64 = 65_525;

/// The largest cluster size worth using. Bigger clusters mean fewer
/// clusters, and the cluster count is what drives the write cost: one write
/// per cluster of file data, plus a FAT entry update per cluster in each
/// FAT. 32 KiB is where Windows itself tops out for FAT32, so it is the
/// well-travelled value rather than a clever one.
pub const FAT32_MAX_BYTES_PER_CLUSTER: u32 = 32 * 1024;

/// Chooses the cluster size to format a FAT32 volume of `partition_bytes`
/// with: the largest power of two up to [`FAT32_MAX_BYTES_PER_CLUSTER`]
/// that still leaves comfortably more than [`FAT32_MIN_CLUSTERS`] clusters.
///
/// Both ends of that range matter. Too small and the write becomes millions
/// of tiny operations -- profiling a 6.1 GB write at 4 KiB clusters found
/// 7.7 million writes, 80% of them a single sector. Too large and the volume
/// drops below FAT32's cluster minimum and is no longer a valid FAT32
/// filesystem at all, which a fixed 32 KiB does to any volume under ~2 GiB.
///
/// The cost of larger clusters is slack -- up to one cluster wasted per
/// file. Windows install media is ~900 files, mostly large, so at 32 KiB
/// that is about 29 MB against 6 GB: irrelevant next to the time saved.
pub fn fat32_bytes_per_cluster_for(partition_bytes: u64) -> u32 {
    // 2x headroom over the bare minimum: the usable area is smaller than the
    // partition (reserved sectors, two FATs), so sizing against the raw
    // partition size would sail too close to the limit.
    let required_clusters = FAT32_MIN_CLUSTERS * 2;
    let mut bytes_per_cluster = FAT32_MAX_BYTES_PER_CLUSTER;
    while bytes_per_cluster > 512 {
        if partition_bytes / u64::from(bytes_per_cluster) >= required_clusters {
            break;
        }
        bytes_per_cluster /= 2;
    }
    bytes_per_cluster
}

/// The BIOS translation geometry every partitioning tool assumes when it
/// fills in an MBR entry's CHS fields: 255 heads, 63 sectors per track.
/// Nothing physical has looked like this in decades -- it is a convention,
/// and being *the* convention is exactly why it matters.
pub const CHS_HEADS: u32 = 255;
pub const CHS_SECTORS_PER_TRACK: u32 = 63;

/// The value written when an address is past what CHS can express
/// (cylinder 1023, head 254, sector 63): the standard "use the LBA fields
/// instead" marker.
pub const CHS_MAX: (u16, u8, u8) = (1023, 254, 63);

/// Converts an LBA to the `(cylinder, head, sector)` triple an MBR
/// partition entry stores, saturating at [`CHS_MAX`].
///
/// Why bother, when everything that reads this media addresses it by LBA:
/// **because Windows validates these fields.** Leaving them zero produces a
/// partition table that a BIOS boots and that Linux and macOS mount happily,
/// while Windows declines to mount the volume -- which surfaces as install
/// media that starts Setup and then reports that a media driver is missing,
/// with nothing to connect the message to its cause.
pub fn chs_for_lba(lba: u32) -> (u16, u8, u8) {
    let sectors_per_cylinder = CHS_HEADS * CHS_SECTORS_PER_TRACK;
    let cylinder = lba / sectors_per_cylinder;
    if cylinder > u32::from(CHS_MAX.0) {
        return CHS_MAX;
    }
    let within_cylinder = lba % sectors_per_cylinder;
    let head = within_cylinder / CHS_SECTORS_PER_TRACK;
    // Sectors are numbered from 1, not 0 -- the one off-by-one this format
    // is famous for.
    let sector = within_cylinder % CHS_SECTORS_PER_TRACK + 1;
    (cylinder as u16, head as u8, sector as u8)
}

/// The single-partition MBR layout for a BIOS-bootable FAT32 Windows
/// installer write (phase 3 M6.2, backlog #45).
///
/// Same shape as [`WindowsFat32Plan`] -- one FAT32 partition holding the
/// whole installer file tree -- differing only in the partition table that
/// describes it, and in two consequences of that:
///
/// - **No backup structure at the end of the device.** GPT mirrors its
///   header and entry array at the tail; MBR has nothing there, so a device
///   of a given size fits a slightly larger partition.
/// - **The partition is marked active** ([`MBR_BOOTABLE_FLAG`]), and sector
///   0 carries boot code. A legacy BIOS loads that code, which finds the
///   active partition and chain-loads its boot sector; neither step has any
///   equivalent in the UEFI path, where firmware reads the filesystem
///   itself. Writing those two boot records is M6.3/M6.4 -- this type only
///   decides geometry.
///
/// Why MBR rather than GPT for BIOS: some legacy BIOSes will boot a GPT disk
/// through its protective MBR, but many will not, and the failure is silent.
/// MBR is what every BIOS understands, and Windows 10 -- the only Windows
/// that runs on such machines at all, since 11 requires UEFI -- installs from
/// it happily.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsMbrPlan {
    pub windows_partition: PartitionRegion,
}

impl WindowsMbrPlan {
    /// Lays out the single FAT32 partition from the total size the installer
    /// files will occupy on the target -- the same input
    /// [`WindowsFat32Plan::new`] takes, with `install.wim` already counted at
    /// its split `.swm` size where the splitter applies.
    pub fn new(windows_files_total_size_bytes: u64) -> Self {
        let start = align_up(MBR_OVERHEAD_BYTES, ALIGNMENT_BYTES);
        let size = align_up(
            (windows_files_total_size_bytes + FAT32_OVERHEAD_MARGIN_BYTES)
                .max(FAT32_MIN_PARTITION_BYTES),
            SECTOR_SIZE,
        );
        Self {
            windows_partition: PartitionRegion {
                start_offset_bytes: start,
                size_bytes: size,
            },
        }
    }

    /// The smallest device this plan fits on. Unlike GPT's, this is just the
    /// end of the partition: MBR reserves nothing at the tail.
    pub fn total_bytes_required(&self) -> u64 {
        align_up(self.windows_partition.end_offset_bytes(), SECTOR_SIZE)
    }

    /// The partition's placement in 512-byte sectors, which is the unit MBR
    /// partition entries store (`lba_start` and `sectors`, both `u32`).
    ///
    /// Returns `None` if either value overflows 32 bits -- an MBR simply
    /// cannot describe a partition starting or extending beyond 2 TiB, and
    /// silently truncating would produce a table pointing at the wrong place.
    /// Not reachable with any USB stick this tool targets, but the check is
    /// what makes that a fact rather than an assumption.
    pub fn partition_sectors(&self) -> Option<(u32, u32)> {
        let start = self.windows_partition.start_offset_bytes / SECTOR_SIZE;
        let count = self.windows_partition.size_bytes / SECTOR_SIZE;
        match (u32::try_from(start), u32::try_from(count)) {
            (Ok(start), Ok(count)) => Some((start, count)),
            _ => None,
        }
    }
}

/// The single-partition layout for a FAT32 Windows installer write (phase 3
/// M3, backlog #43): one Microsoft Basic Data partition holding the whole
/// Windows installer file tree on FAT32, booted directly by the firmware via
/// the ISO's own `efi/boot/bootx64.efi` -- no boot partition, no vendored
/// driver image.
///
/// Type-GUID decision (M3.2, recorded): **Microsoft Basic Data, not EFI
/// System Partition.** UEFI firmware boots removable media by scanning for
/// `\efi\boot\bootx64.efi` on any FAT filesystem it can read -- the ESP type
/// GUID is only load-bearing for *fixed* system disks. Rufus marks its FAT32
/// Windows media basic-data and that boots everywhere it was ever tested;
/// matching it keeps Argos on the most-travelled firmware path, and keeps
/// Windows itself (which hides ESPs from Explorer) treating the stick as a
/// normal data drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsFat32Plan {
    pub windows_partition: PartitionRegion,
}

impl WindowsFat32Plan {
    /// Lays out the single FAT32 partition from the one size that varies per
    /// write: the sum of every file `image::windows::WindowsIso::list_files`
    /// reports for this particular ISO (with `install.wim` already counted at
    /// its split size once M2's splitter is in the pipeline -- the parts sum
    /// to at most a header per part more than the original).
    pub fn new(windows_files_total_size_bytes: u64) -> Self {
        let start = align_up(PRIMARY_GPT_OVERHEAD_BYTES, ALIGNMENT_BYTES);
        let size = align_up(
            (windows_files_total_size_bytes + FAT32_OVERHEAD_MARGIN_BYTES)
                .max(FAT32_MIN_PARTITION_BYTES),
            SECTOR_SIZE,
        );
        Self {
            windows_partition: PartitionRegion {
                start_offset_bytes: start,
                size_bytes: size,
            },
        }
    }

    /// The smallest device size (in bytes) this plan fits on -- the partition
    /// plus the backup GPT structures that must follow it. Same contract as
    /// [`WindowsPartitionPlan::total_bytes_required`].
    pub fn total_bytes_required(&self) -> u64 {
        align_up(
            self.windows_partition.end_offset_bytes() + BACKUP_GPT_OVERHEAD_BYTES,
            SECTOR_SIZE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A little over 1 MiB, deliberately not sector- or alignment-round, so
    // rounding bugs in either direction would show up in the assertions
    // below rather than being masked by already-aligned inputs.
    const UEFI_NTFS_IMAGE_SIZE: u64 = 1_474_990;
    const WINDOWS_FILES_TOTAL_SIZE: u64 = 5_432_100_000;

    /// Renders a mixed-endian on-disk GUID back to its canonical dashed
    /// string form, so the type GUID constants below can be checked against
    /// the well-known strings from the UEFI/GPT spec instead of trusting the
    /// hand-transcribed byte arrays by eye.
    fn mixed_endian_guid_to_string(bytes: [u8; 16]) -> String {
        format!(
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            bytes[3], bytes[2], bytes[1], bytes[0],
            bytes[5], bytes[4],
            bytes[7], bytes[6],
            bytes[8], bytes[9],
            bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        )
    }

    #[test]
    fn efi_system_partition_type_guid_matches_the_uefi_spec_string() {
        assert_eq!(
            mixed_endian_guid_to_string(EFI_SYSTEM_PARTITION_TYPE_GUID),
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
    }

    #[test]
    fn microsoft_basic_data_partition_type_guid_matches_the_uefi_spec_string() {
        assert_eq!(
            mixed_endian_guid_to_string(MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
    }

    #[test]
    fn boot_partition_starts_at_the_first_1mib_aligned_lba_after_the_primary_gpt() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        assert_eq!(plan.boot_partition.start_offset_bytes, ALIGNMENT_BYTES);
        assert_eq!(plan.boot_partition.start_offset_bytes % ALIGNMENT_BYTES, 0);
    }

    #[test]
    fn boot_partition_size_is_the_image_size_rounded_up_to_a_whole_sector() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        assert_eq!(plan.boot_partition.size_bytes % SECTOR_SIZE, 0);
        assert!(plan.boot_partition.size_bytes >= UEFI_NTFS_IMAGE_SIZE);
        // Rounded up by less than one whole sector.
        assert!(plan.boot_partition.size_bytes - UEFI_NTFS_IMAGE_SIZE < SECTOR_SIZE);
    }

    #[test]
    fn boot_partition_size_is_unchanged_when_already_sector_aligned() {
        let plan = WindowsPartitionPlan::new(2 * 1024 * 1024, WINDOWS_FILES_TOTAL_SIZE);
        assert_eq!(plan.boot_partition.size_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn windows_partition_starts_at_the_first_1mib_aligned_lba_after_the_boot_partition() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        let boot_end = plan.boot_partition.end_offset_bytes();
        let windows_start = plan.windows_partition.start_offset_bytes;

        assert_eq!(windows_start % ALIGNMENT_BYTES, 0);
        assert!(windows_start >= boot_end);
        // It's the *smallest* aligned offset that clears the boot partition,
        // not just some aligned offset further out.
        assert!(windows_start - boot_end < ALIGNMENT_BYTES);
    }

    #[test]
    fn windows_partition_size_includes_the_overhead_margin_and_is_sector_rounded() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        assert!(
            plan.windows_partition.size_bytes
                >= WINDOWS_FILES_TOTAL_SIZE + NTFS_OVERHEAD_MARGIN_BYTES
        );
        assert_eq!(plan.windows_partition.size_bytes % SECTOR_SIZE, 0);
    }

    #[test]
    fn total_bytes_required_covers_both_partitions_plus_the_backup_gpt() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        let required = plan.total_bytes_required();
        assert!(required > plan.windows_partition.end_offset_bytes());
        assert!(required - plan.windows_partition.end_offset_bytes() >= BACKUP_GPT_OVERHEAD_BYTES);
        assert_eq!(required % SECTOR_SIZE, 0);
    }

    #[test]
    fn larger_windows_file_trees_require_a_larger_device() {
        let small = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, 1_000_000_000);
        let large = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, 10_000_000_000);
        assert!(large.total_bytes_required() > small.total_bytes_required());
    }

    #[test]
    fn fat32_partition_starts_at_the_first_1mib_aligned_lba_after_the_primary_gpt() {
        let plan = WindowsFat32Plan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert_eq!(plan.windows_partition.start_offset_bytes, ALIGNMENT_BYTES);
    }

    #[test]
    fn fat32_partition_size_includes_the_overhead_margin_and_is_sector_rounded() {
        let plan = WindowsFat32Plan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert!(
            plan.windows_partition.size_bytes
                >= WINDOWS_FILES_TOTAL_SIZE + FAT32_OVERHEAD_MARGIN_BYTES
        );
        assert_eq!(plan.windows_partition.size_bytes % SECTOR_SIZE, 0);
    }

    #[test]
    fn fat32_partition_never_shrinks_below_the_forced_fat32_floor() {
        // A tiny synthetic ISO (the integration-test case) must still get a
        // partition big enough to *force* FAT32 formatting on.
        let plan = WindowsFat32Plan::new(1_000_000);
        assert_eq!(plan.windows_partition.size_bytes, FAT32_MIN_PARTITION_BYTES);
    }

    #[test]
    fn fat32_total_bytes_required_covers_the_partition_plus_the_backup_gpt() {
        let plan = WindowsFat32Plan::new(WINDOWS_FILES_TOTAL_SIZE);
        let required = plan.total_bytes_required();
        assert!(required - plan.windows_partition.end_offset_bytes() >= BACKUP_GPT_OVERHEAD_BYTES);
        assert_eq!(required % SECTOR_SIZE, 0);
    }

    #[test]
    fn mbr_partition_starts_at_the_first_1mib_aligned_lba() {
        let plan = WindowsMbrPlan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert_eq!(plan.windows_partition.start_offset_bytes, ALIGNMENT_BYTES);
    }

    /// The MBR occupies only sector 0, so the partition must not start at
    /// sector 1 -- alignment, not the table's size, is what places it.
    #[test]
    fn mbr_partition_clears_sector_zero_by_a_full_alignment_unit() {
        let plan = WindowsMbrPlan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert!(plan.windows_partition.start_offset_bytes >= MBR_OVERHEAD_BYTES);
        assert_eq!(
            plan.windows_partition.start_offset_bytes % ALIGNMENT_BYTES,
            0
        );
    }

    #[test]
    fn mbr_partition_size_includes_the_overhead_margin_and_is_sector_rounded() {
        let plan = WindowsMbrPlan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert!(
            plan.windows_partition.size_bytes
                >= WINDOWS_FILES_TOTAL_SIZE + FAT32_OVERHEAD_MARGIN_BYTES
        );
        assert_eq!(plan.windows_partition.size_bytes % SECTOR_SIZE, 0);
    }

    #[test]
    fn mbr_partition_never_shrinks_below_the_forced_fat32_floor() {
        let plan = WindowsMbrPlan::new(1_000_000);
        assert_eq!(plan.windows_partition.size_bytes, FAT32_MIN_PARTITION_BYTES);
    }

    /// MBR reserves nothing at the tail, unlike GPT's mirrored header --
    /// so for identical inputs it needs strictly less device than the GPT
    /// plan, and the difference is exactly the backup GPT.
    #[test]
    fn mbr_needs_less_device_than_gpt_for_the_same_files() {
        let mbr = WindowsMbrPlan::new(WINDOWS_FILES_TOTAL_SIZE);
        let gpt = WindowsFat32Plan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert!(mbr.total_bytes_required() < gpt.total_bytes_required());
        assert_eq!(
            gpt.total_bytes_required() - mbr.total_bytes_required(),
            BACKUP_GPT_OVERHEAD_BYTES
        );
    }

    #[test]
    fn mbr_total_bytes_required_is_exactly_the_partition_end() {
        let plan = WindowsMbrPlan::new(WINDOWS_FILES_TOTAL_SIZE);
        assert_eq!(
            plan.total_bytes_required(),
            plan.windows_partition.end_offset_bytes()
        );
    }

    #[test]
    fn partition_sectors_reports_the_lba_start_and_count_an_mbr_entry_stores() {
        let plan = WindowsMbrPlan::new(WINDOWS_FILES_TOTAL_SIZE);
        let (start, count) = plan.partition_sectors().expect("a normal plan fits u32");
        assert_eq!(
            u64::from(start) * SECTOR_SIZE,
            plan.windows_partition.start_offset_bytes
        );
        assert_eq!(
            u64::from(count) * SECTOR_SIZE,
            plan.windows_partition.size_bytes
        );
    }

    /// An MBR entry stores LBAs in 32 bits, so it cannot describe a partition
    /// past 2 TiB. Refusing beats silently truncating into a table that
    /// points somewhere else entirely.
    #[test]
    fn partition_sectors_refuses_a_partition_beyond_what_an_mbr_entry_can_hold() {
        // Over 2 TiB of files: the sector count alone overflows u32.
        let plan = WindowsMbrPlan::new(3 * 1024 * 1024 * 1024 * 1024);
        assert!(plan.partition_sectors().is_none());
    }

    #[test]
    fn cluster_size_grows_with_the_volume_and_caps_where_windows_caps() {
        // A real Windows-media partition: big enough for the maximum.
        assert_eq!(
            fat32_bytes_per_cluster_for(6 * 1024 * 1024 * 1024),
            FAT32_MAX_BYTES_PER_CLUSTER
        );
        // The plan's 512 MiB floor cannot afford 32 KiB clusters.
        assert!(
            fat32_bytes_per_cluster_for(FAT32_MIN_PARTITION_BYTES) < FAT32_MAX_BYTES_PER_CLUSTER
        );
    }

    /// The property that actually matters: whatever size is chosen, the
    /// volume must still have enough clusters to *be* FAT32. A fixed 32 KiB
    /// fails this for every volume under about 2 GiB.
    #[test]
    fn the_chosen_cluster_size_always_leaves_a_valid_fat32_volume() {
        let sizes = [
            FAT32_MIN_PARTITION_BYTES,
            600 * 1024 * 1024,
            1024 * 1024 * 1024,
            2 * 1024 * 1024 * 1024,
            6 * 1024 * 1024 * 1024,
            32 * 1024 * 1024 * 1024,
        ];
        for size in sizes {
            let bytes_per_cluster = fat32_bytes_per_cluster_for(size);
            assert!(
                bytes_per_cluster.is_power_of_two() && bytes_per_cluster >= 512,
                "{bytes_per_cluster} is not a usable cluster size"
            );
            let clusters = size / u64::from(bytes_per_cluster);
            assert!(
                clusters >= FAT32_MIN_CLUSTERS,
                "a {size}-byte volume at {bytes_per_cluster} bytes/cluster has only {clusters} \
                 clusters, below FAT32's {FAT32_MIN_CLUSTERS} minimum"
            );
        }
    }

    #[test]
    fn chs_conversion_matches_the_classic_255x63_geometry() {
        // LBA 0 is cylinder 0, head 0, sector 1 -- sectors count from one.
        assert_eq!(chs_for_lba(0), (0, 0, 1));
        // The 1 MiB partition start every plan here uses.
        assert_eq!(chs_for_lba(2048), (0, 32, 33));
        // Last address of the first cylinder, then the first of the second.
        assert_eq!(chs_for_lba(255 * 63 - 1), (0, 254, 63));
        assert_eq!(chs_for_lba(255 * 63), (1, 0, 1));
    }

    #[test]
    fn chs_saturates_instead_of_wrapping_past_what_it_can_express() {
        let last_expressible = 1024 * 255 * 63 - 1;
        assert_eq!(chs_for_lba(last_expressible), (1023, 254, 63));
        // One past it, and anything beyond, must clamp rather than wrap --
        // a wrapped value would point the entry at the wrong place entirely.
        assert_eq!(chs_for_lba(last_expressible + 1), CHS_MAX);
        assert_eq!(chs_for_lba(u32::MAX), CHS_MAX);
    }

    /// Every field of a CHS triple has to fit its byte; a conversion that
    /// produced head 255 or sector 64 would silently corrupt the entry.
    #[test]
    fn chs_fields_always_fit_their_on_disk_widths() {
        for lba in [
            0u32,
            1,
            63,
            2048,
            100_000,
            16_064,
            16_065,
            1 << 20,
            u32::MAX,
        ] {
            let (c, h, s) = chs_for_lba(lba);
            assert!(c <= 1023, "cylinder {c} for lba {lba}");
            assert!(h <= 254, "head {h} for lba {lba}");
            assert!((1..=63).contains(&s), "sector {s} for lba {lba}");
        }
    }

    #[test]
    fn mbr_partition_type_and_bootable_flag_match_the_documented_values() {
        // 0x0C = FAT32 (LBA); 0x80 = active. Spelled out here so a typo in
        // the constants is caught by the value, not by a dead lab machine.
        assert_eq!(MBR_FAT32_LBA_PARTITION_TYPE, 0x0C);
        assert_eq!(MBR_BOOTABLE_FLAG, 0x80);
    }

    #[test]
    fn partitions_never_overlap() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        assert!(
            plan.windows_partition.start_offset_bytes >= plan.boot_partition.end_offset_bytes()
        );
    }
}
