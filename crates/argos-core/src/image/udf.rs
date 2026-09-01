//! Minimal, read-only UDF (ECMA-167) reader with **streaming** file access --
//! Argos's own implementation, written for backlog #40 (phase 3, M1).
//!
//! Real official Windows installer ISOs are UDF bridge images (see
//! [`image::windows`](super::windows)'s top doc comment), and the previous
//! backend, the `hadris-udf` crate, only exposed a whole-file-into-memory
//! read -- a multi-GB `install.wim` cost that much RAM during a copy, which
//! OOM-killed a real machine during the first real-hardware Windows write
//! attempt (#38). Its extent resolution is private, so streaming could not be
//! added from outside the crate; per the phase-3 plan
//! (`docs/plan-phase3-self-contained.md`), Argos now carries this module
//! instead, and `hadris-udf` remains only as a *dev-dependency* fixture
//! generator -- an independent implementation whose output this reader is
//! tested against.
//!
//! Scope: exactly what reading Windows install media (UDF 1.02-2.60 bridge
//! discs mastered by `oscdimg`/`mkisofs`, 2048-byte blocks) needs --
//!
//! - volume recognition sequence check (`NSR02`/`NSR03`), anchor volume
//!   descriptor pointer, main + reserve volume descriptor sequences;
//! - type-1 (physical) partition maps only -- virtual/sparable/metadata
//!   partitions (UDF 2.50+ rewritable-media features) are refused, not
//!   half-supported; Windows install media never uses them;
//! - File Entries and Extended File Entries, ICB strategies 4 and 4096,
//!   short/long allocation descriptors, embedded (inline) data, sparse
//!   extents, and allocation-extent continuation blocks;
//! - directory listing via File Identifier Descriptors, OSTA CS0 names
//!   (8- and 16-bit compression).
//!
//! Every descriptor's tag checksum *and* CRC are verified -- corrupt
//! metadata is reported, never silently misparsed. File *content* bytes are
//! not covered by any UDF checksum; content integrity is `image::checksum`'s
//! job, one layer up.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Mutex;

/// UDF on ISO images uses one logical block per 2048-byte sector. Media with
/// a different logical block size (some rewritable formats) is refused in
/// `open` rather than misread.
const BLOCK: u64 = 2048;

/// How many volume structure descriptors to scan at most before giving up on
/// finding `NSR0x` -- a bridge disc carries a handful (ISO9660's own set plus
/// `BEA01`/`NSR0x`/`TEA01`), nowhere near this.
const MAX_VSD_SCAN: u64 = 64;

/// Upper bound on volume descriptor sequence blocks walked, as a loop guard
/// against a corrupt sequence pointing at itself.
const MAX_VDS_BLOCKS: u32 = 1024;

/// Upper bound on chained allocation-extent continuation blocks per file.
const MAX_AED_CHAIN: usize = 4096;

/// Directories are materialized in memory to parse their File Identifier
/// Descriptors; a real install-media directory is a few KB. This cap turns a
/// corrupt information length into an error instead of an allocation bomb.
const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum UdfError {
    /// The image is not a UDF volume at all -- callers fall back to ISO9660.
    NotUdf,
    /// A real I/O failure from the underlying reader -- always propagated.
    Io(io::Error),
    /// Structurally UDF, but a descriptor is malformed or fails its
    /// checksum/CRC.
    Corrupt(String),
    /// Valid UDF using a feature outside this reader's documented scope.
    Unsupported(String),
}

impl fmt::Display for UdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UdfError::NotUdf => write!(f, "not a UDF volume"),
            UdfError::Io(err) => write!(f, "I/O error reading UDF volume: {err}"),
            UdfError::Corrupt(what) => write!(f, "corrupt UDF volume: {what}"),
            UdfError::Unsupported(what) => write!(f, "unsupported UDF feature: {what}"),
        }
    }
}

impl std::error::Error for UdfError {}

impl From<io::Error> for UdfError {
    fn from(err: io::Error) -> Self {
        // An unexpected EOF while reading a descriptor a valid volume must
        // contain means the image is truncated/not UDF, not that the disk
        // failed -- keep it distinguishable from real I/O errors so `open`'s
        // callers can still fall back to ISO9660 on it.
        if err.kind() == io::ErrorKind::UnexpectedEof {
            UdfError::Corrupt("descriptor read past end of image".to_string())
        } else {
            UdfError::Io(err)
        }
    }
}

