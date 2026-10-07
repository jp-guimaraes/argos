//! Lays out what `argos format` puts on a device: one FAT32 partition
//! spanning the whole device, described by an MBR. Pure arithmetic, like the
//! rest of this module; `argos-privileged::format` turns it into bytes.
//!
//! The point of the operation is a stick that ordinary tools treat as an
//! ordinary stick again. Media Argos wrote for Windows carries a partition
//! sized to its contents (see [`super::windows::WindowsMbrPlan::new`]), and a
//! DD-written hybrid ISO carries whatever table the image shipped with, so a
//! file manager's "Format" reformats only that one volume and leaves the rest
//! of the device unreachable. Spanning the device is what fixes that; MBR is
//! what every OS, camera, TV and old BIOS reads.

use super::windows::SECTOR_SIZE;
use super::windows::{align_up, PartitionRegion, ALIGNMENT_BYTES, FAT32_MIN_PARTITION_BYTES};

/// Bytes `argos format` zeroes at each end of the device before writing the
/// new table: the MBR, a GPT's primary header and entry array, an ISO9660
/// volume descriptor set (from 32 KiB) and any boot loader in the gap before
/// the first partition at the start; a GPT's backup copy at the end.
///
/// 1 MiB because that is exactly the gap before a 1 MiB-aligned partition --
/// zeroing it touches nothing the new layout uses -- and comfortably more
/// than the 33 sectors a backup GPT occupies.
pub const WIPE_BYTES: u64 = ALIGNMENT_BYTES;

/// The longest a FAT volume label can be: 11 bytes, the width of the BPB's
/// label field and of a root-directory entry's 8.3 name.
pub const FAT_LABEL_MAX_BYTES: usize = 11;

/// The label `argos format` gives a volume when the user names none.
pub const DEFAULT_FORMAT_LABEL: &str = "ARGOS";

/// The single partition `argos format` lays out on a device of
/// `device_size_bytes`: starting at 1 MiB, ending at the device's last whole
/// sector.
///
/// `None` when the device is outside what this layout can hold: smaller than
/// the start offset plus [`FAT32_MIN_PARTITION_BYTES`], so a forced FAT32
/// volume would fall below its cluster minimum; or past 2 TiB, where an MBR
/// entry's 32-bit sector fields can no longer describe the partition.
pub fn whole_device_fat32_region(device_size_bytes: u64) -> Option<PartitionRegion> {
    let start = align_up(SECTOR_SIZE, ALIGNMENT_BYTES);
    let device_sectors = device_size_bytes / SECTOR_SIZE;
    if u32::try_from(device_sectors).is_err() {
        return None;
    }
    let end = device_sectors * SECTOR_SIZE;
    let size = end.checked_sub(start)?;
    if size < FAT32_MIN_PARTITION_BYTES {
        return None;
    }
    Some(PartitionRegion {
        start_offset_bytes: start,
        size_bytes: size,
    })
}

/// The bytes of a FAT volume label, upper-cased and space-padded to
/// [`FAT_LABEL_MAX_BYTES`], or `None` if `label` cannot be one.
///
/// FAT stores the label as an 8.3-style name, so the rules are a short name's
/// rules: printable ASCII only, none of `" * + , . / : ; < = > ? [ \ ] |`,
/// at most 11 bytes, and not blank. Upper-cased because that is how the
/// field is stored and how Windows shows it; a lower-case label would be
/// read back differently by different systems.
pub fn fat_volume_label(label: &str) -> Option<[u8; FAT_LABEL_MAX_BYTES]> {
    const FORBIDDEN: &[u8] = b"\"*+,./:;<=>?[\\]|";
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > FAT_LABEL_MAX_BYTES || label.trim().is_empty() {
        return None;
    }
    if bytes
        .iter()
        .any(|b| !(0x20..0x7F).contains(b) || FORBIDDEN.contains(b))
    {
        return None;
    }
    let mut out = [b' '; FAT_LABEL_MAX_BYTES];
    for (slot, byte) in out.iter_mut().zip(bytes) {
        *slot = byte.to_ascii_uppercase();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn the_partition_starts_at_one_mib_and_runs_to_the_last_whole_sector() {
        // A real 32 GB stick: not a multiple of a MiB, nor of anything else.
        let size = 31_457_280_000 + 300;
        let region = whole_device_fat32_region(size).unwrap();
        assert_eq!(region.start_offset_bytes, MIB);
        assert_eq!(region.end_offset_bytes(), size / SECTOR_SIZE * SECTOR_SIZE);
        assert_eq!(region.size_bytes % SECTOR_SIZE, 0);
    }

    #[test]
    fn a_device_too_small_for_a_fat32_volume_is_refused() {
        assert!(whole_device_fat32_region(256 * MIB).is_none());
        assert!(whole_device_fat32_region(MIB + FAT32_MIN_PARTITION_BYTES - 1).is_none());
        assert!(whole_device_fat32_region(MIB + FAT32_MIN_PARTITION_BYTES).is_some());
    }

    #[test]
    fn a_device_past_what_an_mbr_can_describe_is_refused() {
        let last_describable = u64::from(u32::MAX) * SECTOR_SIZE;
        assert!(whole_device_fat32_region(last_describable).is_some());
        assert!(whole_device_fat32_region(last_describable + SECTOR_SIZE).is_none());
        assert!(whole_device_fat32_region(4096 * GIB).is_none());
    }

    #[test]
    fn the_wipe_fits_inside_the_gap_before_the_partition() {
        let region = whole_device_fat32_region(8 * GIB).unwrap();
        assert!(WIPE_BYTES <= region.start_offset_bytes);
    }

    #[test]
    fn a_label_is_upper_cased_and_space_padded() {
        assert_eq!(&fat_volume_label("argos").unwrap(), b"ARGOS      ");
        assert_eq!(&fat_volume_label("Lab 3-B_x").unwrap(), b"LAB 3-B_X  ");
        assert_eq!(&fat_volume_label("ELEVENCHARS").unwrap(), b"ELEVENCHARS");
        assert!(fat_volume_label(DEFAULT_FORMAT_LABEL).is_some());
    }

    #[test]
    fn a_label_that_fat_cannot_store_is_refused() {
        for bad in ["", "   ", "TWELVE_CHARS", "a.b", "x/y", "café", "tab\there"] {
            assert!(fat_volume_label(bad).is_none(), "{bad:?} should be refused");
        }
    }
}
