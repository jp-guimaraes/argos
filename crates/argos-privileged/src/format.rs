//! `argos format`: returns a stick to ordinary use. Zeroes what a previous
//! image left at either end of the device, writes an MBR with one FAT32
//! partition spanning the whole device, and formats it -- the same
//! one-file-descriptor approach as [`crate::windows_fat32`], whose MBR writer
//! and formatter this reuses, so it runs unchanged on Linux and macOS.
//!
//! Seconds, not minutes: nothing here touches the data area beyond what the
//! formatter itself writes, so the old contents are unreachable rather than
//! overwritten. That is what "format" means on every OS this targets; a
//! secure erase is a different operation.

use crate::partition_io::{PartitionWindow, SizedDevice};
use crate::protocol::{validate_refreshed_device_for_format, FormatPlan};
use crate::windows_fat32::{
    format_fat32_volume, record_partition_start, write_single_mbr_partition,
};
use argos_core::error::{ArgosError, Result};
use argos_core::partition::format::{fat_volume_label, whole_device_fat32_region, WIPE_BYTES};
use argos_core::partition::windows::{PartitionRegion, MBR_FAT32_LBA_PARTITION_TYPE, SECTOR_SIZE};
use argos_core::progress::{Phase, ProgressSink};
use argos_platform::PlatformOps;
use std::io::{Read, Seek, SeekFrom, Write};

/// What [`execute_format`] returns on success.
#[derive(Debug)]
pub struct FormatOutcome {
    pub partition_bytes: u64,
    pub label: String,
}

/// Re-validates the device, unmounts it, and lays a fresh whole-device FAT32
/// volume on it. Not cancellable: it takes seconds, and stopping between the
/// table and the filesystem would leave less usable media than either
/// finishing or never starting.
pub fn execute_format(plan: &FormatPlan, progress: &dyn ProgressSink) -> Result<FormatOutcome> {
    let platform = crate::platform_select::current_platform();

    let refreshed = platform.refresh(&plan.device_path, plan.expected_serial.as_deref())?;
    validate_refreshed_device_for_format(plan, refreshed.as_ref())?;
    let device = refreshed.expect(
        "validate_refreshed_device_for_format already returned Ok, so refreshed must be Some",
    );

    // Both re-derived here rather than trusted from the plan: the label must
    // fit the BPB, and the layout is a function of the size the device
    // reports *now*.
    let label = fat_volume_label(&plan.label)
        .ok_or_else(|| ArgosError::InvalidVolumeLabel(plan.label.clone()))?;
    let region = whole_device_fat32_region(device.size_bytes).ok_or_else(|| {
        ArgosError::DeviceSizeUnsupportedForFormat {
            device: plan.device_path.clone(),
            size_bytes: device.size_bytes,
        }
    })?;

    progress.on_phase(Phase::Unmounting);
    platform
        .unmount(&device)
        .map_err(|e| in_step("unmounting", e))?;

    // Exclusive for the same reason as the Windows path: the moment the new
    // FAT32 volume is recognisable, macOS would auto-mount it underneath us.
    let mut device_file = crate::partition_io::open_device_exclusive(&plan.device_path)
        .map_err(|e| in_step("opening the device", ArgosError::Io(e)))?;
    {
        let buffered = crate::partition_io::BufferedDevice::new(&mut device_file)
            .map_err(|e| in_step("opening the device", ArgosError::Io(e)))?;
        let mut sized = SizedDevice::new(buffered, device.size_bytes);
        format_device(&mut sized, region, label, progress)?;
        sized
            .flush()
            .map_err(|e| in_step("flushing", ArgosError::Io(e)))?;
    }
    progress.on_phase(Phase::Flushing);
    crate::partition_io::sync_device(&device_file)
        .map_err(|e| in_step("flushing", ArgosError::Io(e)))?;

    Ok(FormatOutcome {
        partition_bytes: region.size_bytes,
        label: String::from_utf8_lossy(&label).trim_end().to_string(),
    })
}