impl UdfError {
    /// Maps to `io::Error` for callers speaking plain `std::io`.
    pub fn into_io(self) -> io::Error {
        match self {
            UdfError::Io(err) => err,
            other => io::Error::other(other.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, UdfError>;

/// CRC-ITU-T (polynomial 0x1021, initial value 0), as ECMA-167 1/7.2.6
/// requires for descriptor tags.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A parsed and *verified* descriptor tag (ECMA-167 3/7.2): checksum,
/// descriptor version, and CRC have all already passed.
struct Tag {
    id: u16,
}

/// Parses the 16-byte tag at the start of `block` and verifies it against
/// the descriptor bytes that follow. The recorded tag location is *not*
/// checked: mastering tools disagree on partition-relative vs absolute
/// numbering, and the checksum + CRC already authenticate the descriptor.
fn parse_tag(block: &[u8]) -> Result<Tag> {
    if block.len() < 16 {
        return Err(UdfError::Corrupt("descriptor shorter than its tag".into()));
    }
    let id = u16::from_le_bytes([block[0], block[1]]);
    let version = u16::from_le_bytes([block[2], block[3]]);
    let checksum = block[4];
    let crc = u16::from_le_bytes([block[8], block[9]]);
    let crc_len = u16::from_le_bytes([block[10], block[11]]);

    let mut sum: u8 = 0;
    for (i, &b) in block[..16].iter().enumerate() {
        if i != 4 {
            sum = sum.wrapping_add(b);
        }
    }
    if sum != checksum {
        return Err(UdfError::Corrupt("descriptor tag checksum mismatch".into()));
    }
    if !matches!(version, 2 | 3) {
        return Err(UdfError::Corrupt(format!(
            "descriptor tag version {version} (expected 2 or 3)"
        )));
    }
    let crc_end = 16usize
        .checked_add(usize::from(crc_len))
        .filter(|&end| end <= block.len())
        .ok_or_else(|| UdfError::Corrupt("descriptor CRC length exceeds its block".into()))?;
    if crc16(&block[16..crc_end]) != crc {
        return Err(UdfError::Corrupt(format!(
            "descriptor CRC mismatch on tag {id}"
        )));
    }
    Ok(Tag { id })
}

// Tag identifiers this module handles (ECMA-167 3/7.2.1 and 4/7.2.1).
const TAG_ANCHOR: u16 = 2;
const TAG_VDS_POINTER: u16 = 3;
const TAG_PARTITION: u16 = 5;
const TAG_LOGICAL_VOLUME: u16 = 6;
const TAG_TERMINATOR: u16 = 8;
const TAG_FILE_SET: u16 = 256;
const TAG_FILE_IDENTIFIER: u16 = 257;
const TAG_ALLOCATION_EXTENT: u16 = 258;
const TAG_INDIRECT_ENTRY: u16 = 259;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXTENDED_FILE_ENTRY: u16 = 266;

// ICB file types (ECMA-167 4/14.6.6).
const FILE_TYPE_DIRECTORY: u8 = 4;

/// A long allocation descriptor's ICB address (ECMA-167 4/14.14.2), kept
/// opaque outside this module: logical block + partition reference. This is
/// what a directory entry carries to name the File Entry behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcbRef {
    block: u32,
    partition_ref: u16,
}

fn parse_long_ad(bytes: &[u8]) -> (u64, u8, IcbRef) {
    let len_raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let block = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let partition_ref = u16::from_le_bytes([bytes[8], bytes[9]]);
    (
        u64::from(len_raw & 0x3FFF_FFFF),
        (len_raw >> 30) as u8,
        IcbRef {
            block,
            partition_ref,
        },
    )
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdfDirEntry {
    name: String,
    is_dir: bool,
    /// The file's information length in bytes (0 for directories' own
    /// listing purposes -- a directory's byte size is not meaningful to
    /// callers here).
    pub size: u64,
    /// The ICB behind this entry -- pass to
    /// [`UdfVolume::read_directory`] to descend into a subdirectory.
    pub icb: IcbRef,
}

impl UdfDirEntry {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }
    pub fn is_file(&self) -> bool {
        !self.is_dir
    }
}

/// A directory's parsed entries (`.`/parent excluded).
pub struct UdfDir {
    entries: Vec<UdfDirEntry>,
}

impl UdfDir {
    pub fn entries(&self) -> std::slice::Iter<'_, UdfDirEntry> {
        self.entries.iter()
    }
}

/// One run of a file's content: either `len` bytes at `image_offset` in the
/// underlying image, or `len` zero bytes (a sparse/unrecorded extent).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Extent {
    image_offset: Option<u64>,
    len: u64,
}

/// A file's resolved content layout.
enum FileData {
    /// Data embedded directly in the File Entry (small files).
    Inline(Vec<u8>),
    Extents(Vec<Extent>),
}

/// What a File Entry / Extended File Entry says about one file, before its
/// allocation descriptors are walked.
struct FileMeta {
    file_type: u8,
    info_len: u64,
    /// ICB tag flags bits 0-2: 0 = short ADs, 1 = long ADs, 2 = extended
    /// ADs (unsupported), 3 = embedded data.
    ad_type: u8,
    /// The raw allocation-descriptor area (or embedded data, for ad_type 3).
    ads: Vec<u8>,
    /// The partition the File Entry itself lives in -- short ADs are
    /// relative to it.
    home_pref: u16,
}

/// A read-only UDF volume over any `Read + Seek` source.
///
/// The source sits behind a `Mutex` so [`UdfFileReader`]s can share it with
/// `&self` metadata operations; every access seeks absolutely, so the lock
/// only serializes, never coordinates state.
pub struct UdfVolume<R> {
    reader: Mutex<R>,
    total_len: u64,
    /// Absolute byte offset of each mapped partition's start, indexed by
    /// partition reference number (the LVD's partition-map order).
    partition_starts: Vec<u64>,
    root_icb: IcbRef,
}

impl<R: Read + Seek> UdfVolume<R> {
    /// Opens `reader` as a UDF volume: verifies the volume recognition
    /// sequence, locates the anchor, walks the volume descriptor sequence,
    /// and resolves the root directory's ICB. Returns [`UdfError::NotUdf`]
    /// (fall back to ISO9660) when the image simply isn't UDF.
    pub fn open(mut reader: R) -> Result<Self> {
        let total_len = reader.seek(SeekFrom::End(0)).map_err(UdfError::Io)?;
        if total_len < 257 * BLOCK {
            // Too small to hold even the mandatory anchor at block 256.
            return Err(UdfError::NotUdf);
        }
        let mut volume = UdfVolume {
            reader: Mutex::new(reader),
            total_len,
            partition_starts: Vec::new(),
            root_icb: IcbRef {
                block: 0,
                partition_ref: 0,
            },
        };

        if !volume.has_nsr_descriptor()? {
            return Err(UdfError::NotUdf);
        }

        let (main_vds, reserve_vds) = volume.find_anchor()?;
        let vds = match volume.parse_vds(main_vds) {
            Ok(vds) => vds,
            // The reserve sequence exists exactly for a damaged main one.
            Err(UdfError::Io(err)) => return Err(UdfError::Io(err)),
            Err(_) => volume.parse_vds(reserve_vds)?,
        };
        volume.partition_starts = vds.partition_starts;

        let fsd_block = volume.read_block_at(volume.resolve(&vds.fsd_icb)?)?;
        let fsd_tag = parse_tag(&fsd_block)?;
        if fsd_tag.id != TAG_FILE_SET {
            return Err(UdfError::Corrupt(format!(
                "expected a file set descriptor, found tag {}",
                fsd_tag.id
            )));
        }
        let (_, _, root_icb) = parse_long_ad(&fsd_block[400..416]);
        volume.root_icb = root_icb;
        Ok(volume)
    }

