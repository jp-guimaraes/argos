//! Detects a Windows installer ISO and provides a thin, read-only wrapper over
//! its file tree -- the counterpart to `image::isohybrid` for the phase 2
//! (Windows ISO support) backlog: [`docs/architecture.md`](../../../../../docs/architecture.md)
//! records the guiding decisions this module implements (Linux-first, and
//! UEFI:NTFS rather than `install.wim` splitting).
//!
//! Unlike an isohybrid Linux image, a Windows installation ISO carries no
//! embedded MBR/GPT at all. It also, in practice, is **not** a plain ISO9660
//! filesystem: real official Windows 10/11 media is mastered as a UDF
//! bridge (ISO9660 + UDF, ECMA-167) -- confirmed against a real Microsoft
//! Windows 10 22H2 ISO during W1's own validation, contradicting Argos's
//! initial phase 2 planning, which treated UDF as a rare edge case for
//! unusually large multi-edition images rather than the norm. The plain
//! ISO9660 layer such a bridge carries exposes only a tiny stub (a
//! `README.TXT` pointing UEFI:NTFS-less systems at Microsoft's site); the
//! real `bootmgr`/`sources` tree lives in the UDF layer only. So this module
//! tries [`image::udf`](super::udf) (Argos's own read-only, streaming
//! UDF/ECMA-167 reader -- see its module docs for why it replaced the
//! `hadris-udf` crate) first, falling through to [`cdfs`] (pure-Rust
//! ISO9660/ECMA-119, under the local dependency name `cdfs`, backed by the
//! `newtua-cdfs` fork -- see `Cargo.toml` for why) only for genuinely
//! ISO9660-only Windows-shaped images, including this module's own synthetic
//! test fixtures.
//!
//! Detection looks for the same two paths every official Windows 10/11
//! install media has always shipped: `bootmgr` and `sources/boot.wim` at the
//! root. `sources/install.wim` (or `.esd`) is what actually gets copied onto
//! the NTFS partition in W3, but it is *not* part of detection: its absence
//! (some multi-edition or trimmed media rename or omit it) says nothing about
//! whether this is a Windows installer, whereas `bootmgr` and `boot.wim`
//! missing does.

use super::udf;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const BOOTMGR_PATH: &str = "bootmgr";
const BOOT_WIM_PATH: &str = "sources/boot.wim";

/// The result of probing an image for the Windows-installer shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsClassification {
    pub is_windows_installer: bool,
}

impl WindowsClassification {
    /// The only question the write pipeline asks: does this look enough like
    /// an official Windows installer image to attempt the UEFI:NTFS write
    /// path (`WindowsPartitionPlan`, landing in W2) at all?
    pub fn is_windows_installer_iso(&self) -> bool {
        self.is_windows_installer
    }
}

/// Classifies the image at `path` as a Windows installer or not.
pub fn classify(path: &Path) -> io::Result<WindowsClassification> {
    let file = File::open(path)?;
    classify_reader(file)
}

