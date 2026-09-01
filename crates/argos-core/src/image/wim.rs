//! Argos's own minimal WIM (Windows Imaging) format reader and splitter
//! (phase 3 M2, backlog #42).
//!
//! Purpose-built for exactly one job: splitting an `install.wim` into
//! `.swm` parts that each fit under FAT32's 4GiB-1 file limit, so the FAT32
//! Windows layout (M3, #43) can carry official install media. To do that it
//! needs to *parse* the container -- header, resource lookup table, XML data
//! block -- and *redistribute whole stored resources* across parts. It never
//! decompresses, recompresses, or re-encodes anything: every resource's
//! bytes (chunk tables and all) are copied verbatim, so the per-resource
//! SHA-1s in the lookup table stay valid by construction, and no XPRESS/LZX
//! codec is needed at all.
//!
//! Format references: Microsoft's published "Windows Imaging File Format
//! (WIM)" paper (October 2007) -- the authority for every struct layout and
//! for what a split (spanned) WIM must look like: *"the first part will
//! always contain a copy of the WIM header and all metadata resources.
//! Subsequent .swm files will contain the remaining resources (file
//! resources, lookup table, xml data, and integrity table)"* -- plus
//! wimlib's documentation, read for understanding only (wimlib is GPL; no
//! code was copied, and it is not linked).
//!
//! Deliberate scope limits:
//! - **Read + split only.** Argos copies media; it does not build or edit
//!   images (an explicit phase-3 non-goal).
//! - **Solid/ESD images are refused**, not mishandled: `install.esd` uses
//!   the solid-resource WIM variant (header version 0xE00, LZMS solid
//!   blocks), where multiple files share one compressed block --
//!   resource-boundary splitting cannot work there, so
//!   [`WimImage::open`] fails with a clear message instead (the M2.3
//!   requirement).
//! - **No integrity table is emitted**: it's optional per the spec (only
//!   created by `imagex /check`), and each part's lookup-table SHA-1s
//!   already cover the data. The source's integrity table, if any, is
//!   dropped rather than recomputed.

use std::io::{self, Read, Seek, SeekFrom, Write};

/// The WIM header (`_WIMHEADER_V1_PACKED`) is exactly 208 bytes.
pub const HEADER_SIZE: usize = 208;

/// `ImageTag`: "MSWIM\0\0\0".
const SIGNATURE: [u8; 8] = *b"MSWIM\0\0\0";

/// `dwVersion` of every non-solid WIM this module accepts (the value all
/// `install.wim` files use).
pub const WIM_VERSION: u32 = 0x0001_0D00;

/// `dwVersion` of the solid-resource variant (`install.esd`) -- recognized
/// only to refuse it with a precise error.
pub const WIM_VERSION_SOLID: u32 = 0x0000_0E00;

/// `FLAG_HEADER_SPANNED`: resource data referenced by this WIM's images may
/// live in another part.
pub const FLAG_HEADER_SPANNED: u32 = 0x0000_0008;

/// `RESHDR_FLAG_METADATA`: this lookup-table resource is an image's
/// metadata resource, not a file resource.
pub const RESHDR_FLAG_METADATA: u8 = 0x02;

/// One `RESHDR_DISK_SHORT` (24 bytes): where a resource lives in this .wim
/// file and how big it is, stored and uncompressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceHeader {
    /// `sizebytes[7]`: bytes the resource occupies *in the WIM* (compressed
    /// size when compressed, else the plain size). 56 bits on disk.
    pub size_in_wim: u64,
    /// `bFlags` (`RESHDR_FLAG_*`).
    pub flags: u8,
    /// `liOffset`: absolute byte offset of the resource in this file.
    pub offset: u64,
    /// `liOriginalSize`: uncompressed size.
    pub original_size: u64,
}

impl ResourceHeader {
    pub fn is_metadata(&self) -> bool {
        self.flags & RESHDR_FLAG_METADATA != 0
    }

    /// Present at all? An all-zero resource header means "no such resource"
    /// (e.g. no integrity table, no boot metadata).
    pub fn is_present(&self) -> bool {
        self.size_in_wim != 0 || self.offset != 0
    }

    fn parse(bytes: &[u8; 24]) -> Self {
        let mut size = [0u8; 8];
        size[..7].copy_from_slice(&bytes[..7]);
        Self {
            size_in_wim: u64::from_le_bytes(size),
            flags: bytes[7],
            offset: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            original_size: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        }
    }