    /// Lists the root directory.
    pub fn root_dir(&self) -> Result<UdfDir> {
        let root = self.root_icb.clone();
        self.read_directory(&root)
    }

    /// Lists the directory behind `icb` (from a [`UdfDirEntry`] whose
    /// `is_dir()` is true).
    pub fn read_directory(&self, icb: &IcbRef) -> Result<UdfDir> {
        let meta = self.read_file_meta(icb)?;
        if meta.file_type != FILE_TYPE_DIRECTORY {
            return Err(UdfError::Corrupt(
                "ICB expected to be a directory is not one".into(),
            ));
        }
        if meta.info_len > MAX_DIR_BYTES {
            return Err(UdfError::Corrupt(format!(
                "directory data of {} bytes exceeds the {} byte sanity cap",
                meta.info_len, MAX_DIR_BYTES
            )));
        }
        let data = match self.file_data(&meta)? {
            FileData::Inline(bytes) => bytes,
            FileData::Extents(extents) => {
                let mut buf = vec![0u8; meta.info_len as usize];
                read_extents_into(self, &extents, 0, &mut buf)?;
                buf
            }
        };
        self.parse_fids(&data)
    }

    /// Returns a streaming reader over one file's content. Constant memory:
    /// only the file's extent list is materialized (embedded-data files --
    /// at most one block -- are held inline).
    pub fn open_file(&self, entry: &UdfDirEntry) -> Result<UdfFileReader<'_, R>> {
        let meta = self.read_file_meta(&entry.icb)?;
        if meta.file_type == FILE_TYPE_DIRECTORY {
            return Err(UdfError::Corrupt(
                "attempted to open a directory as a file".into(),
            ));
        }
        let data = self.file_data(&meta)?;
        Ok(UdfFileReader {
            volume: self,
            data,
            len: meta.info_len,
            pos: 0,
        })
    }

    // -- volume structure parsing --

