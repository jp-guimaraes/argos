//! Phase 3 M2 acceptance against **real Windows install media**: streams the
//! actual multi-GB `install.wim` out of an official Windows ISO through
//! `image::udf` (M1) into `image::wim`'s splitter (M2), producing real
//! `.swm` parts, then hands those parts to wimlib to verify.
//!
//! This is the pairing the FAT32 layout depends on and the one the
//! synthetic fixtures cannot prove: a genuine Microsoft-authored WIM, with
//! whatever resource sizes, chunk tables and metadata layout Microsoft's
//! own imaging tools produced, split at the real ~3.8GB part target.
//!
//! Needs a real ISO (`ARGOS_TEST_REAL_WINDOWS_ISO`), `wimlib-imagex` on
//! PATH, and several GB of scratch space (`ARGOS_TEST_SPLIT_OUT_DIR` to
//! choose where; defaults to the system temp dir). Run:
//!
//! ```sh
//! ARGOS_TEST_REAL_WINDOWS_ISO=.testdata/Win10_22H2_English_x64v1.iso \
//!     cargo test -p argos-core --release --test real_wim_split \
//!     -- --ignored --nocapture
//! ```

use argos_core::image::wim::{self, WimImage};
use argos_core::image::windows::WindowsIso;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

/// Same target the FAT32 write path uses (`argos-privileged`'s
/// `SWM_PART_TARGET_BYTES`), duplicated rather than depended on: this crate
/// doesn't depend on `argos-privileged`, and the value is what the media
/// itself must satisfy.
const SWM_PART_TARGET_BYTES: u64 = 3_800 * 1024 * 1024;

/// FAT32's hard per-file ceiling -- what every emitted part must clear.
const FAT32_MAX_FILE_BYTES: u64 = u32::MAX as u64;