    fn serialize(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[..8].copy_from_slice(&self.size_in_wim.to_le_bytes());
        debug_assert!(self.size_in_wim < 1 << 56);
        out[7] = self.flags;
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.original_size.to_le_bytes());
        out
    }
}

/// The fixed 208-byte WIM header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WimHeader {
    pub version: u32,
    pub flags: u32,
    /// `dwCompressionSize`: the compression chunk size resources were
    /// captured with (32768 in practice). Copied verbatim into every part.
    pub compression_size: u32,
    pub guid: [u8; 16],
    pub part_number: u16,
    pub total_parts: u16,
    pub image_count: u32,
    pub offset_table: ResourceHeader,
    pub xml_data: ResourceHeader,
    pub boot_metadata: ResourceHeader,
    pub boot_index: u32,
    pub integrity: ResourceHeader,
}

impl WimHeader {
    pub fn parse(bytes: &[u8; HEADER_SIZE]) -> io::Result<Self> {
        if bytes[..8] != SIGNATURE {
            return Err(io::Error::other("not a WIM file (bad MSWIM signature)"));
        }
        let header_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if header_size as usize != HEADER_SIZE {
            return Err(io::Error::other(format!(
                "unsupported WIM header size {header_size} (expected {HEADER_SIZE})"
            )));
        }
        let le32 = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let le16 = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        let rh = |at: usize| ResourceHeader::parse(bytes[at..at + 24].try_into().unwrap());
        Ok(Self {
            version: le32(12),
            flags: le32(16),
            compression_size: le32(20),
            guid: bytes[24..40].try_into().unwrap(),
            part_number: le16(40),
            total_parts: le16(42),
            image_count: le32(44),
            offset_table: rh(48),
            xml_data: rh(72),
            boot_metadata: rh(96),
            boot_index: le32(120),
            integrity: rh(124),
        })
    }

    pub fn serialize(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[..8].copy_from_slice(&SIGNATURE);
        out[8..12].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        out[12..16].copy_from_slice(&self.version.to_le_bytes());
        out[16..20].copy_from_slice(&self.flags.to_le_bytes());
        out[20..24].copy_from_slice(&self.compression_size.to_le_bytes());
        out[24..40].copy_from_slice(&self.guid);
        out[40..42].copy_from_slice(&self.part_number.to_le_bytes());
        out[42..44].copy_from_slice(&self.total_parts.to_le_bytes());
        out[44..48].copy_from_slice(&self.image_count.to_le_bytes());
        out[48..72].copy_from_slice(&self.offset_table.serialize());
        out[72..96].copy_from_slice(&self.xml_data.serialize());
        out[96..120].copy_from_slice(&self.boot_metadata.serialize());
        out[120..124].copy_from_slice(&self.boot_index.to_le_bytes());
        out[124..148].copy_from_slice(&self.integrity.serialize());
        // bUnused[60] stays zero.
        out
    }
}

/// One lookup-table entry (`_RESHDR_DISK`, 50 bytes on disk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LookupEntry {
    pub resource: ResourceHeader,
    pub part_number: u16,
    pub ref_count: u32,
    /// SHA-1 of the *uncompressed* data -- copied verbatim, valid by
    /// construction since resources are never re-encoded.
    pub sha1: [u8; 20],
}

/// Size of one lookup-table entry on disk.
pub const LOOKUP_ENTRY_SIZE: u64 = 50;

impl LookupEntry {
    fn parse(bytes: &[u8; 50]) -> Self {
        Self {
            resource: ResourceHeader::parse(bytes[..24].try_into().unwrap()),
            part_number: u16::from_le_bytes(bytes[24..26].try_into().unwrap()),
            ref_count: u32::from_le_bytes(bytes[26..30].try_into().unwrap()),
            sha1: bytes[30..50].try_into().unwrap(),
        }
    }

    fn serialize(&self) -> [u8; 50] {
        let mut out = [0u8; 50];
        out[..24].copy_from_slice(&self.resource.serialize());
        out[24..26].copy_from_slice(&self.part_number.to_le_bytes());
        out[26..30].copy_from_slice(&self.ref_count.to_le_bytes());
        out[30..50].copy_from_slice(&self.sha1);
        out
    }
}