/// Same as [`classify`], but against any `Read + Seek` source -- lets tests
/// exercise this against small in-memory fixtures instead of real files.
pub fn classify_reader<R: Read + Seek>(mut reader: R) -> io::Result<WindowsClassification> {
    // Tries UDF first (real Windows media), falling back to ISO9660 (this
    // module's own synthetic fixtures, and any genuinely ISO9660-only
    // Windows-shaped image) -- see this module's top doc comment for why.
    // `UdfVolume::open` takes its reader by value, so it's given a temporary
    // `&mut` borrow here rather than `reader` itself: on failure, `reader` is
    // still ours to rewind and retry with cdfs.
    match udf::UdfVolume::open(&mut reader) {
        Ok(udf) => {
            let has_bootmgr = udf_is_file_at(&udf, BOOTMGR_PATH)?;
            let has_boot_wim = has_bootmgr && udf_is_file_at(&udf, BOOT_WIM_PATH)?;
            return Ok(WindowsClassification {
                is_windows_installer: has_bootmgr && has_boot_wim,
            });
        }
        // A real I/O failure is Argos's problem and must propagate; any
        // other failure just means "not (usable) UDF" -- fall through to
        // ISO9660 exactly as before.
        Err(udf::UdfError::Io(err)) => return Err(err),
        Err(_) => {}
    }

    reader.seek(SeekFrom::Start(0))?;
    let iso = match cdfs::ISO9660::new(reader) {
        Ok(iso) => iso,
        // A real I/O failure (short read, broken pipe, ...) is Argos's
        // problem and must propagate. Anything else means the image simply
        // isn't a (supported) ISO9660 filesystem at all -- exactly as
        // unrecognizable as one that's ISO9660 but lacks the paths below, so
        // it's reported the same way: not a Windows installer.
        Err(cdfs::ISOError::Io(err)) => return Err(err),
        Err(_) => {
            return Ok(WindowsClassification {
                is_windows_installer: false,
            })
        }
    };

    let has_bootmgr = is_file_at(&iso, BOOTMGR_PATH)?;
    let has_boot_wim = has_bootmgr && is_file_at(&iso, BOOT_WIM_PATH)?;

    Ok(WindowsClassification {
        is_windows_installer: has_bootmgr && has_boot_wim,
    })
}

fn is_file_at<T: cdfs::ISO9660Reader>(iso: &cdfs::ISO9660<T>, path: &str) -> io::Result<bool> {
    match iso.open(path) {
        Ok(Some(cdfs::DirectoryEntry::File(_))) => Ok(true),
        Ok(_) => Ok(false),
        Err(cdfs::ISOError::Io(err)) => Err(err),
        Err(_) => Ok(false),
    }
}

fn udf_is_file_at<T: Read + Seek>(udf: &udf::UdfVolume<T>, path: &str) -> io::Result<bool> {
    match udf_find(udf, path).map_err(udf::UdfError::into_io)? {
        Some(entry) => Ok(entry.is_file()),
        None => Ok(false),
    }
}

/// Descends `path` (`/`-separated, case-insensitive -- real Windows UDF
/// media has been observed using both `bootmgr` and `BOOTMGR`) from `udf`'s
/// root directory, the UDF counterpart to `cdfs::ISODirectory::find_recursive`.
fn udf_find<T: Read + Seek>(
    udf: &udf::UdfVolume<T>,
    path: &str,
) -> Result<Option<udf::UdfDirEntry>, udf::UdfError> {
    let mut dir = udf.root_dir()?;
    let mut segments = path.split('/').filter(|s| !s.is_empty()).peekable();

    while let Some(segment) = segments.next() {
        let Some(entry) = dir
            .entries()
            .find(|e| e.name().eq_ignore_ascii_case(segment))
            .cloned()
        else {
            return Ok(None);
        };

        if segments.peek().is_none() {
            return Ok(Some(entry));
        }
        if !entry.is_dir() {
            return Ok(None);
        }
        dir = udf.read_directory(&entry.icb)?;
    }

    // Empty path: the root directory itself was asked for, which is never a
    // file -- not reachable via this module's own callers, but Ok(None) is
    // the honest answer rather than panicking on an empty entries() lookup.
    Ok(None)
}

/// One regular file found while walking a [`WindowsIso`]'s tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoFileEntry {
    /// Path relative to the image root, `/`-separated (e.g. `"sources/boot.wim"`).
    pub path: String,
    pub size: u64,
}

/// A thin, read-only wrapper over a Windows installer image's file tree.
///
/// This is deliberately dumb: it lists files and hands back a reader for one
/// at a time. W3 is what actually copies bytes onto a mounted NTFS partition
/// (hashing each file as it goes); nothing here touches a disk.
pub struct WindowsIso {
    backing: Backing,
}

enum Backing {
    // Plain `File`, not `BufReader<File>`: every access seeks absolutely
    // (metadata is random-access, content reads come in large chunks), so a
    // read buffer would be discarded on every seek and help nothing.
    Udf(udf::UdfVolume<File>),
    Iso9660(Box<cdfs::ISO9660<File>>),
}