fn wimlib_available() -> bool {
    Command::new("wimlib-imagex")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
#[ignore = "needs a real Windows ISO + wimlib + several GB of scratch space"]
fn splits_a_real_install_wim_into_swm_parts_wimlib_accepts() {
    let Some(iso_path) = std::env::var_os("ARGOS_TEST_REAL_WINDOWS_ISO").map(PathBuf::from) else {
        eprintln!("skipping: ARGOS_TEST_REAL_WINDOWS_ISO not set");
        return;
    };
    if !wimlib_available() {
        eprintln!("skipping: wimlib-imagex not installed");
        return;
    }

    let iso = WindowsIso::open(&iso_path).expect("failed to open the ISO");
    let files = iso.list_files().expect("failed to list the ISO");
    let wim_entry = files
        .iter()
        .find(|e| e.path.eq_ignore_ascii_case("sources/install.wim"))
        .expect("no sources/install.wim -- .esd media can't be split (that's M2.3's refusal)");
    eprintln!(
        "source: {} ({} bytes, {:.2} GiB)",
        wim_entry.path,
        wim_entry.size,
        wim_entry.size as f64 / (1 << 30) as f64
    );
    assert!(
        wim_entry.size > FAT32_MAX_FILE_BYTES,
        "this install.wim already fits FAT32 -- nothing for the splitter to prove"
    );

    let out_dir = match std::env::var_os("ARGOS_TEST_SPLIT_OUT_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            ScratchDir::Borrowed(dir)
        }
        None => ScratchDir::Owned(tempfile::tempdir().unwrap()),
    };

    // Read the WIM straight out of the ISO -- no extraction to disk first.
    let mut reader = iso
        .open_file_seekable(&wim_entry.path)
        .expect("open_file_seekable failed")
        .expect("install.wim should be seekable on a UDF ISO");
    let image = WimImage::open(&mut reader).expect("our reader should parse a real install.wim");
    eprintln!(
        "parsed: {} lookup entries ({} metadata), {} bytes of resources, XML {} bytes, {} image(s)",
        image.entries.len(),
        image
            .entries
            .iter()
            .filter(|e| e.resource.is_metadata())
            .count(),
        image.total_resource_bytes(),
        image.xml_data.len(),
        image.header.image_count,
    );

    let predicted = wim::plan_part_sizes(&image, SWM_PART_TARGET_BYTES);
    eprintln!("planned {} part(s): {predicted:?}", predicted.len());
    assert!(
        predicted.len() > 1,
        "a >4GiB install.wim must need more than one part"
    );

    let started = Instant::now();
    let paths = std::cell::RefCell::new(Vec::<PathBuf>::new());
    let outcome = wim::split(
        &mut reader,
        &image,
        SWM_PART_TARGET_BYTES,
        |part_number| {
            let name = if part_number == 1 {
                "install.swm".to_string()
            } else {
                format!("install{part_number}.swm")
            };
            let path = out_dir.path().join(name);
            let file = std::fs::File::create(&path)?;
            paths.borrow_mut().push(path);
            Ok(std::io::BufWriter::with_capacity(1 << 20, file))
        },
        |_| {},
    )
    .expect("splitting a real install.wim should succeed");
    let paths = paths.into_inner();
    eprintln!(
        "split into {} part(s) in {:.1?}: {:?}",
        paths.len(),
        started.elapsed(),
        outcome.part_sizes
    );

    assert_eq!(
        outcome.part_sizes, predicted,
        "plan_part_sizes must predict what split writes, on real media too"
    );

    // The whole point: every part fits FAT32.
    for (idx, (path, size)) in paths.iter().zip(&outcome.part_sizes).enumerate() {
        let on_disk = std::fs::metadata(path).unwrap().len();
        assert_eq!(on_disk, *size, "part {} size mismatch", idx + 1);
        assert!(
            on_disk <= FAT32_MAX_FILE_BYTES,
            "part {} is {on_disk} bytes, over FAT32's per-file limit",
            idx + 1
        );
    }

    // The parts sum to slightly *less* than the source: they carry only
    // what the lookup table references (resources + table + XML + one
    // header each), dropping the source's integrity table and any
    // unreferenced slack. Accounted exactly, so a silent loss of resource
    // bytes would show up here rather than hiding in a fuzzy comparison.
    let total_split: u64 = outcome.part_sizes.iter().sum();
    let accounted = image.total_resource_bytes()
        + wim::LOOKUP_ENTRY_SIZE * image.entries.len() as u64
        + (wim::HEADER_SIZE as u64 + image.xml_data.len() as u64) * outcome.part_sizes.len() as u64;
    assert_eq!(
        total_split, accounted,
        "the parts must hold every resource byte plus per-part bookkeeping, nothing else"
    );
    eprintln!(
        "total across parts: {total_split} bytes ({} bytes of source slack/integrity table not carried over)",
        wim_entry.size as i64 - total_split as i64
    );

    // wimlib reads each part and agrees on the numbering.
    for (idx, path) in paths.iter().enumerate() {
        let output = Command::new("wimlib-imagex")
            .args(["info", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "wimlib rejected part {}: {}",
            idx + 1,
            String::from_utf8_lossy(&output.stderr)
        );
        let info = String::from_utf8_lossy(&output.stdout);
        let part_line = info
            .lines()
            .find(|l| l.starts_with("Part Number:"))
            .unwrap_or_else(|| panic!("no Part Number in wimlib info:\n{info}"));
        assert_eq!(
            part_line.split(':').nth(1).unwrap().trim(),
            format!("{}/{}", idx + 1, paths.len())
        );
    }

    // The acceptance criterion: wimlib's own integrity check over the set.
    let glob = out_dir.path().join("install*.swm");
    let verify = Command::new("wimlib-imagex")
        .args([
            "verify",
            paths[0].to_str().unwrap(),
            "--ref",
            glob.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    eprintln!(
        "wimlib verify: {}\n{}",
        verify.status,
        String::from_utf8_lossy(&verify.stdout)
    );
    assert!(
        verify.status.success(),
        "wimlib verify failed on our split of real media: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    // And that part 1 still carries the image list. `--extract-xml` writes
    // the block verbatim, which per the spec is UTF-16LE with a 0xFEFF BOM
    // -- decode it rather than searching the raw bytes for ASCII (the first
    // run of this test asserted on UTF-8 and failed on a perfectly good
    // part).
    let xml_path = out_dir.path().join("part1.xml");
    let info = Command::new("wimlib-imagex")
        .args([
            "info",
            paths[0].to_str().unwrap(),
            "--extract-xml",
            xml_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        info.status.success(),
        "wimlib could not read the image list from part 1: {}",
        String::from_utf8_lossy(&info.stderr)
    );
    let raw = std::fs::read(&xml_path).unwrap();
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    let xml = String::from_utf16_lossy(&units);
    // `<IMAGE ` with the trailing space: bare "<IMAGE" also matches every
    // closing `</IMAGE>` tag, which double-counts.
    let image_count = xml.matches("<IMAGE ").count();
    assert_eq!(
        image_count as u32, image.header.image_count,
        "part 1's XML block should still list every image"
    );
    eprintln!("part 1 still lists {image_count} image(s)");
}

/// Either a caller-chosen output directory (kept) or a temp one (deleted).
enum ScratchDir {
    Borrowed(PathBuf),
    Owned(tempfile::TempDir),
}

impl ScratchDir {
    fn path(&self) -> &std::path::Path {
        match self {
            ScratchDir::Borrowed(p) => p,
            ScratchDir::Owned(d) => d.path(),
        }
    }
}