/// A parsed, validated WIM: header, every lookup-table entry, and the raw
/// XML data block. Holds no file data -- resources are streamed straight
/// from the source reader at split time.
#[derive(Debug, Clone)]
pub struct WimImage {
    pub header: WimHeader,
    pub entries: Vec<LookupEntry>,
    /// The XML data block, verbatim (UTF-16LE, 0xFEFF BOM). Copied into
    /// every split part, per the spec's sample layout.
    pub xml_data: Vec<u8>,
}

impl WimImage {
    /// Parses and validates the WIM at `source`'s start. Refuses solid
    /// (ESD) images and spanned inputs -- splitting an already-split set is
    /// out of scope.
    pub fn open<R: Read + Seek>(source: &mut R) -> io::Result<Self> {
        source.seek(SeekFrom::Start(0))?;
        let mut header_bytes = [0u8; HEADER_SIZE];
        source.read_exact(&mut header_bytes)?;
        let header = WimHeader::parse(&header_bytes)?;

        if header.version == WIM_VERSION_SOLID {
            return Err(io::Error::other(
                "this is a solid-compressed image (install.esd, WIM version 0xE00): its LZMS \
                 blocks pack multiple files together, so it cannot be split at resource \
                 boundaries -- use install media that ships install.wim instead",
            ));
        }
        if header.version != WIM_VERSION {
            return Err(io::Error::other(format!(
                "unsupported WIM version {:#x} (expected {WIM_VERSION:#x})",
                header.version
            )));
        }
        if header.total_parts > 1 {
            return Err(io::Error::other(format!(
                "already a split WIM part ({} of {}); refusing to re-split",
                header.part_number, header.total_parts
            )));
        }
        if !header.offset_table.is_present() {
            return Err(io::Error::other("WIM has no resource lookup table"));
        }
        if header.offset_table.size_in_wim % LOOKUP_ENTRY_SIZE != 0 {
            return Err(io::Error::other(format!(
                "lookup table size {} is not a multiple of {LOOKUP_ENTRY_SIZE}",
                header.offset_table.size_in_wim
            )));
        }

        let entry_count = header.offset_table.size_in_wim / LOOKUP_ENTRY_SIZE;
        source.seek(SeekFrom::Start(header.offset_table.offset))?;
        let mut entries = Vec::with_capacity(entry_count as usize);
        for _ in 0..entry_count {
            let mut raw = [0u8; 50];
            source.read_exact(&mut raw)?;
            entries.push(LookupEntry::parse(&raw));
        }

        let mut xml_data = Vec::new();
        if header.xml_data.is_present() {
            source.seek(SeekFrom::Start(header.xml_data.offset))?;
            xml_data = vec![0u8; header.xml_data.size_in_wim as usize];
            source.read_exact(&mut xml_data)?;
        }

        Ok(Self {
            header,
            entries,
            xml_data,
        })
    }

    /// Total stored bytes across all resources -- what actually has to be
    /// copied when splitting (excludes header/table/XML bookkeeping).
    pub fn total_resource_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.resource.size_in_wim).sum()
    }
}

/// What [`split`] produced: how many parts, and each part's byte size.
#[derive(Debug)]
pub struct SplitOutcome {
    pub part_sizes: Vec<u64>,
}

/// The fixed per-part overhead [`plan_parts`] budgets for on top of the
/// resources themselves: the header plus the XML data block (copied into
/// every part). The part's own lookup table is accounted per entry.
fn part_overhead(xml_len: u64) -> u64 {
    HEADER_SIZE as u64 + xml_len
}