impl WindowsIso {
    pub fn open(path: &Path) -> io::Result<Self> {
        // Same UDF-first, ISO9660-fallback order as classify_reader, and for
        // the same reason -- see this module's top doc comment.
        match udf::UdfVolume::open(File::open(path)?) {
            Ok(udf) => {
                return Ok(Self {
                    backing: Backing::Udf(udf),
                })
            }
            Err(udf::UdfError::Io(err)) => return Err(err),
            Err(_) => {}
        }
        let iso = cdfs::ISO9660::new(File::open(path)?).map_err(cdfs_err_to_io)?;
        Ok(Self {
            backing: Backing::Iso9660(Box::new(iso)),
        })
    }

    /// Recursively lists every regular file in the image, depth-first.
    /// Symlinks are skipped (Windows install media has none in either its
    /// UDF or ISO9660 layer) rather than followed or reported as an error.
    pub fn list_files(&self) -> io::Result<Vec<IsoFileEntry>> {
        let mut out = Vec::new();
        match &self.backing {
            Backing::Udf(udf) => {
                let root = udf.root_dir().map_err(udf::UdfError::into_io)?;
                walk_udf(udf, &root, "", &mut out)?;
            }
            Backing::Iso9660(iso) => walk(iso.root(), "", &mut out)?,
        }
        Ok(out)
    }

    /// Returns a reader over one file's contents, by its path relative to the
    /// image root (e.g. `"sources/boot.wim"`). `Ok(None)` if `path` doesn't
    /// name a regular file (missing, or a directory/symlink).
    ///
    /// Both backends stream: the UDF backend resolves the file's extent list
    /// once and reads content in whatever chunks the caller asks for
    /// (`image::udf`, phase 3 M1 / #40), so even a multi-GB `install.wim`
    /// costs constant memory during W3's copy.
    pub fn open_file(&self, path: &str) -> io::Result<Option<Box<dyn Read + '_>>> {
        match &self.backing {
            Backing::Udf(udf) => match udf_find(udf, path).map_err(udf::UdfError::into_io)? {
                Some(entry) if entry.is_file() => {
                    let reader = udf.open_file(&entry).map_err(udf::UdfError::into_io)?;
                    Ok(Some(Box::new(reader)))
                }
                _ => Ok(None),
            },
            Backing::Iso9660(iso) => match iso.open(path).map_err(cdfs_err_to_io)? {
                Some(cdfs::DirectoryEntry::File(file)) => Ok(Some(Box::new(file.read()))),
                _ => Ok(None),
            },
        }
    }
}

fn walk<T: cdfs::ISO9660Reader>(
    dir: &cdfs::ISODirectory<T>,
    prefix: &str,
    out: &mut Vec<IsoFileEntry>,
) -> io::Result<()> {
    for entry in dir.contents() {
        let entry = entry.map_err(cdfs_err_to_io)?;
        let name = entry.identifier();
        if name == "." || name == ".." {
            continue;
        }
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match entry {
            cdfs::DirectoryEntry::Directory(subdir) => walk(&subdir, &path, out)?,
            cdfs::DirectoryEntry::File(file) => out.push(IsoFileEntry {
                path,
                size: u64::from(file.size()),
            }),
            cdfs::DirectoryEntry::Symlink(_) => {}
        }
    }
    Ok(())
}

fn walk_udf<T: Read + Seek>(
    udf: &udf::UdfVolume<T>,
    dir: &udf::UdfDir,
    prefix: &str,
    out: &mut Vec<IsoFileEntry>,
) -> io::Result<()> {
    for entry in dir.entries() {
        let path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{prefix}/{}", entry.name())
        };
        if entry.is_dir() {
            let subdir = udf
                .read_directory(&entry.icb)
                .map_err(udf::UdfError::into_io)?;
            walk_udf(udf, &subdir, &path, out)?;
        } else {
            out.push(IsoFileEntry {
                path,
                size: entry.size,
            });
        }
    }
    Ok(())
}

