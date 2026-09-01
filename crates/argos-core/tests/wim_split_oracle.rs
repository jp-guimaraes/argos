//! Phase 3 M2.4 (backlog #42): validates `image::wim`'s splitter against
//! **wimlib** as an external test oracle -- a fully independent
//! implementation of the WIM format.
//!
//! wimlib is GPL, so it is never linked or vendored: this harness only
//! *runs the `wimlib-imagex` binary as a subprocess in tests*, the same way
//! a human would run it by hand. Nothing here ships in any Argos binary.
//!
//! What it checks, on WIMs wimlib itself captured (uncompressed, XPRESS,
//! and LZX):
//! 1. `wimlib-imagex info` accepts each `.swm` part we emit and reports the
//!    part numbering we intended;
//! 2. `wimlib-imagex verify` passes on the split set -- meaning every
//!    resource's stored bytes still hash to the SHA-1 recorded in the
//!    lookup table, across parts;
//! 3. `wimlib-imagex apply` extracts the original file tree *from our split
//!    set*, byte-identical to applying the unsplit source. This is the real
//!    acceptance criterion: it's what Windows Setup does with `install.swm`.
//!
//! Ignored by default (needs `wimlib-imagex` on PATH: `brew install wimlib`
//! / `apt install wimtools`). Run:
//!
//! ```sh
//! cargo test -p argos-core --test wim_split_oracle -- --ignored --nocapture
//! ```
//!
//! Every test skips itself (rather than failing) when wimlib is missing.