/// Assigns every lookup entry (by index into `wim.entries`) to a part.
/// Returns one `Vec<usize>` per part, in the order the entries' resources
/// will be laid out in that part.
///
/// Placement rules, per the spec's split-WIM section:
/// - Every metadata resource goes in part 1, first, in source order.
/// - File resources then fill parts first-fit in source-offset order; a
///   part is closed when adding the next resource would push
///   header + resources + lookup table + XML past `max_part_bytes`.
/// - A single resource that can never fit (even alone in a fresh part)
///   still gets its own part, which will simply exceed `max_part_bytes` --
///   the same behavior `wimsplit` documents. Callers who cannot tolerate an
///   oversized part (FAT32 can't) must check the outcome's `part_sizes`.
fn plan_parts(wim: &WimImage, max_part_bytes: u64) -> Vec<Vec<usize>> {
    let xml_len = wim.xml_data.len() as u64;

    // Source-offset order keeps the copy pass sequential over the input.
    let mut order: Vec<usize> = (0..wim.entries.len()).collect();
    order.sort_by_key(|&i| wim.entries[i].resource.offset);

    let mut parts: Vec<Vec<usize>> = vec![Vec::new()];
    let mut current_bytes = part_overhead(xml_len);

    // Metadata resources first, all pinned to part 1 regardless of budget:
    // the spec requires them there, and install.wim metadata is small.
    for &i in order
        .iter()
        .filter(|&&i| wim.entries[i].resource.is_metadata())
    {
        parts[0].push(i);
        current_bytes += wim.entries[i].resource.size_in_wim + LOOKUP_ENTRY_SIZE;
    }

    for &i in order
        .iter()
        .filter(|&&i| !wim.entries[i].resource.is_metadata())
    {
        let cost = wim.entries[i].resource.size_in_wim + LOOKUP_ENTRY_SIZE;
        let fits = current_bytes + cost <= max_part_bytes;
        let part_is_empty = parts.last().is_some_and(Vec::is_empty) && parts.len() > 1;
        if !fits && !part_is_empty && !(parts.len() == 1 && parts[0].is_empty()) {
            parts.push(Vec::new());
            current_bytes = part_overhead(xml_len);
        }
        parts.last_mut().expect("parts is never empty").push(i);
        current_bytes += cost;
    }

    parts
}