fn cdfs_err_to_io(err: cdfs::ISOError) -> io::Error {
    match err {
        cdfs::ISOError::Io(err) => err,
        other => io::Error::other(other.to_string()),
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixtures {
    //! Hand-rolled, minimal ISO9660 images -- just enough structure for
    //! `cdfs` to parse (Primary Volume Descriptor, terminator, single-block
    //! directory extents, no path table), not a byte-accurate replica of what
    //! a real mastering tool emits. Same spirit as `image::isohybrid`'s
    //! hand-crafted sector fixtures, one level deeper since here `cdfs` has
    //! to actually walk a directory tree rather than read fixed offsets.
    //!
    //! Exposed beyond this crate's own tests, behind the `test-fixtures`
    //! feature, so `argos-privileged`'s root-gated loop-device integration
    //! tests (backlog #27, W3) can exercise a real
    //! Windows-installer-shaped ISO without duplicating this builder.

    const BLOCK: usize = 2048;

    fn even_pad(mut field: Vec<u8>) -> Vec<u8> {
        if !field.len().is_multiple_of(2) {
            field.push(0);
        }
        field
    }

    /// One ISO9660 directory record (ECMA-119 §9.1): fixed 32-byte header +
    /// a length-prefixed identifier, no System Use (SUSP) area.
    fn dir_record(extent_loc: u32, extent_length: u32, is_dir: bool, identifier: &[u8]) -> Vec<u8> {
        let mut identifier_field = vec![identifier.len() as u8];
        identifier_field.extend_from_slice(identifier);
        let identifier_field = even_pad(identifier_field);

        let length = 32 + identifier_field.len();
        let mut record = Vec::with_capacity(length);
        record.push(length as u8);
        record.push(0); // extended attribute record length
        record.extend_from_slice(&extent_loc.to_le_bytes());
        record.extend_from_slice(&extent_loc.to_be_bytes());
        record.extend_from_slice(&extent_length.to_le_bytes());
        record.extend_from_slice(&extent_length.to_be_bytes());
        record.extend_from_slice(&[0u8; 7]); // recording date/time: unused by these tests
        record.push(if is_dir { 0x02 } else { 0x00 }); // file flags (bit 1 = directory)
        record.push(0); // file unit size
        record.push(0); // interleave gap size
        record.extend_from_slice(&1u16.to_le_bytes());
        record.extend_from_slice(&1u16.to_be_bytes()); // volume sequence number
        record.extend_from_slice(&identifier_field);
        debug_assert_eq!(record.len(), length);
        record
    }

    fn pad_to_block(mut data: Vec<u8>) -> Vec<u8> {
        assert!(data.len() <= BLOCK, "fixture content overflowed one block");
        data.resize(BLOCK, 0);
        data
    }

    /// The Primary Volume Descriptor at LBA 16, embedding `root_record` (the
    /// root directory's own directory-record entry, ECMA-119 §8.4.14).
    fn primary_volume_descriptor(root_record: &[u8], volume_space_size: u32) -> Vec<u8> {
        let mut d = Vec::with_capacity(BLOCK);
        d.push(1); // volume descriptor type: Primary
        d.extend_from_slice(b"CD001\x01"); // standard identifier + version
        d.push(0); // unused
        d.extend_from_slice(&[0u8; 32]); // system identifier
        d.extend_from_slice(&[0u8; 32]); // volume identifier
        d.extend_from_slice(&[0u8; 8]); // unused
        d.extend_from_slice(&volume_space_size.to_le_bytes());
        d.extend_from_slice(&volume_space_size.to_be_bytes());
        d.extend_from_slice(&[0u8; 32]); // escape sequences: none, plain ISO9660
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_be_bytes()); // volume set size
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_be_bytes()); // volume sequence number
        d.extend_from_slice(&2048u16.to_le_bytes());
        d.extend_from_slice(&2048u16.to_be_bytes()); // logical block size
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_be_bytes()); // path table size: no path table
        d.extend_from_slice(&0u32.to_le_bytes()); // type L path table location
        d.extend_from_slice(&0u32.to_le_bytes()); // optional type L path table location
        d.extend_from_slice(&0u32.to_be_bytes()); // type M path table location
        d.extend_from_slice(&0u32.to_be_bytes()); // optional type M path table location
        d.extend_from_slice(root_record);
        d.extend_from_slice(&[0u8; 128]); // volume set identifier
        d.extend_from_slice(&[0u8; 128]); // publisher identifier
        d.extend_from_slice(&[0u8; 128]); // data preparer identifier
        d.extend_from_slice(&[0u8; 128]); // application identifier
        d.extend_from_slice(&[0u8; 38]); // copyright file identifier
        d.extend_from_slice(&[0u8; 36]); // abstract file identifier
        d.extend_from_slice(&[0u8; 37]); // bibliographic file identifier
        d.extend_from_slice(&[0u8; 17]); // creation date/time
        d.extend_from_slice(&[0u8; 17]); // modification date/time
        d.extend_from_slice(&[0u8; 17]); // expiration date/time
        d.extend_from_slice(&[0u8; 17]); // effective date/time
        d.push(1); // file structure version
        pad_to_block(d)
    }

    fn volume_descriptor_set_terminator() -> Vec<u8> {
        let mut d = Vec::with_capacity(BLOCK);
        d.push(255);
        d.extend_from_slice(b"CD001\x01");
        pad_to_block(d)
    }

    /// Builds a minimal, synthetic Windows-installer-shaped ISO9660 image:
    ///
    /// ```text
    /// LBA 16  Primary Volume Descriptor
    /// LBA 17  Volume Descriptor Set Terminator
    /// LBA 18  root directory extent   ("." / ".." / BOOTMGR / SOURCES)
    /// LBA 19  SOURCES directory extent ("." / ".." / BOOT.WIM)
    /// LBA 20  BOOTMGR file content
    /// LBA 21  BOOT.WIM file content
    /// ```
    ///
    /// `include_bootmgr` / `include_boot_wim` let tests build the negative
    /// cases: a Windows-shaped ISO missing one of the two required files.
    pub fn windows_installer_iso(include_bootmgr: bool, include_boot_wim: bool) -> Vec<u8> {
        const ROOT_LBA: u32 = 18;
        const SOURCES_LBA: u32 = 19;
        const BOOTMGR_LBA: u32 = 20;
        const BOOT_WIM_LBA: u32 = 21;
        const ROOT_EXTENT_LEN: u32 = BLOCK as u32;

        let bootmgr_content = b"argos test fixture: windows boot manager".to_vec();
        let boot_wim_content = b"argos test fixture: boot.wim payload".to_vec();

        let mut sources_dir = Vec::new();
        sources_dir.extend(dir_record(SOURCES_LBA, ROOT_EXTENT_LEN, true, &[0])); // "."
        sources_dir.extend(dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[1])); // ".."
        if include_boot_wim {
            sources_dir.extend(dir_record(
                BOOT_WIM_LBA,
                boot_wim_content.len() as u32,
                false,
                b"BOOT.WIM;1",
            ));
        }
        let sources_dir = pad_to_block(sources_dir);

        let mut root_dir = Vec::new();
        root_dir.extend(dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[0])); // "."
        root_dir.extend(dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[1])); // ".."
        if include_bootmgr {
            root_dir.extend(dir_record(
                BOOTMGR_LBA,
                bootmgr_content.len() as u32,
                false,
                b"BOOTMGR;1",
            ));
        }
        root_dir.extend(dir_record(SOURCES_LBA, ROOT_EXTENT_LEN, true, b"SOURCES"));
        let root_dir = pad_to_block(root_dir);

        let root_record = dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[0]);
        let pvd = primary_volume_descriptor(&root_record, 22);
        let terminator = volume_descriptor_set_terminator();

        let mut image = vec![0u8; 16 * BLOCK]; // system area, LBA 0-15
        image.extend(pvd);
        image.extend(terminator);
        image.extend(root_dir);
        image.extend(sources_dir);
        image.extend(pad_to_block(bootmgr_content));
        image.extend(pad_to_block(boot_wim_content));
        image
    }

    /// The UDF counterpart to [`windows_installer_iso`] above: what a real
    /// official Windows installer image actually looks like (confirmed
    /// against a real Windows 10 22H2 ISO -- see this module's parent doc
    /// comment), built with `hadris_udf::write` rather than hand-rolled
    /// bytes, since UDF/ECMA-167 has no equivalent to ISO9660's "just a
    /// couple of directory records" simplicity.
    pub fn udf_windows_installer_iso(include_bootmgr: bool, include_boot_wim: bool) -> Vec<u8> {
        use hadris_udf::write::{SimpleDir, SimpleFile, UdfWriteOptions, UdfWriter};
        use std::io::Cursor;

        let mut root = SimpleDir::new("");
        if include_bootmgr {
            root.add_file(SimpleFile::new(
                "bootmgr",
                b"argos test fixture: windows boot manager".to_vec(),
            ));
        }
        let mut sources = SimpleDir::new("sources");
        if include_boot_wim {
            sources.add_file(SimpleFile::new(
                "boot.wim",
                b"argos test fixture: boot.wim payload".to_vec(),
            ));
        }
        root.add_dir(sources);

        let output = UdfWriter::create(Cursor::new(Vec::new()), &root, UdfWriteOptions::default())
            .expect("building the synthetic UDF fixture should succeed");
        output.into_inner().into_inner()
    }

    /// A plain, empty UDF image -- the UDF counterpart to [`plain_iso`]
    /// below: a valid UDF volume, but nothing Windows-shaped about it.
    pub fn plain_udf() -> Vec<u8> {
        use hadris_udf::write::{SimpleDir, UdfWriteOptions, UdfWriter};
        use std::io::Cursor;

        let root = SimpleDir::new("");
        let output = UdfWriter::create(Cursor::new(Vec::new()), &root, UdfWriteOptions::default())
            .expect("building the synthetic UDF fixture should succeed");
        output.into_inner().into_inner()
    }

    /// A plain ISO9660 image with an empty root directory and nothing else --
    /// e.g. what a Linux ISO's filesystem looks like from `cdfs`'s point of
    /// view: valid ISO9660, but nothing Windows-shaped about it.
    pub fn plain_iso() -> Vec<u8> {
        const ROOT_LBA: u32 = 18;
        const ROOT_EXTENT_LEN: u32 = BLOCK as u32;

        let mut root_dir = Vec::new();
        root_dir.extend(dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[0]));
        root_dir.extend(dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[1]));
        let root_dir = pad_to_block(root_dir);

        let root_record = dir_record(ROOT_LBA, ROOT_EXTENT_LEN, true, &[0]);
        let pvd = primary_volume_descriptor(&root_record, 19);
        let terminator = volume_descriptor_set_terminator();

        let mut image = vec![0u8; 16 * BLOCK];
        image.extend(pvd);
        image.extend(terminator);
        image.extend(root_dir);
        image
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{plain_iso, plain_udf, udf_windows_installer_iso, windows_installer_iso};
    use super::*;
    use std::io::Cursor;

    // -- UDF: the shape real Windows installer media actually has --

    #[test]
    fn classifies_a_real_windows_installer_shape_udf() {
        let image = udf_windows_installer_iso(true, true);
        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_udf_missing_bootmgr() {
        let image = udf_windows_installer_iso(false, true);
        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_udf_missing_boot_wim() {
        let image = udf_windows_installer_iso(true, false);
        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_plain_non_windows_udf() {
        let result = classify_reader(Cursor::new(plain_udf())).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn lists_every_file_with_its_path_and_size_udf() {
        let dir = tempfile::tempdir().unwrap();
        let iso_path = dir.path().join("windows.iso");
        std::fs::write(&iso_path, udf_windows_installer_iso(true, true)).unwrap();

        let iso = WindowsIso::open(&iso_path).unwrap();
        let mut files = iso.list_files().unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(
            files,
            vec![
                IsoFileEntry {
                    path: "bootmgr".to_string(),
                    size: 40,
                },
                IsoFileEntry {
                    path: "sources/boot.wim".to_string(),
                    size: 36,
                },
            ]
        );
    }

    #[test]
    fn opens_and_reads_a_file_by_path_udf() {
        let dir = tempfile::tempdir().unwrap();
        let iso_path = dir.path().join("windows.iso");
        std::fs::write(&iso_path, udf_windows_installer_iso(true, true)).unwrap();

        let iso = WindowsIso::open(&iso_path).unwrap();
        let mut contents = Vec::new();
        iso.open_file(BOOT_WIM_PATH)
            .unwrap()
            .expect("sources/boot.wim should exist")
            .read_to_end(&mut contents)
            .unwrap();

        assert_eq!(contents, b"argos test fixture: boot.wim payload");
        assert!(iso.open_file("no/such/file").unwrap().is_none());
    }

    #[test]
    fn classification_is_case_insensitive_on_udf_names() {
        // hadris_udf::write::SimpleFile keeps whatever case it's given;
        // real Windows UDF media has been observed both ways, so lookups
        // must not assume one.
        use hadris_udf::write::{SimpleDir, SimpleFile, UdfWriteOptions, UdfWriter};

        let mut root = SimpleDir::new("");
        root.add_file(SimpleFile::new("BOOTMGR", b"x".to_vec()));
        let mut sources = SimpleDir::new("SOURCES");
        sources.add_file(SimpleFile::new("BOOT.WIM", b"y".to_vec()));
        root.add_dir(sources);
        let image = UdfWriter::create(Cursor::new(Vec::new()), &root, UdfWriteOptions::default())
            .unwrap()
            .into_inner()
            .into_inner();

        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(result.is_windows_installer_iso());
    }

    // -- ISO9660 fallback: this module's own synthetic fixtures, and any
    // genuinely ISO9660-only Windows-shaped image (real media is UDF -- see
    // this module's top doc comment) --

    #[test]
    fn classifies_a_windows_installer_shape_iso9660_fallback() {
        let image = windows_installer_iso(true, true);
        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_iso_missing_bootmgr() {
        let image = windows_installer_iso(false, true);
        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_iso_missing_boot_wim() {
        let image = windows_installer_iso(true, false);
        let result = classify_reader(Cursor::new(image)).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_plain_non_windows_iso() {
        let result = classify_reader(Cursor::new(plain_iso())).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn rejects_input_that_is_not_recognized_in_either_format() {
        let result = classify_reader(Cursor::new(vec![0u8; 4096])).unwrap();
        assert!(!result.is_windows_installer_iso());
    }

    #[test]
    fn lists_every_file_with_its_path_and_size() {
        let image = windows_installer_iso(true, true);
        let iso = cdfs::ISO9660::new(Cursor::new(image)).unwrap();
        let mut out = Vec::new();
        walk(iso.root(), "", &mut out).unwrap();
        out.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(
            out,
            vec![
                IsoFileEntry {
                    path: "BOOTMGR".to_string(),
                    size: 40,
                },
                IsoFileEntry {
                    path: "SOURCES/BOOT.WIM".to_string(),
                    size: 36,
                },
            ]
        );
    }

    #[test]
    fn opens_and_reads_a_file_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let iso_path = dir.path().join("windows.iso");
        std::fs::write(&iso_path, windows_installer_iso(true, true)).unwrap();

        let iso = WindowsIso::open(&iso_path).unwrap();
        let mut contents = Vec::new();
        iso.open_file(BOOT_WIM_PATH)
            .unwrap()
            .expect("sources/boot.wim should exist")
            .read_to_end(&mut contents)
            .unwrap();

        assert_eq!(contents, b"argos test fixture: boot.wim payload");
        assert!(iso.open_file("no/such/file").unwrap().is_none());
    }

    #[test]
    fn list_files_matches_open_file_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let iso_path = dir.path().join("windows.iso");
        std::fs::write(&iso_path, windows_installer_iso(true, true)).unwrap();

        let iso = WindowsIso::open(&iso_path).unwrap();
        let mut files = iso.list_files().unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "BOOTMGR");
        assert_eq!(files[1].path, "SOURCES/BOOT.WIM");
    }
}
