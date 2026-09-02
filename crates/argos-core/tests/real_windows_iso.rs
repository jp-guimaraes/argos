//! Phase 3 M1.5 (backlog #40): manual validation of `image::udf`'s streaming
//! reader against a *real* Windows installer ISO.
//!
//! `image::udf`'s unit tests already cover the format corners (multi-extent
//! files, inline data, 2KB blocks) against `hadris-udf`'s writer as an
//! independent oracle. What they cannot cover is M1's acceptance criterion
//! on real media: extracting a multi-GB `install.wim`/`install.esd` from an
//! official Windows 10/11 ISO must cost O(buffer) memory, and the bytes must
//! match what an independent UDF implementation reads from the same ISO.
//!
//! Ignored because it needs a real ISO (multi-GB, not committable): point
//! `ARGOS_TEST_REAL_WINDOWS_ISO` at one and run
//!
//! ```sh
//! ARGOS_TEST_REAL_WINDOWS_ISO=.testdata/Win10_22H2_English_x64v1.iso \
//!     cargo test -p argos-core --test real_windows_iso -- --ignored --nocapture
//! ```
//!
//! On macOS the test additionally mounts the ISO read-only with `hdiutil`
//! and hashes the same file through Apple's own UDF driver -- a second,
//! fully independent oracle -- and asserts both digests match. On other
//! platforms it prints the streamed digest for manual comparison (e.g.
//! against `mount -o loop,ro` + `sha256sum`).

use argos_core::image::windows::WindowsIso;
use sha2::{Digest, Sha256};
use std::io::Read;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

/// High-water mark of this process's resident set, in bytes.
fn peak_rss_bytes() -> u64 {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
    let ru = unsafe { ru.assume_init() };
    // ru_maxrss is bytes on macOS, kilobytes on Linux.
    let raw = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

fn sha256_hex_of_reader(mut reader: impl Read) -> (String, u64) {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).expect("read failed");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    (format!("{:x}", hasher.finalize()), total)
}

/// Mounts the ISO read-only via `hdiutil attach` and returns the mount
/// point; detaches on drop. `None` (never a panic) when hdiutil is missing
/// or the attach fails, so the oracle step can skip cleanly.
#[cfg(target_os = "macos")]
struct MountedIso {
    mount_point: PathBuf,
}

#[cfg(target_os = "macos")]
impl MountedIso {
    fn attach(iso: &Path) -> Option<Self> {
        let output = Command::new("hdiutil")
            .args(["attach", "-readonly", "-nobrowse"])
            .arg(iso)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        // Each output line ends with the mount point for mounted volumes;
        // find the first path under /Volumes/. Windows install media volume
        // labels have no whitespace, so taking the line's tail from
        // "/Volumes/" is unambiguous here.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mount = stdout.lines().find_map(|line| {
            line.find("/Volumes/")
                .map(|idx| PathBuf::from(line[idx..].trim_end()))
        })?;
        Some(Self { mount_point: mount })
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedIso {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .arg("detach")
            .arg(&self.mount_point)
            .output();
    }
}

#[test]
#[ignore = "needs a real Windows ISO via ARGOS_TEST_REAL_WINDOWS_ISO"]
fn streams_real_install_image_in_constant_memory() {
    let iso_path = match std::env::var_os("ARGOS_TEST_REAL_WINDOWS_ISO") {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("skipping: ARGOS_TEST_REAL_WINDOWS_ISO not set");
            return;
        }
    };

    let iso = WindowsIso::open(&iso_path).expect("failed to open ISO");
    let files = iso.list_files().expect("failed to list ISO files");

    // Real Windows media carries exactly one of sources/install.{wim,esd}.
    let entry = files
        .iter()
        .find(|e| {
            e.path.eq_ignore_ascii_case("sources/install.wim")
                || e.path.eq_ignore_ascii_case("sources/install.esd")
        })
        .expect("no sources/install.{wim,esd} in the ISO -- not real Windows install media?");

    // The whole point is a file large enough that a whole-file read would
    // visibly blow the RSS bound below; official Win10/11 install images
    // all clear this comfortably.
    assert!(
        entry.size >= 2 * 1024 * 1024 * 1024,
        "{} is only {} bytes -- too small to prove streaming",
        entry.path,
        entry.size
    );

    let started = std::time::Instant::now();
    let reader = iso
        .open_file(&entry.path)
        .expect("open_file failed")
        .expect("open_file returned None for a listed file");
    let (streamed_hash, streamed_bytes) = sha256_hex_of_reader(reader);
    let elapsed = started.elapsed();

    assert_eq!(
        streamed_bytes, entry.size,
        "streamed byte count differs from the listed file size"
    );

    let peak = peak_rss_bytes();
    eprintln!(
        "streamed {} ({} bytes) in {:.1?}: sha256={streamed_hash}, peak RSS {}MiB",
        entry.path,
        entry.size,
        elapsed,
        peak >> 20,
    );
    // O(buffer) acceptance: with a 1MiB read buffer, real peaks sit well
    // under 100MiB; the pre-M1 whole-file read peaked near the file size
    // (>=2GiB here). 256MiB splits those regimes with margin for
    // allocator/test-harness overhead.
    let rss_bound = 256 * 1024 * 1024;
    assert!(
        peak < rss_bound,
        "peak RSS {peak} bytes >= {rss_bound} -- the UDF reader is not streaming"
    );

    // Independent oracle: Apple's UDF driver reading the very same file.
    #[cfg(target_os = "macos")]
    {
        let Some(mounted) = MountedIso::attach(&iso_path) else {
            eprintln!("skipping oracle comparison: hdiutil attach failed");
            return;
        };
        let oracle_path = mounted.mount_point.join(&entry.path);
        let oracle_file = std::fs::File::open(&oracle_path)
            .unwrap_or_else(|e| panic!("oracle open {} failed: {e}", oracle_path.display()));
        let (oracle_hash, oracle_bytes) = sha256_hex_of_reader(oracle_file);
        assert_eq!(oracle_bytes, entry.size, "oracle byte count differs");
        assert_eq!(
            streamed_hash, oracle_hash,
            "image::udf and macOS's native UDF driver disagree on {}",
            entry.path
        );
        eprintln!("oracle match: macOS UDF driver agrees, sha256={oracle_hash}");
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "no in-test oracle on this platform; compare the digest above \
             against a loop-mounted read of the same file"
        );
    }
}