    /// Scans the volume recognition sequence (byte 32768 onward) for an
    /// `NSR02`/`NSR03` descriptor -- ECMA-167's own "this is UDF" marker. A
    /// bridge disc interleaves these with ISO9660's `CD001` descriptors.
    fn has_nsr_descriptor(&self) -> Result<bool> {
        for i in 0..MAX_VSD_SCAN {
            let offset = (16 + i) * BLOCK;
            if offset + BLOCK > self.total_len {
                return Ok(false);
            }
            let mut header = [0u8; 7];
            self.read_at(offset, &mut header)?;
            match &header[1..6] {
                b"NSR02" | b"NSR03" => return Ok(true),
                b"BEA01" | b"BOOT2" | b"CD001" | b"CDW02" => continue,
                // TEA01 terminates the sequence; anything else means we ran
                // off the end of it without an NSR.
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    /// Locates the anchor volume descriptor pointer and returns the (main,
    /// reserve) volume descriptor sequence extents as `(location_block,
    /// length_bytes)` pairs. Block 256 is mandatory; N-256 and N-1 are the
    /// spec's fallback positions for a damaged block 256.
    fn find_anchor(&self) -> Result<((u32, u32), (u32, u32))> {
        let last_block = (self.total_len / BLOCK).saturating_sub(1);
        let candidates = [256, last_block.saturating_sub(256), last_block];
        for &candidate in &candidates {
            if candidate < 256 {
                continue;
            }
            let block = match self.read_block_at(candidate * BLOCK) {
                Ok(block) => block,
                Err(UdfError::Io(err)) => return Err(UdfError::Io(err)),
                Err(_) => continue,
            };
            let Ok(tag) = parse_tag(&block) else { continue };
            if tag.id != TAG_ANCHOR {
                continue;
            }
            let main_len = u32::from_le_bytes([block[16], block[17], block[18], block[19]]);
            let main_loc = u32::from_le_bytes([block[20], block[21], block[22], block[23]]);
            let reserve_len = u32::from_le_bytes([block[24], block[25], block[26], block[27]]);
            let reserve_loc = u32::from_le_bytes([block[28], block[29], block[30], block[31]]);
            return Ok(((main_loc, main_len), (reserve_loc, reserve_len)));
        }
        Err(UdfError::NotUdf)
    }

    fn parse_vds(&self, extent: (u32, u32)) -> Result<ParsedVds> {
        // Prevailing-descriptor rule (ECMA-167 3/8.4.3): the highest volume
        // descriptor sequence number of each kind wins.
        let mut lvd: Option<(u32, Vec<u8>)> = None;
        let mut partitions: Vec<(u32, u16, u64)> = Vec::new(); // (seq, number, start_block)
        let (mut location, mut length) = extent;
        let mut blocks_walked: u32 = 0;

        'sequence: loop {
            let block_count = length.div_ceil(BLOCK as u32);
            for i in 0..block_count {
                blocks_walked += 1;
                if blocks_walked > MAX_VDS_BLOCKS {
                    return Err(UdfError::Corrupt(
                        "volume descriptor sequence exceeds the walk limit".into(),
                    ));
                }
                let block = self.read_block_at(u64::from(location + i) * BLOCK)?;
                if block[..16].iter().all(|&b| b == 0) {
                    break 'sequence; // unterminated but blank-ended sequence
                }
                let tag = parse_tag(&block)?;
                let seq = u32::from_le_bytes([block[16], block[17], block[18], block[19]]);
                match tag.id {
                    TAG_TERMINATOR => break 'sequence,
                    TAG_VDS_POINTER => {
                        length = u32::from_le_bytes([block[20], block[21], block[22], block[23]]);
                        location = u32::from_le_bytes([block[24], block[25], block[26], block[27]]);
                        continue 'sequence;
                    }
                    TAG_PARTITION => {
                        let number = u16::from_le_bytes([block[22], block[23]]);
                        let start =
                            u32::from_le_bytes([block[188], block[189], block[190], block[191]]);
                        partitions.push((seq, number, u64::from(start)));
                    }
                    TAG_LOGICAL_VOLUME if lvd.as_ref().is_none_or(|(prev, _)| seq >= *prev) => {
                        lvd = Some((seq, block.to_vec()));
                    }
                    _ => {} // PVD, IUVD, USD: not needed for reading files
                }
            }
            break;
        }

        let (_, lvd) = lvd.ok_or_else(|| {
            UdfError::Corrupt("volume descriptor sequence has no logical volume descriptor".into())
        })?;
        let block_size = u32::from_le_bytes([lvd[212], lvd[213], lvd[214], lvd[215]]);
        if u64::from(block_size) != BLOCK {
            return Err(UdfError::Unsupported(format!(
                "logical block size {block_size} (only 2048 is supported)"
            )));
        }
        let (_, _, fsd_icb) = parse_long_ad(&lvd[248..264]);
        let map_table_len =
            usize::try_from(u32::from_le_bytes([lvd[264], lvd[265], lvd[266], lvd[267]]))
                .map_err(|_| UdfError::Corrupt("partition map table length overflows".into()))?;
        let map_count = u32::from_le_bytes([lvd[268], lvd[269], lvd[270], lvd[271]]);

        let maps_end = 440usize
            .checked_add(map_table_len)
            .filter(|&end| end <= lvd.len())
            .ok_or_else(|| {
                UdfError::Corrupt("partition map table runs past its descriptor".into())
            })?;
        let mut partition_starts = Vec::new();
        let mut offset = 440;
        for _ in 0..map_count {
            if offset + 2 > maps_end {
                return Err(UdfError::Corrupt(
                    "partition map table shorter than its own count".into(),
                ));
            }
            let map_type = lvd[offset];
            let map_len = usize::from(lvd[offset + 1]);
            if map_len < 2 || offset + map_len > maps_end {
                return Err(UdfError::Corrupt("malformed partition map entry".into()));
            }
            match map_type {
                1 => {
                    let number = u16::from_le_bytes([lvd[offset + 4], lvd[offset + 5]]);
                    // Resolve through the prevailing partition descriptor
                    // with that partition number.
                    let start = partitions
                        .iter()
                        .filter(|(_, n, _)| *n == number)
                        .max_by_key(|(seq, _, _)| *seq)
                        .map(|(_, _, start)| *start)
                        .ok_or_else(|| {
                            UdfError::Corrupt(format!(
                                "partition map references partition number {number}, which has no \
                                 partition descriptor"
                            ))
                        })?;
                    partition_starts.push(start * BLOCK);
                }
                2 => {
                    return Err(UdfError::Unsupported(
                        "type-2 (virtual/sparable/metadata) partition maps".into(),
                    ))
                }
                other => {
                    return Err(UdfError::Corrupt(format!(
                        "unknown partition map type {other}"
                    )))
                }
            }
            offset += map_len;
        }
        if partition_starts.is_empty() {
            return Err(UdfError::Corrupt(
                "logical volume descriptor maps no partitions".into(),
            ));
        }
        Ok(ParsedVds {
            partition_starts,
            fsd_icb,
        })
    }

    // -- file structure parsing --

    fn read_file_meta(&self, icb: &IcbRef) -> Result<FileMeta> {
        let block = self.read_block_at(self.resolve(icb)?)?;
        let tag = parse_tag(&block)?;
        let (l_ea_off, l_ad_off, ads_start): (usize, usize, usize) = match tag.id {
            TAG_FILE_ENTRY => (168, 172, 176),
            TAG_EXTENDED_FILE_ENTRY => (208, 212, 216),
            TAG_INDIRECT_ENTRY => {
                // Strategy trees whose *first* block is an indirect entry
                // are outside scope; strategy-4096 ICBs put the direct File
                // Entry first, which the branch below handles.
                return Err(UdfError::Unsupported(
                    "ICB whose first entry is an indirect entry".into(),
                ));
            }
            other => {
                return Err(UdfError::Corrupt(format!(
                    "expected a file entry at the ICB, found tag {other}"
                )))
            }
        };

        let strategy = u16::from_le_bytes([block[20], block[21]]);
        if !matches!(strategy, 4 | 4096) {
            return Err(UdfError::Unsupported(format!("ICB strategy {strategy}")));
        }
        let file_type = block[27];
        let ad_type = (u16::from_le_bytes([block[34], block[35]]) & 0x7) as u8;
        let info_len = u64::from_le_bytes(block[56..64].try_into().unwrap());

        let l_ea = u32::from_le_bytes(block[l_ea_off..l_ea_off + 4].try_into().unwrap()) as usize;
        let l_ad = u32::from_le_bytes(block[l_ad_off..l_ad_off + 4].try_into().unwrap()) as usize;
        let ads_off = ads_start
            .checked_add(l_ea)
            .filter(|&off| off.checked_add(l_ad).is_some_and(|end| end <= block.len()))
            .ok_or_else(|| {
                UdfError::Corrupt("allocation descriptors run past the file entry".into())
            })?;

        Ok(FileMeta {
            file_type,
            info_len,
            ad_type,
            ads: block[ads_off..ads_off + l_ad].to_vec(),
            home_pref: icb.partition_ref,
        })
    }

    fn file_data(&self, meta: &FileMeta) -> Result<FileData> {
        match meta.ad_type {
            3 => {
                // Embedded data: the AD area *is* the content.
                let len = usize::try_from(meta.info_len)
                    .ok()
                    .filter(|&len| len <= meta.ads.len())
                    .ok_or_else(|| {
                        UdfError::Corrupt(
                            "embedded data shorter than the information length".into(),
                        )
                    })?;
                Ok(FileData::Inline(meta.ads[..len].to_vec()))
            }
            ad_type @ (0 | 1) => {
                let extents =
                    self.parse_extents(&meta.ads, ad_type, meta.info_len, meta.home_pref)?;
                Ok(FileData::Extents(extents))
            }
            2 => Err(UdfError::Unsupported(
                "extended allocation descriptors".into(),
            )),
            other => Err(UdfError::Corrupt(format!(
                "reserved allocation descriptor type {other}"
            ))),
        }
    }

    /// Walks an allocation-descriptor list (short or long form), following
    /// continuation (allocation extent) blocks, into a resolved extent list
    /// totalling exactly `info_len` bytes.
    fn parse_extents(
        &self,
        ads: &[u8],
        ad_type: u8,
        info_len: u64,
        home_pref: u16,
    ) -> Result<Vec<Extent>> {
        let entry_size = if ad_type == 0 { 8 } else { 16 };
        let mut extents = Vec::new();
        let mut remaining = info_len;
        let mut ads = ads.to_vec();
        let mut chain = 0usize;

        'list: while remaining > 0 {
            let mut offset = 0;
            while offset + entry_size <= ads.len() && remaining > 0 {
                let len_raw = u32::from_le_bytes(ads[offset..offset + 4].try_into().unwrap());
                let extent_kind = len_raw >> 30;
                let extent_len = u64::from(len_raw & 0x3FFF_FFFF);
                if extent_len == 0 {
                    break 'list;
                }
                let block = u32::from_le_bytes(ads[offset + 4..offset + 8].try_into().unwrap());
                let pref = if ad_type == 0 {
                    home_pref
                } else {
                    u16::from_le_bytes(ads[offset + 8..offset + 10].try_into().unwrap())
                };
                offset += entry_size;

                match extent_kind {
                    0 => {
                        let start = self.resolve(&IcbRef {
                            block,
                            partition_ref: pref,
                        })?;
                        let len = extent_len.min(remaining);
                        if start
                            .checked_add(len)
                            .is_none_or(|end| end > self.total_len)
                        {
                            return Err(UdfError::Corrupt(
                                "file extent runs past the end of the image".into(),
                            ));
                        }
                        extents.push(Extent {
                            image_offset: Some(start),
                            len,
                        });
                        remaining -= len;
                    }
                    // Allocated-unrecorded and unallocated extents both read
                    // as zeros (sparse file regions).
                    1 | 2 => {
                        let len = extent_len.min(remaining);
                        extents.push(Extent {
                            image_offset: None,
                            len,
                        });
                        remaining -= len;
                    }
                    3 => {
                        // Continuation: the next chunk of ADs lives in an
                        // allocation extent descriptor block.
                        chain += 1;
                        if chain > MAX_AED_CHAIN {
                            return Err(UdfError::Corrupt(
                                "allocation-extent continuation chain exceeds the limit".into(),
                            ));
                        }
                        let aed = self.read_block_at(self.resolve(&IcbRef {
                            block,
                            partition_ref: pref,
                        })?)?;
                        let tag = parse_tag(&aed)?;
                        if tag.id != TAG_ALLOCATION_EXTENT {
                            return Err(UdfError::Corrupt(format!(
                                "expected an allocation extent descriptor, found tag {}",
                                tag.id
                            )));
                        }
                        let l_ad =
                            u32::from_le_bytes([aed[20], aed[21], aed[22], aed[23]]) as usize;
                        if 24 + l_ad > aed.len() {
                            return Err(UdfError::Corrupt(
                                "allocation extent descriptor overflows its block".into(),
                            ));
                        }
                        ads = aed[24..24 + l_ad].to_vec();
                        continue 'list;
                    }
                    _ => unreachable!("2-bit extent kind"),
                }
            }
            break;
        }

        if remaining > 0 {
            return Err(UdfError::Corrupt(
                "allocation descriptors cover less than the file's information length".into(),
            ));
        }
        Ok(extents)
    }

    fn parse_fids(&self, data: &[u8]) -> Result<UdfDir> {
        let mut entries = Vec::new();
        let mut pos = 0usize;
        while pos + 38 <= data.len() {
            if data[pos..pos + 16].iter().all(|&b| b == 0) {
                break; // padding after the last FID
            }
            let fid = &data[pos..];
            let tag = parse_tag(fid)?;
            if tag.id != TAG_FILE_IDENTIFIER {
                return Err(UdfError::Corrupt(format!(
                    "expected a file identifier descriptor, found tag {}",
                    tag.id
                )));
            }
            let characteristics = fid[18];
            let l_fi = usize::from(fid[19]);
            let (_, _, icb) = parse_long_ad(&fid[20..36]);
            let l_iu = usize::from(u16::from_le_bytes([fid[36], fid[37]]));
            let total = 38 + l_iu + l_fi;
            let padded = total.div_ceil(4) * 4;
            if pos + padded > data.len() {
                return Err(UdfError::Corrupt(
                    "file identifier descriptor overflows its directory".into(),
                ));
            }

            let is_parent = characteristics & 0x08 != 0;
            let is_deleted = characteristics & 0x04 != 0;
            if !is_parent && !is_deleted && l_fi > 0 {
                let name = decode_dstring(&fid[38 + l_iu..38 + l_iu + l_fi])?;
                let is_dir = characteristics & 0x02 != 0;
                // The FID doesn't carry the file's size; its File Entry does.
                let size = if is_dir {
                    0
                } else {
                    self.read_file_meta(&icb)?.info_len
                };
                entries.push(UdfDirEntry {
                    name,
                    is_dir,
                    size,
                    icb,
                });
            }
            pos += padded;
        }
        Ok(UdfDir { entries })
    }

    // -- raw access --

    /// Absolute byte offset of `icb`'s block in the underlying image.
    fn resolve(&self, icb: &IcbRef) -> Result<u64> {
        let start = self
            .partition_starts
            .get(usize::from(icb.partition_ref))
            .copied()
            .ok_or_else(|| {
                UdfError::Corrupt(format!(
                    "reference to unmapped partition {}",
                    icb.partition_ref
                ))
            })?;
        Ok(start + u64::from(icb.block) * BLOCK)
    }

    fn read_block_at(&self, offset: u64) -> Result<[u8; BLOCK as usize]> {
        let mut block = [0u8; BLOCK as usize];
        self.read_at(offset, &mut block)?;
        Ok(block)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut reader = self.reader.lock().unwrap_or_else(|p| p.into_inner());
        reader.seek(SeekFrom::Start(offset)).map_err(UdfError::Io)?;
        reader.read_exact(buf)?;
        Ok(())
    }
}

struct ParsedVds {
    partition_starts: Vec<u64>,
    fsd_icb: IcbRef,
}

/// Copies bytes `[start, start + buf.len())` of the file described by
/// `extents` into `buf`. Shared by the streaming reader and directory
/// materialization.
fn read_extents_into<R: Read + Seek>(
    volume: &UdfVolume<R>,
    extents: &[Extent],
    start: u64,
    buf: &mut [u8],
) -> Result<()> {
    let mut pos = start;
    let mut filled = 0usize;
    while filled < buf.len() {
        let mut offset_in_extent = pos;
        let mut extent = None;
        for candidate in extents {
            if offset_in_extent < candidate.len {
                extent = Some(candidate);
                break;
            }
            offset_in_extent -= candidate.len;
        }
        let Some(extent) = extent else {
            return Err(UdfError::Corrupt(
                "read past the file's mapped extents".into(),
            ));
        };
        let n = usize::try_from((extent.len - offset_in_extent).min((buf.len() - filled) as u64))
            .expect("chunk bounded by buf.len(), which fits usize");
        match extent.image_offset {
            Some(base) => volume.read_at(base + offset_in_extent, &mut buf[filled..filled + n])?,
            None => buf[filled..filled + n].fill(0),
        }
        filled += n;
        pos += n as u64;
    }
    Ok(())
}

/// Decodes an OSTA CS0 compressed-unicode d-string (UDF 2.1.1): a
/// compression-ID byte (8 or 16) followed by the code units.
fn decode_dstring(bytes: &[u8]) -> Result<String> {
    let Some((&compression, rest)) = bytes.split_first() else {
        return Ok(String::new());
    };
    match compression {
        8 => Ok(rest.iter().map(|&b| char::from(b)).collect()),
        16 => {
            if !rest.len().is_multiple_of(2) {
                return Err(UdfError::Corrupt(
                    "16-bit compressed name with an odd byte count".into(),
                ));
            }
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            // Unpaired surrogates become U+FFFD rather than failing the
            // whole listing -- the name is still displayable and matchable.
            Ok(char::decode_utf16(units)
                .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect())
        }
        other => Err(UdfError::Unsupported(format!(
            "d-string compression ID {other}"
        ))),
    }
}

/// Streaming reader over one file's content -- `Read + Seek`, constant
/// memory. Reads lock the volume's shared source per chunk, so interleaved
/// metadata reads on the same volume stay safe.
pub struct UdfFileReader<'a, R> {
    volume: &'a UdfVolume<R>,
    data: FileData,
    len: u64,
    pos: u64,
}