/// Everything [`execute_format`] does to the device once it is open, over any
/// `Read + Write + Seek` handle -- so the integration tests run it against a
/// plain file with no privilege, the way they do the Windows path.
pub fn format_device<H: Read + Write + Seek>(
    device: &mut H,
    region: PartitionRegion,
    label: [u8; 11],
    progress: &dyn ProgressSink,
) -> Result<()> {
    progress.on_phase(Phase::Wiping);
    wipe_both_ends(device).map_err(|e| in_step("clearing old signatures", e))?;

    progress.on_phase(Phase::Partitioning);
    // Inactive, and no boot code in the bootstrap area: this is a data
    // stick, and a BIOS that finds an active partition or boot code on it
    // would try to boot something that is not there.
    write_single_mbr_partition(device, region, mbrman::BOOT_INACTIVE)
        .map_err(|e| in_step("writing the partition table", e))?;

    progress.on_phase(Phase::FormattingFat32);
    let start_lba = u32::try_from(region.start_offset_bytes / SECTOR_SIZE)
        .expect("whole_device_fat32_region keeps the start within 32 bits");
    {
        let mut window = PartitionWindow::new(&mut *device, region);
        format_fat32_volume(&mut window, region.size_bytes, label)
            .map_err(|e| in_step("formatting", e))?;
        // Windows checks the BPB's hidden-sectors field against where the
        // partition really starts; fatfs, formatting through a window,
        // writes 0. See record_partition_start.
        record_partition_start(&mut window, start_lba).map_err(|e| in_step("formatting", e))?;
    }

    verify_formatted_device(device, region)
}

/// Names the step an I/O error came from. A bare "Operation not permitted
/// (os error 1)" -- what a real stick produced -- could mean the unmount, the
/// open, any write or the final flush, and each has a different cause.
/// Leaves typed, non-I/O errors alone: they already say what went wrong.
fn in_step(step: &str, err: ArgosError) -> ArgosError {
    match err {
        ArgosError::Io(io) => {
            ArgosError::Io(std::io::Error::new(io.kind(), format!("{step}: {io}")))
        }
        other => other,
    }
}

/// Zeroes [`WIPE_BYTES`] at the start and at the end of the device.
///
/// The start holds every signature a previous image could leave where a tool
/// looks first: the old MBR and its boot code, a GPT header and entry array,
/// an ISO9660 volume descriptor at 32 KiB -- which on its own makes `blkid`
/// and udisks report the whole stick as a read-only `iso9660` -- and any
/// boot loader parked before the first partition. The end holds a backup
/// GPT, which some tools will otherwise "restore" over the new MBR.
///
/// A DD-written image's own backup GPT sits at the end of the *image*, not
/// the device, and is left alone: with no primary header and a non-protective
/// MBR in front of it, nothing looks for it there, and it ends up inside the
/// new FAT32 volume's free space.
fn wipe_both_ends<H: Write + Seek>(device: &mut H) -> Result<()> {
    let device_size = device.seek(SeekFrom::End(0)).map_err(ArgosError::Io)?;
    let zeros = vec![0u8; WIPE_BYTES as usize];

    device.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    device.write_all(&zeros).map_err(ArgosError::Io)?;

    let tail = WIPE_BYTES.min(device_size);
    device
        .seek(SeekFrom::Start(device_size - tail))
        .map_err(ArgosError::Io)?;
    device
        .write_all(&zeros[..tail as usize])
        .map_err(ArgosError::Io)?;

    device.flush().map_err(ArgosError::Io)
}