use argos_core::image::wim::{self, WimImage};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn wimlib_available() -> bool {
    Command::new("wimlib-imagex")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        return Err(format!(
            "{program} {args:?} failed ({}): {}{}",
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(stdout)
}

/// Builds a source directory of distinct files, each getting its own WIM
/// resource -- the splitter only has boundaries to choose between if there
/// are several.
///
/// Content is deliberately **incompressible** (a seeded xorshift stream, a
/// different seed per file): with compressible filler, XPRESS/LZX shrink a
/// multi-megabyte tree to a few tens of kilobytes and no realistic part
/// limit forces a split at all -- which silently turned the compressed
/// round-trip tests into single-part no-ops the first time this ran.
/// Determinism (rather than real randomness) keeps failures reproducible.
fn build_source_tree(root: &Path, file_count: usize, file_size: usize) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    for i in 0..file_count {
        let mut state = 0x2545_F491_4F6C_DD1Du64 ^ ((i as u64 + 1) << 32);
        let mut content = Vec::with_capacity(file_size);
        while content.len() < file_size {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            content.extend_from_slice(&state.to_le_bytes());
        }
        content.truncate(file_size);
        let dir = if i % 2 == 0 {
            root.to_path_buf()
        } else {
            root.join("sub")
        };
        std::fs::write(dir.join(format!("file{i}.bin")), &content).unwrap();
    }
}

/// Captures `source_dir` into a real WIM with wimlib.
fn capture_wim(source_dir: &Path, wim_path: &Path, compression: &str) -> Result<(), String> {
    run(
        "wimlib-imagex",
        &[
            "capture",
            source_dir.to_str().unwrap(),
            wim_path.to_str().unwrap(),
            "TestImage",
            "--compress",
            compression,
            "--norpfix",
        ],
    )
    .map(|_| ())
}

/// Splits `wim_path` with **our** splitter into `out_dir`, using wimlib's
/// own naming convention (`name.swm`, `name2.swm`, ...) so `wimlib-imagex`
/// can pick the set up with a glob.
fn split_with_argos(wim_path: &Path, out_dir: &Path, max_part_bytes: u64) -> Vec<PathBuf> {
    let mut source = File::open(wim_path).unwrap();
    let image = WimImage::open(&mut source).unwrap();

    let paths = std::cell::RefCell::new(Vec::new());
    wim::split(
        &mut source,
        &image,
        max_part_bytes,
        |part_number| {
            let name = if part_number == 1 {
                "split.swm".to_string()
            } else {
                format!("split{part_number}.swm")
            };
            let path = out_dir.join(name);
            let file = File::create(&path)?;
            paths.borrow_mut().push(path);
            Ok(file)
        },
        |_| {},
    )
    .unwrap();

    paths.into_inner()
}

/// One full round trip at a given compression: capture with wimlib, split
/// with Argos, then have wimlib verify and apply *our* parts.
fn round_trip_against_wimlib(compression: &str, max_part_bytes: u64, expect_multiple: bool) {
    if !wimlib_available() {
        eprintln!("skipping: wimlib-imagex not installed");
        return;
    }

    let work = tempfile::tempdir().unwrap();
    let source_dir = work.path().join("src");
    let wim_path = work.path().join("source.wim");
    let split_dir = work.path().join("parts");
    std::fs::create_dir_all(&split_dir).unwrap();

    build_source_tree(&source_dir, 12, 400 * 1024);
    capture_wim(&source_dir, &wim_path, compression).expect("wimlib capture should succeed");

    let parts = split_with_argos(&wim_path, &split_dir, max_part_bytes);
    eprintln!(
        "{compression}: source {} bytes -> {} part(s): {:?}",
        std::fs::metadata(&wim_path).unwrap().len(),
        parts.len(),
        parts
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .collect::<Vec<_>>()
    );
    if expect_multiple {
        assert!(
            parts.len() > 1,
            "the chosen limit should have forced more than one part"
        );
    }

    // (1) wimlib parses each part and agrees on the numbering.
    for (idx, part) in parts.iter().enumerate() {
        let info = run("wimlib-imagex", &["info", part.to_str().unwrap()])
            .unwrap_or_else(|e| panic!("wimlib rejected part {}: {e}", idx + 1));
        let part_line = info
            .lines()
            .find(|l| l.starts_with("Part Number:"))
            .unwrap_or_else(|| {
                panic!(
                    "no Part Number in wimlib info for part {}:\n{info}",
                    idx + 1
                )
            });
        assert_eq!(
            part_line.split(':').nth(1).unwrap().trim(),
            format!("{}/{}", idx + 1, parts.len()),
            "wimlib read a different part number than we wrote"
        );
    }

    // (2) wimlib's own integrity check over the split set.
    let first = parts[0].to_str().unwrap();
    let glob = split_dir.join("split*.swm");
    run(
        "wimlib-imagex",
        &["verify", first, "--ref", glob.to_str().unwrap()],
    )
    .expect("wimlib verify should pass on our split set");

    // (3) The acceptance criterion: wimlib applies image 1 *from our parts*
    // and reproduces the original tree byte for byte.
    let applied = work.path().join("applied");
    run(
        "wimlib-imagex",
        &[
            "apply",
            first,
            "1",
            applied.to_str().unwrap(),
            "--ref",
            glob.to_str().unwrap(),
            "--norpfix",
        ],
    )
    .expect("wimlib should apply image 1 from our split set");

    assert_trees_identical(&source_dir, &applied);
}

fn assert_trees_identical(expected_root: &Path, actual_root: &Path) {
    let mut compared = 0;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let expected_dir = expected_root.join(&rel);
        for entry in std::fs::read_dir(&expected_dir).unwrap() {
            let entry = entry.unwrap();
            let rel_child = rel.join(entry.file_name());
            let actual_child = actual_root.join(&rel_child);
            if entry.file_type().unwrap().is_dir() {
                assert!(
                    actual_child.is_dir(),
                    "{} missing from the applied tree",
                    rel_child.display()
                );
                stack.push(rel_child);
            } else {
                let expected_bytes = std::fs::read(entry.path()).unwrap();
                let actual_bytes = std::fs::read(&actual_child).unwrap_or_else(|e| {
                    panic!("{} missing from the applied tree: {e}", rel_child.display())
                });
                assert_eq!(
                    expected_bytes,
                    actual_bytes,
                    "{} differs after split+apply",
                    rel_child.display()
                );
                compared += 1;
            }
        }
    }
    assert!(compared > 0, "compared no files at all");
    eprintln!("applied tree matches the source ({compared} files)");
}

