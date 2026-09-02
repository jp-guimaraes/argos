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
    fn partitions_never_overlap() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        assert!(
            plan.windows_partition.start_offset_bytes >= plan.boot_partition.end_offset_bytes()
        );
    }
}
