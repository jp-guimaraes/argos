//! Classifies an ISO image without needing to know anything about disks yet.
//! This is the decision point for the whole write pipeline: a `Hybrid` ISO can be
//! written byte-for-byte in DD mode (the distro already embedded MBR/GPT and the
//! bootloaders); anything else is refused in v1 rather than half-supported.
//!
//! Detection is done from the first couple of ISO9660 sectors only, so
//! classifying a multi-GB image costs a handful of small reads, never a full scan.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsoKind {
    /// Has a valid embedded MBR (with at least one real partition entry) *and* the
    /// `0x55AA` boot signature. This is what `xorriso -isohybrid-mbr` / genisoimage
    /// produce for essentially every mainstream Linux distro today.
    Hybrid,
    /// Has an El Torito boot catalog (BIOS-bootable) but no valid embedded MBR --
    /// older or hand-rolled images. Not safe to DD onto a raw disk.
    ElToritoOnly,
    /// Neither signature found; likely a data-only ISO never meant to be booted
    /// directly from a raw disk.
    PlainData,
}

#[derive(Debug, Clone, Copy)]
pub struct IsoClassification {
    pub kind: IsoKind,
    /// Best-effort hint only (checked via the LBA1 `"EFI PART"` GPT signature that
    /// hybrid GPT/MBR images carry). A `false` here does not prove UEFI is
    /// unsupported -- it only means Argos could not confirm it cheaply.
    pub likely_uefi_capable: bool,
}

impl IsoClassification {
    /// The only question v1's write pipeline actually asks.
    pub fn is_writable_as_dd_image(&self) -> bool {
        self.kind == IsoKind::Hybrid
    }
}

const SECTOR_512: usize = 512;
const EL_TORITO_DESCRIPTOR_OFFSET: u64 = 17 * 2048; // ISO9660 sector 17, 2048-byte sectors

pub fn classify(path: &Path) -> io::Result<IsoClassification> {
    let mut file = File::open(path)?;
    classify_reader(&mut file)
}

pub fn classify_reader<R: Read + Seek>(reader: &mut R) -> io::Result<IsoClassification> {
    reader.seek(SeekFrom::Start(0))?;
    let mut first_1k = [0u8; SECTOR_512 * 2];
    read_best_effort(reader, &mut first_1k)?;

    let sector0 = &first_1k[..SECTOR_512];
    let sector1 = &first_1k[SECTOR_512..];

    let has_mbr_signature = sector0[510..512] == [0x55, 0xAA];
    let has_nonzero_partition_entry = mbr_partition_entries(sector0)
        .iter()
        .any(|entry| entry[4] != 0);
    let is_hybrid = has_mbr_signature && has_nonzero_partition_entry;

    let likely_uefi_capable = &sector1[0..8] == b"EFI PART";

    let kind = if is_hybrid {
        IsoKind::Hybrid
    } else if has_el_torito_descriptor(reader)? {
        IsoKind::ElToritoOnly
    } else {
        IsoKind::PlainData
    };

    Ok(IsoClassification {
        kind,
        likely_uefi_capable,
    })
}

/// The four 16-byte partition entries in a legacy MBR, at their fixed offsets.
fn mbr_partition_entries(sector0: &[u8]) -> [&[u8]; 4] {
    [
        &sector0[446..462],
        &sector0[462..478],
        &sector0[478..494],
        &sector0[494..510],
    ]
}

fn has_el_torito_descriptor<R: Read + Seek>(reader: &mut R) -> io::Result<bool> {
    reader.seek(SeekFrom::Start(EL_TORITO_DESCRIPTOR_OFFSET))?;
    let mut descriptor = [0u8; 32];
    if read_best_effort(reader, &mut descriptor).is_err() {
        // Image shorter than the El Torito descriptor offset: definitely not El Torito.
        return Ok(false);
    }

    let volume_descriptor_type = descriptor[0];
    let standard_identifier = &descriptor[1..6];
    let boot_system_identifier = &descriptor[7..30];

    Ok(volume_descriptor_type == 0
        && standard_identifier == b"CD001"
        && boot_system_identifier.starts_with(b"EL TORITO SPECIFICATION"))
}

/// Reads into `buf`, tolerating a source shorter than `buf` by zero-filling the
/// remainder -- classification fixtures and tiny test images may be smaller than
/// the offsets we probe.
fn read_best_effort<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hybrid_iso_header() -> Vec<u8> {
        let mut buf = vec![0u8; 3 * 2048];
        // Boot signature.
        buf[510] = 0x55;
        buf[511] = 0xAA;
        // A single non-zero partition entry (type 0x0C = FAT32 LBA), just enough
        // to look like a real embedded MBR.
        buf[446 + 4] = 0x0C;
        // GPT protective/hybrid header signature at LBA1.
        buf[512..520].copy_from_slice(b"EFI PART");
        buf
    }

    fn el_torito_only_header() -> Vec<u8> {
        let mut buf = vec![0u8; 18 * 2048];
        let offset = 17 * 2048;
        buf[offset] = 0; // Boot Record volume descriptor type
        buf[offset + 1..offset + 6].copy_from_slice(b"CD001");
        buf[offset + 6] = 1;
        buf[offset + 7..offset + 7 + 23].copy_from_slice(b"EL TORITO SPECIFICATION");
        buf
    }

    #[test]
    fn classifies_hybrid_iso_and_detects_uefi_hint() {
        let mut cursor = Cursor::new(hybrid_iso_header());
        let result = classify_reader(&mut cursor).unwrap();
        assert_eq!(result.kind, IsoKind::Hybrid);
        assert!(result.likely_uefi_capable);
        assert!(result.is_writable_as_dd_image());
    }

    #[test]
    fn classifies_el_torito_only_iso_as_not_writable() {
        let mut cursor = Cursor::new(el_torito_only_header());
        let result = classify_reader(&mut cursor).unwrap();
        assert_eq!(result.kind, IsoKind::ElToritoOnly);
        assert!(!result.is_writable_as_dd_image());
    }

    #[test]
    fn classifies_empty_input_as_plain_data() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = classify_reader(&mut cursor).unwrap();
        assert_eq!(result.kind, IsoKind::PlainData);
        assert!(!result.is_writable_as_dd_image());
    }

    #[test]
    fn boot_signature_alone_without_partition_entry_is_not_hybrid() {
        // Plenty of non-bootable ISOs still end their first sector with 0x55AA by
        // coincidence of padding; a hybrid needs a real partition entry too.
        let mut buf = vec![0u8; 2048];
        buf[510] = 0x55;
        buf[511] = 0xAA;
        let mut cursor = Cursor::new(buf);
        let result = classify_reader(&mut cursor).unwrap();
        assert_eq!(result.kind, IsoKind::PlainData);
    }
}