impl<R: Read + Seek> Read for UdfFileReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from((self.len - self.pos).min(buf.len() as u64))
            .expect("chunk bounded by buf.len(), which fits usize");
        match &self.data {
            FileData::Inline(bytes) => {
                let start = self.pos as usize;
                buf[..n].copy_from_slice(&bytes[start..start + n]);
            }
            FileData::Extents(extents) => {
                read_extents_into(self.volume, extents, self.pos, &mut buf[..n])
                    .map_err(UdfError::into_io)?;
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for UdfFileReader<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        // Seeking past the end is allowed (reads there return 0), matching
        // `std::fs::File`; seeking before the start is an error, same as it
        // is there.
        let target = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        self.pos = target;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hadris_udf::write::{SimpleDir, SimpleFile, UdfWriteOptions, UdfWriter};
    use std::io::Cursor;

    /// Builds a UDF image with `hadris-udf`'s writer -- an independent
    /// implementation serving as this module's test oracle.
    fn build_image(root: &SimpleDir) -> Vec<u8> {
        UdfWriter::create(Cursor::new(Vec::new()), root, UdfWriteOptions::default())
            .expect("fixture build should succeed")
            .into_inner()
            .into_inner()
    }

    fn windows_shaped_image() -> Vec<u8> {
        let mut root = SimpleDir::new("");
        root.add_file(SimpleFile::new("bootmgr", b"fixture bootmgr".to_vec()));
        let mut sources = SimpleDir::new("sources");
        sources.add_file(SimpleFile::new("boot.wim", b"fixture boot.wim".to_vec()));
        root.add_dir(sources);
        build_image(&root)
    }

    #[test]
    fn opens_a_udf_image_and_lists_the_root() {
        let volume = UdfVolume::open(Cursor::new(windows_shaped_image())).unwrap();
        let root = volume.root_dir().unwrap();
        let mut names: Vec<_> = root.entries().map(|e| e.name().to_string()).collect();
        names.sort();
        assert_eq!(names, ["bootmgr", "sources"]);

        let bootmgr = root.entries().find(|e| e.name() == "bootmgr").unwrap();
        assert!(bootmgr.is_file());
        assert_eq!(bootmgr.size, 15);
        let sources = root.entries().find(|e| e.name() == "sources").unwrap();
        assert!(sources.is_dir());
    }

    #[test]
    fn descends_into_a_subdirectory() {
        let volume = UdfVolume::open(Cursor::new(windows_shaped_image())).unwrap();
        let root = volume.root_dir().unwrap();
        let sources = root.entries().find(|e| e.name() == "sources").unwrap();
        let dir = volume.read_directory(&sources.icb).unwrap();
        let entries: Vec<_> = dir.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "boot.wim");
        assert_eq!(entries[0].size, 16);
    }

    #[test]
    fn reads_file_content_exactly() {
        let volume = UdfVolume::open(Cursor::new(windows_shaped_image())).unwrap();
        let root = volume.root_dir().unwrap();
        let bootmgr = root.entries().find(|e| e.name() == "bootmgr").unwrap();
        let mut content = Vec::new();
        volume
            .open_file(bootmgr)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, b"fixture bootmgr");
    }

    #[test]
    fn streams_a_multi_block_file_in_odd_sized_chunks() {
        // Bigger than one 2048-byte block, and a content pattern where any
        // off-by-one in extent arithmetic changes the bytes read.
        let payload: Vec<u8> = (0u32..100_000).map(|i| (i * 7 % 251) as u8).collect();
        let mut root = SimpleDir::new("");
        root.add_file(SimpleFile::new("big.bin", payload.clone()));
        let volume = UdfVolume::open(Cursor::new(build_image(&root))).unwrap();

        let dir = volume.root_dir().unwrap();
        let entry = dir.entries().find(|e| e.name() == "big.bin").unwrap();
        assert_eq!(entry.size, payload.len() as u64);

        let mut reader = volume.open_file(entry).unwrap();
        let mut collected = Vec::new();
        let mut chunk = [0u8; 7919]; // prime-sized chunks cross block edges
        loop {
            let n = reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(collected, payload);
    }

    #[test]
    fn seek_then_read_returns_the_right_window() {
        let payload: Vec<u8> = (0u32..50_000).map(|i| (i % 256) as u8).collect();
        let mut root = SimpleDir::new("");
        root.add_file(SimpleFile::new("big.bin", payload.clone()));
        let volume = UdfVolume::open(Cursor::new(build_image(&root))).unwrap();
        let dir = volume.root_dir().unwrap();
        let entry = dir.entries().find(|e| e.name() == "big.bin").unwrap();

        let mut reader = volume.open_file(entry).unwrap();
        reader.seek(SeekFrom::Start(4090)).unwrap(); // straddles block 2->3
        let mut window = [0u8; 16];
        reader.read_exact(&mut window).unwrap();
        assert_eq!(window[..], payload[4090..4106]);

        reader.seek(SeekFrom::End(-4)).unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, payload[payload.len() - 4..]);

        // Past the end: reads return 0 rather than erroring.
        reader
            .seek(SeekFrom::Start(payload.len() as u64 + 10))
            .unwrap();
        assert_eq!(reader.read(&mut window).unwrap(), 0);
    }

    #[test]
    fn rejects_non_udf_input() {
        assert!(matches!(
            UdfVolume::open(Cursor::new(vec![0u8; 4096])),
            Err(UdfError::NotUdf)
        ));
        // Large but structureless: no VRS, no anchor.
        assert!(matches!(
            UdfVolume::open(Cursor::new(vec![0u8; 600 * 2048])),
            Err(UdfError::NotUdf)
        ));
    }

    #[test]
    fn corrupting_a_descriptor_is_detected_not_misparsed() {
        let mut image = windows_shaped_image();
        // Find the file set descriptor (tag 256) by scanning block starts,
        // then flip one byte inside its CRC-covered area.
        let fsd_offset = (0..image.len() / 2048)
            .map(|block| block * 2048)
            .find(|&offset| {
                u16::from_le_bytes([image[offset], image[offset + 1]]) == TAG_FILE_SET
                    && parse_tag(&image[offset..offset + 2048]).is_ok()
            })
            .expect("fixture should contain a file set descriptor");
        image[fsd_offset + 100] ^= 0xFF;
        let Err(err) = UdfVolume::open(Cursor::new(image)) else {
            panic!("corrupted FSD should not open");
        };
        assert!(matches!(err, UdfError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn crc16_matches_the_itu_t_reference_value() {
        // CRC-16/CCITT (init 0x0000) of "123456789" is 0x31C3 -- the
        // standard check value for this polynomial/init combination.
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn decodes_8_and_16_bit_dstrings() {
        assert_eq!(decode_dstring(&[8, b'a', b'b']).unwrap(), "ab");
        let utf16: Vec<u8> = [0x00e9u16, 0x0041] // "éA"
            .iter()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        let mut bytes = vec![16u8];
        bytes.extend(utf16);
        assert_eq!(decode_dstring(&bytes).unwrap(), "éA");
        assert!(matches!(
            decode_dstring(&[9, 0, 0]),
            Err(UdfError::Unsupported(_))
        ));
    }

    #[test]
    fn sparse_and_multi_extent_layouts_read_correctly() {
        // parse_extents + read_extents_into exercised against a hand-built
        // extent list: recorded / sparse / recorded. The volume only serves
        // the recorded regions; the sparse middle must come back as zeros.
        let image = windows_shaped_image();
        let volume = UdfVolume::open(Cursor::new(image)).unwrap();
        let extents = [
            Extent {
                image_offset: Some(0),
                len: 10,
            },
            Extent {
                image_offset: None,
                len: 5,
            },
            Extent {
                image_offset: Some(2048),
                len: 10,
            },
        ];
        let mut buf = [0xAAu8; 25];
        read_extents_into(&volume, &extents, 0, &mut buf).unwrap();
        assert_eq!(buf[10..15], [0, 0, 0, 0, 0]);
        // A window straddling the sparse hole:
        let mut window = [0u8; 10];
        read_extents_into(&volume, &extents, 8, &mut window).unwrap();
        assert_eq!(window[2..7], [0, 0, 0, 0, 0]);
    }
}