#[test]
#[ignore = "needs wimlib-imagex on PATH; see module docs"]
fn uncompressed_wim_splits_and_wimlib_applies_it_back() {
    // ~4.8MB of file data uncompressed: a 1.5MB limit forces several parts.
    round_trip_against_wimlib("none", 1_500_000, true);
}

#[test]
#[ignore = "needs wimlib-imagex on PATH; see module docs"]
fn xpress_wim_splits_and_wimlib_applies_it_back() {
    round_trip_against_wimlib("fast", 1_500_000, true);
}

#[test]
#[ignore = "needs wimlib-imagex on PATH; see module docs"]
fn lzx_wim_splits_and_wimlib_applies_it_back() {
    round_trip_against_wimlib("maximum", 1_500_000, true);
}

#[test]
#[ignore = "needs wimlib-imagex on PATH; see module docs"]
fn a_limit_larger_than_the_source_still_produces_a_valid_single_part() {
    round_trip_against_wimlib("fast", 1 << 30, false);
}

/// The M2.3 refusal: a real solid `.esd`-style image (wimlib can write one
/// with `--solid`) must be rejected with the dedicated message, not split
/// into something broken.
#[test]
#[ignore = "needs wimlib-imagex on PATH; see module docs"]
fn a_real_solid_image_is_refused() {
    if !wimlib_available() {
        eprintln!("skipping: wimlib-imagex not installed");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let source_dir = work.path().join("src");
    let esd_path = work.path().join("solid.esd");
    build_source_tree(&source_dir, 4, 64 * 1024);
    run(
        "wimlib-imagex",
        &[
            "capture",
            source_dir.to_str().unwrap(),
            esd_path.to_str().unwrap(),
            "TestImage",
            "--solid",
            "--norpfix",
        ],
    )
    .expect("wimlib should capture a solid image");

    let mut file = File::open(&esd_path).unwrap();
    let err = WimImage::open(&mut file).unwrap_err();
    eprintln!("solid image refused with: {err}");
    assert!(
        err.to_string().contains("install.esd") || err.to_string().contains("solid"),
        "expected the dedicated solid-image refusal, got: {err}"
    );
}

/// Sanity check on the harness itself: if wimlib is present, a *corrupted*
/// part must make `wimlib-imagex verify` fail -- otherwise the oracle would
/// pass vacuously and prove nothing about the tests above.
#[test]
#[ignore = "needs wimlib-imagex on PATH; see module docs"]
fn the_oracle_actually_catches_corruption() {
    if !wimlib_available() {
        eprintln!("skipping: wimlib-imagex not installed");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let source_dir = work.path().join("src");
    let wim_path = work.path().join("source.wim");
    let split_dir = work.path().join("parts");
    std::fs::create_dir_all(&split_dir).unwrap();
    build_source_tree(&source_dir, 8, 200 * 1024);
    capture_wim(&source_dir, &wim_path, "fast").unwrap();

    let parts = split_with_argos(&wim_path, &split_dir, 600_000);
    assert!(parts.len() > 1);

    // Corrupt a byte well inside the last part's resource area.
    {
        use std::io::{Seek, SeekFrom};
        let last = parts.last().unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(last).unwrap();
        f.seek(SeekFrom::Start(wim::HEADER_SIZE as u64 + 16))
            .unwrap();
        f.write_all(&[0xFF]).unwrap();
    }

    let glob = split_dir.join("split*.swm");
    let result = run(
        "wimlib-imagex",
        &[
            "verify",
            parts[0].to_str().unwrap(),
            "--ref",
            glob.to_str().unwrap(),
        ],
    );
    assert!(
        result.is_err(),
        "wimlib verify passed on a corrupted part -- the oracle proves nothing"
    );
}
