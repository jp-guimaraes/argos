//! Sizes and lays out the two-partition scheme the UEFI:NTFS write path (W3)
//! needs: a protective MBR + GPT, a small FAT32-formatted boot partition
//! (an exact `dd` of the vendored `uefi-ntfs.img`, see
//! `docs/architecture.md`'s phase 2 guiding decisions) and a large NTFS
//! partition holding the extracted Windows files. Everything here is integer
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

/// Extra space reserved on top of the raw byte count of the extracted Windows
/// files when sizing the NTFS partition. NTFS itself has overhead ($MFT
/// reservation, cluster slack across the tens of thousands of small files a
/// Windows image contains, the journal, boot files) that doesn't show up in a
/// simple sum of file sizes. This is a deliberately generous, uncalibrated
/// margin -- W6's real-hardware pass against a real Windows ISO is what
/// should tell us whether it needs adjusting; overshooting it costs a bit of
/// USB stick capacity, undershooting it fails the write outright.
pub const NTFS_OVERHEAD_MARGIN_BYTES: u64 = 100 * 1024 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;

    // A little over 1 MiB, deliberately not sector- or alignment-round, so
    // rounding bugs in either direction would show up in the assertions
    // below rather than being masked by already-aligned inputs.
    const UEFI_NTFS_IMAGE_SIZE: u64 = 1_474_990;
    const WINDOWS_FILES_TOTAL_SIZE: u64 = 5_432_100_000;

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
    fn partitions_never_overlap() {
        let plan = WindowsPartitionPlan::new(UEFI_NTFS_IMAGE_SIZE, WINDOWS_FILES_TOTAL_SIZE);
        assert!(
            plan.windows_partition.start_offset_bytes >= plan.boot_partition.end_offset_bytes()
        );
    }
}
