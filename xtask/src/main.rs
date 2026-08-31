//! Developer-only tasks (test fixture generation, release helpers, vendored
//! asset generation). Not part of the published crates.

use std::env;
use std::error::Error;
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

fn main() {
    let args: Vec<String> = env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("vendor-uefi-ntfs") => vendor_uefi_ntfs(&args[2..]),
        _ => {
            print_usage();
            return;
        }
    };

    if let Err(err) = result {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!("xtask: no default task -- pick one:");
    println!("  vendor-uefi-ntfs <dir-with-*_signed.efi> <output.img>");
    println!("      Rebuilds crates/argos-privileged/assets/uefi-ntfs.img.");
    println!("      See that subcommand's doc comment for what it expects.");
}

/// Builds the small FAT boot image `dd`'d as partition 1 of a Windows
/// installer write (backlog #27, W3): a UEFI application that, placed at the
/// standard fallback boot path, scans the rest of the disk for the NTFS
/// partition and chainloads Windows's own bootloader from it -- the whole
/// point of the UEFI:NTFS approach (`docs/architecture.md`'s phase 2 guiding
/// decisions) being that this small partition never needs to hold anything
/// bigger than these driver stubs, so the actual `install.wim`/`.esd` can
/// live on NTFS without ever hitting FAT32's 4GB file-size limit.
///
/// [`pbatard/uefi-ntfs`](https://github.com/pbatard/uefi-ntfs) publishes the
/// signed `.efi` driver stubs themselves as release assets, but -- unlike
/// what Argos's own initial phase 2 planning assumed -- does *not* publish a
/// ready-made disk image combining them; Rufus builds that image itself at
/// Rufus-release time. This subcommand is Argos's equivalent: given a
/// directory holding `bootx64_signed.efi`, `bootia32_signed.efi`, and
/// `bootaa64_signed.efi` (downloaded from a `uefi-ntfs` GitHub release),
/// it formats a small FAT image in memory and copies each one to its
/// architecture's standard fallback path (`\EFI\BOOT\BOOT<ARCH>.EFI`) --
/// exactly what a real UEFI firmware looks for on removable media, so the
/// same image boots on x64, IA32, and ARM64 firmware alike. Uses `fatfs`
/// (pure Rust) rather than shelling out to `mkfs.vfat`/`mtools`, since this
/// only ever runs as a one-off developer step, never at Argos's own runtime
/// (which just `dd`s the resulting, already-built image byte-for-byte).
///
/// Re-run this whenever `crates/argos-privileged/assets/PROVENANCE.md`'s
/// pinned `uefi-ntfs` release is updated.
fn vendor_uefi_ntfs(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (efi_dir, output) = match args {
        [efi_dir, output] => (Path::new(efi_dir), Path::new(output)),
        _ => {
            return Err(
                "usage: cargo xtask vendor-uefi-ntfs <dir-with-*_signed.efi> <output.img>".into(),
            )
        }
    };

    // 1.44 MB: the classic floppy-disk size Rufus's own uefi-ntfs.img has
    // long used for exactly this purpose. Comfortably fits three ~20-40KB
    // EFI stubs with room to spare, while staying tiny next to the NTFS
    // partition it sits beside on the target USB stick.
    const IMAGE_SIZE_BYTES: usize = 1_474_560;

    let stubs = [
        ("bootx64_signed.efi", "BOOTX64.EFI"),
        ("bootia32_signed.efi", "BOOTIA32.EFI"),
        ("bootaa64_signed.efi", "BOOTAA64.EFI"),
    ];

    let mut image = Cursor::new(vec![0u8; IMAGE_SIZE_BYTES]);
    fatfs::format_volume(
        &mut image,
        fatfs::FormatVolumeOptions::new().volume_label(*b"UEFI_NTFS  "),
    )?;

    {
        let fs = fatfs::FileSystem::new(&mut image, fatfs::FsOptions::new())?;
        let root = fs.root_dir();
        let efi_boot_dir = root.create_dir("EFI")?.create_dir("BOOT")?;

        for (source_name, dest_name) in stubs {
            let bytes = fs::read(efi_dir.join(source_name))
                .map_err(|e| format!("reading {source_name}: {e}"))?;
            let mut file = efi_boot_dir.create_file(dest_name)?;
            file.write_all(&bytes)?;
        }
    }

    fs::write(output, image.into_inner())?;
    println!("wrote {} ({IMAGE_SIZE_BYTES} bytes)", output.display());
    Ok(())
}