/// Splits `source` (a whole, non-solid `.wim`) into spanned `.swm` parts of
/// at most `max_part_bytes` each (except a part holding one resource that
/// alone exceeds the limit -- see [`plan_parts`]). For each part, `next_part`
/// is called with the 1-based part number and must return the writer to
/// stream that part into; each part is written strictly sequentially, so a
/// plain `Write` sink (a `fatfs` file, a socket, anything) works -- no
/// seek-back patching.
///
/// `on_bytes_copied` is called with the cumulative count of *resource*
/// bytes copied so far (out of [`WimImage::total_resource_bytes`]), for
/// progress reporting.
pub fn split<R, W, F>(
    source: &mut R,
    wim: &WimImage,
    max_part_bytes: u64,
    mut next_part: F,
    mut on_bytes_copied: impl FnMut(u64),
) -> io::Result<SplitOutcome>
where
    R: Read + Seek,
    W: Write,
    F: FnMut(u16) -> io::Result<W>,
{
    let assignment = plan_parts(wim, max_part_bytes);
    let total_parts = assignment.len();
    if total_parts > u16::MAX as usize {
        return Err(io::Error::other(format!(
            "split would need {total_parts} parts, over the format's 65535 ceiling"
        )));
    }
    let total_parts = total_parts as u16;

    let mut part_sizes = Vec::with_capacity(assignment.len());
    let mut copied_total = 0u64;
    let mut copy_buf = vec![0u8; 1 << 20];

    for (part_index, entry_indices) in assignment.iter().enumerate() {
        let part_number = (part_index + 1) as u16;

        // Lay the part out up front -- resources right after the header, in
        // assignment order, then the lookup table, then the XML block -- so
        // the header can be emitted first and everything written forward.
        let mut cursor = HEADER_SIZE as u64;
        let mut placed: Vec<LookupEntry> = Vec::with_capacity(entry_indices.len());
        for &i in entry_indices {
            let mut entry = wim.entries[i];
            entry.resource.offset = cursor;
            entry.part_number = part_number;
            cursor += entry.resource.size_in_wim;
            placed.push(entry);
        }
        let table_offset = cursor;
        let table_len = LOOKUP_ENTRY_SIZE * placed.len() as u64;
        let xml_offset = table_offset + table_len;
        let xml_len = wim.xml_data.len() as u64;
        let part_total = xml_offset + xml_len;

        // Part 1 keeps the boot pointers, remapped to the boot metadata
        // resource's new offset; later parts carry no metadata at all.
        let (boot_metadata, boot_index) = if part_number == 1
            && wim.header.boot_metadata.is_present()
        {
            let remapped = placed
                .iter()
                .zip(entry_indices)
                .find(|(_, &i)| wim.entries[i].resource.offset == wim.header.boot_metadata.offset)
                .map(|(placed_entry, _)| placed_entry.resource);
            match remapped {
                Some(mut resource) => {
                    // The header's copy carries the same flags the source
                    // header used, not the lookup entry's.
                    resource.flags = wim.header.boot_metadata.flags;
                    (resource, wim.header.boot_index)
                }
                None => (ResourceHeader::default(), 0),
            }
        } else {
            (ResourceHeader::default(), 0)
        };

        let header = WimHeader {
            version: wim.header.version,
            flags: wim.header.flags | FLAG_HEADER_SPANNED,
            compression_size: wim.header.compression_size,
            guid: wim.header.guid,
            part_number,
            total_parts,
            image_count: wim.header.image_count,
            offset_table: ResourceHeader {
                size_in_wim: table_len,
                // The lookup table itself is stored uncompressed; keep the
                // source's flags for it verbatim.
                flags: wim.header.offset_table.flags,
                offset: table_offset,
                original_size: table_len,
            },
            xml_data: ResourceHeader {
                size_in_wim: xml_len,
                flags: wim.header.xml_data.flags,
                offset: xml_offset,
                original_size: xml_len,
            },
            boot_metadata,
            boot_index,
            integrity: ResourceHeader::default(),
        };

        let mut out = next_part(part_number)?;
        out.write_all(&header.serialize())?;

        for (&i, placed_entry) in entry_indices.iter().zip(placed.iter()) {
            let original = &wim.entries[i].resource;
            debug_assert_eq!(placed_entry.resource.size_in_wim, original.size_in_wim);
            source.seek(SeekFrom::Start(original.offset))?;
            let mut remaining = original.size_in_wim;
            while remaining > 0 {
                let want = remaining.min(copy_buf.len() as u64) as usize;
                source.read_exact(&mut copy_buf[..want])?;
                out.write_all(&copy_buf[..want])?;
                remaining -= want as u64;
                copied_total += want as u64;
                on_bytes_copied(copied_total);
            }
        }

        for entry in &placed {
            out.write_all(&entry.serialize())?;
        }
        out.write_all(&wim.xml_data)?;
        out.flush()?;
        part_sizes.push(part_total);
    }

    Ok(SplitOutcome { part_sizes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A synthetic resource for fixture WIMs: content bytes plus whether
    /// it's flagged as a metadata resource. The splitter treats resource
    /// content as opaque bytes (it never decompresses), so fixtures don't
    /// need real XPRESS streams or real metadata trees -- format validity
    /// against real WIMs is the wimlib oracle harness's job (M2.4).
    struct FixtureResource {
        content: Vec<u8>,
        metadata: bool,
    }

    fn fake_sha1(seed: u8) -> [u8; 20] {
        [seed; 20]
    }

    /// Hand-builds a structurally valid single-part WIM: header, resources
    /// back to back, lookup table, XML block.
    fn build_wim(resources: &[FixtureResource], xml: &[u8]) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        let mut entries: Vec<LookupEntry> = Vec::new();
        let mut cursor = HEADER_SIZE as u64;
        for (i, res) in resources.iter().enumerate() {
            entries.push(LookupEntry {
                resource: ResourceHeader {
                    size_in_wim: res.content.len() as u64,
                    flags: if res.metadata {
                        RESHDR_FLAG_METADATA
                    } else {
                        0
                    },
                    offset: cursor,
                    original_size: res.content.len() as u64,
                },
                part_number: 1,
                ref_count: 1,
                sha1: fake_sha1(i as u8 + 1),
            });
            body.extend_from_slice(&res.content);
            cursor += res.content.len() as u64;
        }
        let table_offset = cursor;
        let table_len = LOOKUP_ENTRY_SIZE * entries.len() as u64;
        let xml_offset = table_offset + table_len;

        let boot_metadata = entries
            .iter()
            .find(|e| e.resource.is_metadata())
            .map(|e| e.resource)
            .unwrap_or_default();

        let header = WimHeader {
            version: WIM_VERSION,
            flags: 0x2 | 0x0002_0000, // COMPRESSION | COMPRESS_XPRESS
            compression_size: 32768,
            guid: [0xAB; 16],
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
                flags: RESHDR_FLAG_METADATA,
                offset: xml_offset,
                original_size: xml.len() as u64,
            },
            boot_metadata,
            boot_index: if boot_metadata.is_present() { 1 } else { 0 },
            integrity: ResourceHeader::default(),
        };

        let mut out = Vec::new();
        out.extend_from_slice(&header.serialize());
        out.extend_from_slice(&body);
        for e in &entries {
            out.extend_from_slice(&e.serialize());
        }
        out.extend_from_slice(xml);
        out
    }

    fn xml_fixture() -> Vec<u8> {
        // 0xFEFF BOM in UTF-16LE, then a token body; the splitter treats
        // this as opaque bytes.
        let mut xml = vec![0xFF, 0xFE];
        for unit in "<WIM/>".encode_utf16() {
            xml.extend_from_slice(&unit.to_le_bytes());
        }
        xml
    }

    fn typical_fixture() -> Vec<u8> {
        build_wim(
            &[
                FixtureResource {
                    content: vec![0xEE; 300], // metadata resource
                    metadata: true,
                },
                FixtureResource {
                    content: vec![0x11; 900],
                    metadata: false,
                },
                FixtureResource {
                    content: vec![0x22; 900],
                    metadata: false,
                },
                FixtureResource {
                    content: vec![0x33; 900],
                    metadata: false,
                },
            ],
            &xml_fixture(),
        )
    }

    /// Convenience: split an in-memory WIM into in-memory parts.
    fn split_to_vecs(wim_bytes: &[u8], max_part_bytes: u64) -> Vec<Vec<u8>> {
        let mut source = Cursor::new(wim_bytes.to_vec());
        let wim = WimImage::open(&mut source).unwrap();
        let parts = std::cell::RefCell::new(Vec::<Cursor<Vec<u8>>>::new());
        let outcome = split(
            &mut source,
            &wim,
            max_part_bytes,
            |_| {
                parts.borrow_mut().push(Cursor::new(Vec::new()));
                // Hand each part's cursor back by index via a small shim.
                struct Shim<'a>(&'a RefCellParts, usize);
                type RefCellParts = std::cell::RefCell<Vec<Cursor<Vec<u8>>>>;
                impl Write for Shim<'_> {
                    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                        self.0.borrow_mut()[self.1].write(buf)
                    }
                    fn flush(&mut self) -> io::Result<()> {
                        Ok(())
                    }
                }
                let idx = parts.borrow().len() - 1;
                Ok(Shim(&parts, idx))
            },
            |_| {},
        )
        .unwrap();
        let vecs: Vec<Vec<u8>> = parts
            .into_inner()
            .into_iter()
            .map(Cursor::into_inner)
            .collect();
        assert_eq!(outcome.part_sizes.len(), vecs.len());
        for (size, part) in outcome.part_sizes.iter().zip(&vecs) {
            assert_eq!(*size, part.len() as u64, "reported part size must match");
        }
        vecs
    }

    #[test]
    fn header_round_trips_through_parse_and_serialize() {
        let wim_bytes = typical_fixture();
        let header = WimHeader::parse(wim_bytes[..HEADER_SIZE].try_into().unwrap()).unwrap();
        assert_eq!(header.serialize().as_slice(), &wim_bytes[..HEADER_SIZE]);
    }

    #[test]
    fn open_reads_all_entries_and_the_xml_block() {
        let wim_bytes = typical_fixture();
        let wim = WimImage::open(&mut Cursor::new(wim_bytes)).unwrap();
        assert_eq!(wim.entries.len(), 4);
        assert_eq!(wim.xml_data, xml_fixture());
        assert_eq!(wim.total_resource_bytes(), 300 + 900 * 3);
        assert_eq!(
            wim.entries
                .iter()
                .filter(|e| e.resource.is_metadata())
                .count(),
            1
        );
    }

    #[test]
    fn open_refuses_a_solid_esd_image_with_a_clear_message() {
        let mut wim_bytes = typical_fixture();
        wim_bytes[12..16].copy_from_slice(&WIM_VERSION_SOLID.to_le_bytes());
        let err = WimImage::open(&mut Cursor::new(wim_bytes)).unwrap_err();
        assert!(err.to_string().contains("install.esd"), "got: {err}");
    }

    #[test]
    fn open_refuses_a_non_wim_file() {
        let err = WimImage::open(&mut Cursor::new(vec![0u8; 4096])).unwrap_err();
        assert!(err.to_string().contains("MSWIM"), "got: {err}");
    }

    #[test]
    fn open_refuses_an_already_split_part() {
        let mut wim_bytes = typical_fixture();
        wim_bytes[42..44].copy_from_slice(&3u16.to_le_bytes()); // usTotalParts
        let err = WimImage::open(&mut Cursor::new(wim_bytes)).unwrap_err();
        assert!(err.to_string().contains("split"), "got: {err}");
    }

    #[test]
    fn a_limit_bigger_than_the_wim_yields_one_part_holding_everything() {
        let parts = split_to_vecs(&typical_fixture(), 1 << 30);
        assert_eq!(parts.len(), 1);
        let header = WimHeader::parse(parts[0][..HEADER_SIZE].try_into().unwrap()).unwrap();
        assert_eq!((header.part_number, header.total_parts), (1, 1));
        assert_ne!(header.flags & FLAG_HEADER_SPANNED, 0);
    }

    #[test]
    fn a_tight_limit_produces_multiple_parts_each_under_it() {
        // Overhead per part: 208 header + 14-byte XML (= 222); each file
        // resource costs 900 + 50 and the metadata resource 300 + 50. With
        // a 1500-byte limit, part 1 (222 + 350 = 572) has no room left for
        // a 950-byte file resource, so the three file resources land one
        // per part after it: 4 parts total.
        let limit = 1500;
        let parts = split_to_vecs(&typical_fixture(), limit);
        assert_eq!(parts.len(), 4);
        for part in &parts {
            assert!(
                part.len() as u64 <= limit,
                "part size {} > {limit}",
                part.len()
            );
        }
    }

    #[test]
    fn part_headers_carry_consistent_numbering_guid_and_span_flag() {
        let parts = split_to_vecs(&typical_fixture(), 1500);
        let total = parts.len() as u16;
        for (idx, part) in parts.iter().enumerate() {
            let header = WimHeader::parse(part[..HEADER_SIZE].try_into().unwrap()).unwrap();
            assert_eq!(header.part_number, idx as u16 + 1);
            assert_eq!(header.total_parts, total);
            assert_eq!(header.guid, [0xAB; 16]);
            assert_eq!(header.image_count, 1);
            assert_ne!(header.flags & FLAG_HEADER_SPANNED, 0);
            assert!(!header.integrity.is_present());
        }
    }

    #[test]
    fn every_part_carries_its_own_lookup_table_and_the_xml_block() {
        let original = {
            let mut cursor = Cursor::new(typical_fixture());
            WimImage::open(&mut cursor).unwrap()
        };
        let parts = split_to_vecs(&typical_fixture(), 1500);

        let mut seen_hashes = Vec::new();
        for (idx, part) in parts.iter().enumerate() {
            let mut cursor = Cursor::new(part.clone());
            // Re-open each part with this module's own reader -- accepting
            // total_parts > 1 is the one check to bypass, so parse manually.
            let header = WimHeader::parse(part[..HEADER_SIZE].try_into().unwrap()).unwrap();
            let count = header.offset_table.size_in_wim / LOOKUP_ENTRY_SIZE;
            cursor
                .seek(SeekFrom::Start(header.offset_table.offset))
                .unwrap();
            for _ in 0..count {
                let mut raw = [0u8; 50];
                cursor.read_exact(&mut raw).unwrap();
                let entry = LookupEntry::parse(&raw);
                assert_eq!(entry.part_number, idx as u16 + 1);
                seen_hashes.push(entry.sha1);
            }
            // XML block identical in every part, right where the header says.
            let xml_start = header.xml_data.offset as usize;
            let xml_end = xml_start + header.xml_data.size_in_wim as usize;
            assert_eq!(&part[xml_start..xml_end], original.xml_data.as_slice());
        }

        // No resource lost, none duplicated.
        let mut expected: Vec<[u8; 20]> = original.entries.iter().map(|e| e.sha1).collect();
        expected.sort_unstable();
        seen_hashes.sort_unstable();
        assert_eq!(seen_hashes, expected);
    }

    #[test]
    fn resource_bytes_survive_the_split_verbatim() {
        let wim_bytes = typical_fixture();
        let original = WimImage::open(&mut Cursor::new(wim_bytes.clone())).unwrap();
        let parts = split_to_vecs(&wim_bytes, 1500);

        for part in &parts {
            let header = WimHeader::parse(part[..HEADER_SIZE].try_into().unwrap()).unwrap();
            let count = header.offset_table.size_in_wim / LOOKUP_ENTRY_SIZE;
            let table_start = header.offset_table.offset as usize;
            for n in 0..count as usize {
                let at = table_start + n * LOOKUP_ENTRY_SIZE as usize;
                let entry = LookupEntry::parse(part[at..at + 50].try_into().unwrap());
                let original_entry = original
                    .entries
                    .iter()
                    .find(|e| e.sha1 == entry.sha1)
                    .expect("every part entry must come from the original");
                let new = &part[entry.resource.offset as usize
                    ..(entry.resource.offset + entry.resource.size_in_wim) as usize];
                let old = &wim_bytes[original_entry.resource.offset as usize
                    ..(original_entry.resource.offset + original_entry.resource.size_in_wim)
                        as usize];
                assert_eq!(new, old, "stored resource bytes must be copied verbatim");
                assert_eq!(entry.ref_count, original_entry.ref_count);
                assert_eq!(
                    entry.resource.original_size,
                    original_entry.resource.original_size
                );
                assert_eq!(entry.resource.flags, original_entry.resource.flags);
            }
        }
    }

    #[test]
    fn metadata_resources_all_land_in_part_one_with_boot_pointers_remapped() {
        let parts = split_to_vecs(&typical_fixture(), 1500);

        let first = WimHeader::parse(parts[0][..HEADER_SIZE].try_into().unwrap()).unwrap();
        assert!(first.boot_metadata.is_present());
        assert_eq!(first.boot_index, 1);
        // The remapped pointer must land on the metadata resource's bytes
        // (the fixture's metadata content is all 0xEE).
        let at = first.boot_metadata.offset as usize;
        assert_eq!(parts[0][at], 0xEE);

        for part in &parts[1..] {
            let header = WimHeader::parse(part[..HEADER_SIZE].try_into().unwrap()).unwrap();
            assert!(!header.boot_metadata.is_present());
            assert_eq!(header.boot_index, 0);
            let count = header.offset_table.size_in_wim / LOOKUP_ENTRY_SIZE;
            let table_start = header.offset_table.offset as usize;
            for n in 0..count as usize {
                let at = table_start + n * LOOKUP_ENTRY_SIZE as usize;
                let entry = LookupEntry::parse(part[at..at + 50].try_into().unwrap());
                assert!(
                    !entry.resource.is_metadata(),
                    "metadata resources belong to part 1 only"
                );
            }
        }
    }

    #[test]
    fn a_single_resource_bigger_than_the_limit_gets_its_own_oversized_part() {
        let wim_bytes = build_wim(
            &[
                FixtureResource {
                    content: vec![0xEE; 100],
                    metadata: true,
                },
                FixtureResource {
                    content: vec![0x44; 5_000], // alone exceeds the limit
                    metadata: false,
                },
                FixtureResource {
                    content: vec![0x55; 200],
                    metadata: false,
                },
            ],
            &xml_fixture(),
        );
        let mut source = Cursor::new(wim_bytes);
        let wim = WimImage::open(&mut source).unwrap();
        let mut sizes = Vec::new();
        let outcome = split(&mut source, &wim, 1000, |_| Ok(io::sink()), |_| {}).unwrap();
        sizes.extend(outcome.part_sizes.iter().copied());
        // One part exceeds the limit (the 5000-byte resource), the rest
        // stay under it -- and nothing was dropped.
        assert!(sizes.iter().any(|&s| s > 1000));
        assert_eq!(sizes.iter().filter(|&&s| s > 1000).count(), 1);
    }

    #[test]
    fn progress_callback_reports_every_resource_byte_exactly_once() {
        let wim_bytes = typical_fixture();
        let mut source = Cursor::new(wim_bytes);
        let wim = WimImage::open(&mut source).unwrap();
        let total = wim.total_resource_bytes();
        let mut last = 0u64;
        split(
            &mut source,
            &wim,
            1500,
            |_| Ok(io::sink()),
            |done| {
                assert!(done >= last, "progress must be monotonic");
                last = done;
            },
        )
        .unwrap();
        assert_eq!(last, total);
    }
}
