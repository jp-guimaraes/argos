//! Detects a Windows installer ISO and provides a thin, read-only wrapper over
//! its file tree -- the counterpart to `image::isohybrid` for the phase 2
//! (Windows ISO support) backlog: [`docs/architecture.md`](../../../../../docs/architecture.md)
//! records the guiding decisions this module implements (Linux-first, and
//! UEFI:NTFS rather than `install.wim` splitting).
//!
//! Unlike an isohybrid Linux image, a Windows installation ISO carries no
//! embedded MBR/GPT at all -- it's a plain ISO9660 filesystem. Recognizing one
//! means actually reading its directory tree rather than probing a couple of
//! fixed byte offsets, so this module reads through [`cdfs`], a pure-Rust
//! ISO9660/ECMA-119 implementation (used here under the local dependency name
//! `cdfs`, backed by the `newtua-cdfs` fork -- see `Cargo.toml` for why).
//!
//! Detection looks for the same two paths every official Windows 10/11
//! install media has always shipped: `bootmgr` and `sources/boot.wim` at the
//! root. `sources/install.wim` (or `.esd`) is what actually gets copied onto
//! the NTFS partition in W3, but it is *not* part of detection: its absence
//! (some multi-edition or trimmed media rename or omit it) says nothing about
//! whether this is a Windows installer, whereas `bootmgr` and `boot.wim`
//! missing does.

use std::fs::File;
use std::io::{self, Read, Seek};
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

/// Classifies the ISO9660 image at `path` as a Windows installer or not.
pub fn classify(path: &Path) -> io::Result<WindowsClassification> {
    let file = File::open(path)?;
    classify_reader(file)
}

/// Same as [`classify`], but against any `Read + Seek` source -- lets tests
/// exercise this against small in-memory fixtures instead of real files.
pub fn classify_reader<R: Read + Seek>(reader: R) -> io::Result<WindowsClassification> {
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
    iso: cdfs::ISO9660<File>,
}

impl WindowsIso {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let iso = cdfs::ISO9660::new(file).map_err(cdfs_err_to_io)?;
        Ok(Self { iso })
    }

    /// Recursively lists every regular file in the image, depth-first.
    /// Symlinks are skipped (Windows install media is plain ISO9660 with no
    /// Rock Ridge symlinks) rather than followed or reported as an error.
    pub fn list_files(&self) -> io::Result<Vec<IsoFileEntry>> {
        let mut out = Vec::new();
        walk(self.iso.root(), "", &mut out)?;
        Ok(out)
    }

    /// Returns a reader over one file's contents, by its path relative to the
    /// image root (e.g. `"sources/boot.wim"`). `Ok(None)` if `path` doesn't
    /// name a regular file (missing, or a directory/symlink).
    pub fn open_file(&self, path: &str) -> io::Result<Option<cdfs::ISOFileReader<File>>> {
        match self.iso.open(path).map_err(cdfs_err_to_io)? {
            Some(cdfs::DirectoryEntry::File(file)) => Ok(Some(file.read())),
            _ => Ok(None),
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

fn cdfs_err_to_io(err: cdfs::ISOError) -> io::Error {
    match err {
        cdfs::ISOError::Io(err) => err,
        other => io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod fixtures {
    //! Hand-rolled, minimal ISO9660 images -- just enough structure for
    //! `cdfs` to parse (Primary Volume Descriptor, terminator, single-block
    //! directory extents, no path table), not a byte-accurate replica of what
    //! a real mastering tool emits. Same spirit as `image::isohybrid`'s
    //! hand-crafted sector fixtures, one level deeper since here `cdfs` has
    //! to actually walk a directory tree rather than read fixed offsets.

    const BLOCK: usize = 2048;

    fn even_pad(mut field: Vec<u8>) -> Vec<u8> {
        if field.len() % 2 != 0 {
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
    use super::fixtures::{plain_iso, windows_installer_iso};
    use super::*;
    use std::io::Cursor;

    #[test]
    fn classifies_a_real_windows_installer_shape() {
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
    fn rejects_input_that_is_not_iso9660_at_all() {
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