/// Reads the result back the way another OS will meet it: sector 0 as an MBR
/// with one inactive FAT32 entry exactly where `region` says, no GPT behind
/// it, no ISO9660 descriptor left, and a volume that opens as FAT32.
fn verify_formatted_device<H: Read + Write + Seek>(
    device: &mut H,
    region: PartitionRegion,
) -> Result<()> {
    let fail = |what: String| {
        ArgosError::Io(std::io::Error::other(format!(
            "the formatted device does not read back as expected: {what}"
        )))
    };

    let mut sector = [0u8; 512];
    device.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    device.read_exact(&mut sector).map_err(ArgosError::Io)?;
    if sector[510..512] != [0x55, 0xAA] {
        return Err(fail("sector 0 has no boot signature".into()));
    }
    let entry = &sector[0x1BE..0x1BE + 16];
    if entry[0] != mbrman::BOOT_INACTIVE {
        return Err(fail("partition 1 is marked active".into()));
    }
    if entry[4] != MBR_FAT32_LBA_PARTITION_TYPE {
        return Err(fail(format!(
            "partition 1 has type {:#04x}, not FAT32 (LBA)",
            entry[4]
        )));
    }
    let start = u64::from(u32::from_le_bytes(
        entry[8..12].try_into().expect("4 bytes"),
    ));
    let sectors = u64::from(u32::from_le_bytes(
        entry[12..16].try_into().expect("4 bytes"),
    ));
    if start * SECTOR_SIZE != region.start_offset_bytes
        || sectors * SECTOR_SIZE != region.size_bytes
    {
        return Err(fail(format!(
            "partition 1 spans LBA {start}+{sectors}, expected {}+{}",
            region.start_offset_bytes / SECTOR_SIZE,
            region.size_bytes / SECTOR_SIZE
        )));
    }
    if sector[0x1CE..0x1FE].iter().any(|&b| b != 0) {
        return Err(fail("entries 2-4 are not empty".into()));
    }

    let mut probe = [0u8; 8];
    device
        .seek(SeekFrom::Start(SECTOR_SIZE))
        .map_err(ArgosError::Io)?;
    device.read_exact(&mut probe).map_err(ArgosError::Io)?;
    if &probe == b"EFI PART" {
        return Err(fail("a GPT header is still present at LBA 1".into()));
    }
    // ISO9660's primary volume descriptor: type byte, then "CD001", at 32 KiB.
    device
        .seek(SeekFrom::Start(32 * 1024 + 1))
        .map_err(ArgosError::Io)?;
    device.read_exact(&mut probe[..5]).map_err(ArgosError::Io)?;
    if &probe[..5] == b"CD001" {
        return Err(fail("an ISO9660 volume descriptor is still present".into()));
    }

    let mut window = PartitionWindow::new(&mut *device, region);
    window.seek(SeekFrom::Start(0)).map_err(ArgosError::Io)?;
    let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).map_err(|err| {
        fail(format!(
            "the new volume does not open as a FAT filesystem: {err}"
        ))
    })?;
    if fs.fat_type() != fatfs::FatType::Fat32 {
        return Err(fail(format!(
            "the new volume is {:?}, not FAT32",
            fs.fat_type()
        )));
    }
    fs.unmount().map_err(ArgosError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::progress::NoopProgress;
    use std::io::Cursor;

    const MIB: u64 = 1024 * 1024;

    fn label() -> [u8; 11] {
        fat_volume_label("ARGOS").unwrap()
    }

    /// A device that looks like a stick a hybrid ISO was DD'd onto, with a
    /// GPT backup at the device's end for good measure.
    fn dirty_device(size: u64) -> Cursor<Vec<u8>> {
        let mut bytes = vec![0u8; size as usize];
        bytes[..440].fill(0xEB); // boot code
        bytes[510] = 0x55;
        bytes[511] = 0xAA;
        bytes[0x1BE] = 0x80; // an active entry
        bytes[0x1BE + 4] = 0x17; // hidden NTFS, say
        bytes[512..520].copy_from_slice(b"EFI PART");
        bytes[32 * 1024] = 1;
        bytes[32 * 1024 + 1..32 * 1024 + 6].copy_from_slice(b"CD001");
        let tail = size as usize - 512;
        bytes[tail..tail + 8].copy_from_slice(b"EFI PART");
        Cursor::new(bytes)
    }

    #[test]
    fn a_dirty_device_comes_out_as_one_whole_device_fat32_volume() {
        let size = 600 * MIB + 12_345;
        let mut device = dirty_device(size);
        let region = whole_device_fat32_region(size).unwrap();
        format_device(&mut device, region, label(), &NoopProgress).unwrap();

        let bytes = device.get_ref();
        assert!(bytes[..440].iter().all(|&b| b == 0), "boot code survived");
        let tail = size as usize - 512;
        assert_ne!(&bytes[tail..tail + 8], b"EFI PART", "backup GPT survived");

        // The hidden-sectors field records the partition's real start.
        let bpb = region.start_offset_bytes as usize;
        let hidden = u32::from_le_bytes(bytes[bpb + 0x1C..bpb + 0x20].try_into().unwrap());
        assert_eq!(u64::from(hidden), region.start_offset_bytes / SECTOR_SIZE);
        // And the label is in the BPB where every OS reads it from.
        assert_eq!(&bytes[bpb + 0x47..bpb + 0x52], b"ARGOS      ");
    }

    #[test]
    fn the_new_volume_takes_files() {
        let size = 600 * MIB;
        let mut device = Cursor::new(vec![0u8; size as usize]);
        let region = whole_device_fat32_region(size).unwrap();
        format_device(&mut device, region, label(), &NoopProgress).unwrap();

        let window = PartitionWindow::new(&mut device, region);
        let fs = fatfs::FileSystem::new(window, fatfs::FsOptions::new()).unwrap();
        fs.root_dir()
            .create_file("hello.txt")
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        assert!(fs.stats().unwrap().free_clusters() > 0);
        fs.unmount().unwrap();
    }

    #[test]
    fn a_leftover_gpt_header_fails_the_read_back() {
        let size = 600 * MIB;
        let mut device = Cursor::new(vec![0u8; size as usize]);
        let region = whole_device_fat32_region(size).unwrap();
        format_device(&mut device, region, label(), &NoopProgress).unwrap();
        device.get_mut()[512..520].copy_from_slice(b"EFI PART");
        assert!(verify_formatted_device(&mut device, region).is_err());
    }
}
