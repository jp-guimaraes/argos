//! Phase 3 M6.5 (backlog #45): boots Argos-written media under QEMU with a
//! real legacy BIOS (SeaBIOS) and asserts the boot chain actually runs.
//!
//! This exists because boot records are the highest-risk code in the
//! project: a wrong byte produces "no bootable device" on a machine that
//! cannot be single-stepped, and the lab machines this feature is *for* are
//! nowhere near the developer's keyboard. Under emulation the same code can
//! be run, observed and asserted on in under a second.
//!
//! How the chain is observed: the MBR's contract is to find the active
//! partition, load its first sector to 0x7C00, and jump there with `DL` =
//! BIOS drive and `DS:SI` = the active partition entry. A small stub
//! (`asm/vbr_test_stub.asm`) sits where the real FAT32 VBR (M6.4) will go
//! and writes to the serial port what it received, so the MBR can be tested
//! for real before any of M6.4 exists. QEMU redirects COM1 to a file, which
//! is what these tests read.
//!
//! Needs `qemu-system-x86_64` and `nasm` (`brew install qemu nasm`,
//! `apt install qemu-system-x86 nasm`); every test skips itself when they
//! are missing.
//!
//! ```sh
//! cargo test -p argos-privileged --test bios_boot_chain -- --ignored --nocapture
//! ```

use argos_core::partition::windows::{WindowsMbrPlan, SECTOR_SIZE};
use argos_privileged::partition_io::PartitionWindow;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

/// The assembled MBR that ships in the crate -- the same bytes a real write
/// would put on a stick, not a test-only rebuild.
const MBR_CODE: &[u8] = include_bytes!("../asm/mbr.bin");

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn tools_available() -> bool {
    have("qemu-system-x86_64") && have("nasm")
}

/// Assembles `asm/<name>.asm` into `dir`, returning the bytes.
fn assemble(name: &str, dir: &Path) -> Vec<u8> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("asm")
        .join(format!("{name}.asm"));
    let out = dir.join(format!("{name}.bin"));
    let status = Command::new("nasm")
        .args(["-f", "bin"])
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("nasm should run");
    assert!(status.success(), "nasm failed on {}", src.display());
    std::fs::read(&out).expect("assembled output should be readable")
}

/// Builds a bootable disk image: Argos's real MBR code and real partition
/// plan, with `vbr` written at the partition's first sector.
fn build_disk(dir: &Path, vbr: &[u8]) -> std::path::PathBuf {
    let layout = WindowsMbrPlan::new(600_000_000);
    let path = dir.join("disk.img");
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    disk.set_len(layout.total_bytes_required()).unwrap();

    // The partition table, written by the same code the real write path uses.
    argos_privileged::windows_fat32::write_mbr_partition_table_for_test(&mut disk, &layout)
        .unwrap();

    // Our boot code into the bootstrap area, leaving the table untouched.
    disk.seek(SeekFrom::Start(0)).unwrap();
    disk.write_all(MBR_CODE).unwrap();

    // The VBR (here, the reporting stub) at the partition's first sector.
    let (start_lba, _) = layout.partition_sectors().unwrap();
    disk.seek(SeekFrom::Start(u64::from(start_lba) * SECTOR_SIZE))
        .unwrap();
    disk.write_all(vbr).unwrap();
    disk.flush().unwrap();
    path
}

/// Boots `disk` under QEMU with SeaBIOS and returns whatever the guest
/// wrote to COM1, under a wall-clock limit: a working boot sector reaches
/// its `hlt` almost immediately, and a broken one would spin forever.
fn boot_with_timeout(dir: &Path, disk: &Path, seconds: u32) -> String {
    let serial = dir.join("serial.txt");
    let mut child = Command::new("qemu-system-x86_64")
        .args([
            "-machine",
            "pc",
            "-m",
            "64M",
            "-display",
            "none",
            "-no-reboot",
            "-nodefaults",
        ])
        .arg("-drive")
        .arg(format!("format=raw,if=ide,file={}", disk.display()))
        .arg("-serial")
        .arg(format!("file:{}", serial.display()))
        .spawn()
        .expect("qemu should start");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(u64::from(seconds));
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::fs::read_to_string(&serial).unwrap_or_default()
}

#[test]
#[ignore = "needs qemu-system-x86_64 and nasm; see module docs"]
fn the_mbr_finds_the_active_partition_and_hands_off_correctly() {
    if !tools_available() {
        eprintln!("skipping: qemu-system-x86_64 and/or nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let stub = assemble("vbr_test_stub", dir.path());
    let disk = build_disk(dir.path(), &stub);

    let output = boot_with_timeout(dir.path(), &disk, 10);
    eprintln!("serial: {output:?}");

    assert!(
        output.contains("VBR_REACHED"),
        "the MBR never transferred control to the partition's boot sector; serial was {output:?}"
    );
    assert!(
        output.contains("HANDOFF_OK"),
        "control reached the boot sector, but DL (BIOS drive) or DS:SI (active partition entry) \
         was not what the handoff convention requires; serial was {output:?}"
    );
}

/// The negative control: with no active partition, the MBR must say so
/// rather than jumping somewhere arbitrary. Without this, a test that only
/// checks the happy path cannot distinguish "the MBR works" from "the BIOS
/// happened to boot something".
#[test]
#[ignore = "needs qemu-system-x86_64 and nasm; see module docs"]
fn the_mbr_reports_when_no_partition_is_active() {
    if !tools_available() {
        eprintln!("skipping: qemu-system-x86_64 and/or nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let stub = assemble("vbr_test_stub", dir.path());
    let disk = build_disk(dir.path(), &stub);

    // Clear the active flag on every entry.
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk)
            .unwrap();
        for i in 0..4 {
            f.seek(SeekFrom::Start(0x1BE + i * 16)).unwrap();
            f.write_all(&[0x00]).unwrap();
        }
    }

    let output = boot_with_timeout(dir.path(), &disk, 10);
    eprintln!("serial: {output:?}");
    assert!(
        !output.contains("VBR_REACHED"),
        "the MBR jumped to a partition despite none being marked active"
    );
}

/// The committed `asm/mbr.bin` must match what the committed `asm/mbr.asm`
/// assembles to. Users need no assembler because the bytes are checked in;
/// this is what keeps those bytes honest.
#[test]
#[ignore = "needs nasm; see module docs"]
fn the_committed_mbr_binary_matches_its_source() {
    if !have("nasm") {
        eprintln!("skipping: nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fresh = assemble("mbr", dir.path());
    assert_eq!(
        fresh.as_slice(),
        MBR_CODE,
        "asm/mbr.bin is stale -- re-run: nasm -f bin crates/argos-privileged/asm/mbr.asm \
         -o crates/argos-privileged/asm/mbr.bin"
    );
}

/// The bootstrap area is 440 bytes; the partition table starts right after
/// the 4-byte disk signature at 440. Overrunning would corrupt the table.
#[test]
fn the_mbr_code_fits_the_bootstrap_area() {
    assert_eq!(
        MBR_CODE.len(),
        440,
        "the MBR bootstrap area is exactly 440 bytes"
    );
}

/// Size the test's stand-in `bootmgr` is padded to. Deliberately larger than
/// any cluster `fatfs` will choose here, so the file spans several clusters
/// and the VBR has a real chain to follow rather than a single lucky read.
const STUB_BOOTMGR_SIZE: usize = 40960;

/// The marker the stub checks for at its own end -- see
/// `asm/bootmgr_test_stub.asm`.
const END_MARKER: &[u8; 8] = b"ARGOSEND";

/// Builds media the way a real BIOS-mode write would: MBR code and partition
/// table, a genuine FAT32 filesystem written by `fatfs`, our VBR installed
/// over its boot sector, and `bootmgr` present as a file in the root
/// directory.
fn build_bios_media(dir: &Path, bootmgr: &[u8]) -> std::path::PathBuf {
    build_bios_media_with_vbr(dir, bootmgr, None)
}

/// As [`build_bios_media`], but optionally installing `vbr_override` (the
/// diagnostic build) instead of the shipped boot record.
fn build_bios_media_with_vbr(
    dir: &Path,
    bootmgr: &[u8],
    vbr_override: Option<&[u8]>,
) -> std::path::PathBuf {
    let layout = WindowsMbrPlan::new(600_000_000);
    let path = dir.join("bios.img");
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    disk.set_len(layout.total_bytes_required()).unwrap();

    argos_privileged::windows_fat32::write_mbr_partition_table_for_test(&mut disk, &layout)
        .unwrap();
    disk.seek(SeekFrom::Start(0)).unwrap();
    disk.write_all(MBR_CODE).unwrap();

    {
        let mut window = PartitionWindow::new(&mut disk, layout.windows_partition);
        fatfs::format_volume(
            &mut window,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"ARGOS-WIN  "),
        )
        .unwrap();

        window.seek(SeekFrom::Start(0)).unwrap();
        let fs = fatfs::FileSystem::new(&mut window, fatfs::FsOptions::new()).unwrap();
        {
            let mut file = fs.root_dir().create_file("bootmgr").unwrap();
            file.write_all(bootmgr).unwrap();
            file.flush().unwrap();
        }
        fs.unmount().unwrap();

        // After the filesystem exists, not before: installing the VBR is what
        // replaces fatfs's boot sector code while keeping its BPB.
        match vbr_override {
            None => argos_privileged::windows_fat32::install_fat32_vbr_for_test(
                &mut window,
                layout.partition_sectors().unwrap().0,
            )
            .unwrap(),
            Some(code) => {
                // Same merge the installer does: keep the BPB, replace the
                // jump and the code.
                let mut sector = [0u8; 512];
                window.seek(SeekFrom::Start(0)).unwrap();
                window.read_exact(&mut sector).unwrap();
                sector[..3].copy_from_slice(&code[..3]);
                sector[90..].copy_from_slice(&code[90..]);
                // The same hidden-sectors patch the real installer applies.
                sector[0x1C..0x20]
                    .copy_from_slice(&layout.partition_sectors().unwrap().0.to_le_bytes());
                window.seek(SeekFrom::Start(0)).unwrap();
                window.write_all(&sector).unwrap();
                window.flush().unwrap();
            }
        }
    }
    disk.flush().unwrap();
    path
}

/// Pads the assembled stub to [`STUB_BOOTMGR_SIZE`] and stamps the end
/// marker, so the stub can prove the whole file reached memory.
fn stub_bootmgr(dir: &Path) -> Vec<u8> {
    let mut content = assemble("bootmgr_test_stub", dir);
    assert!(
        content.len() < STUB_BOOTMGR_SIZE - END_MARKER.len(),
        "the stub outgrew the file it is padded into"
    );
    content.resize(STUB_BOOTMGR_SIZE - END_MARKER.len(), 0);
    content.extend_from_slice(END_MARKER);
    assert_eq!(content.len(), STUB_BOOTMGR_SIZE);
    content
}

/// The M6.4 acceptance test: a full legacy-BIOS boot, from the MBR through
/// the FAT32 VBR into `bootmgr`, on media built exactly as a real write
/// builds it.
#[test]
#[ignore = "needs qemu-system-x86_64 and nasm; see module docs"]
fn the_full_chain_boots_from_mbr_through_the_vbr_into_bootmgr() {
    if !tools_available() {
        eprintln!("skipping: qemu-system-x86_64 and/or nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let disk = build_bios_media(dir.path(), &stub_bootmgr(dir.path()));

    let output = boot_with_timeout(dir.path(), &disk, 20);
    eprintln!("serial: {output:?}");

    assert!(
        output.contains("BOOTMGR_LOADED"),
        "the VBR never reached bootmgr: it either failed to find the entry in the root \
         directory or never transferred control; serial was {output:?}"
    );
    assert!(
        !output.contains("TRUNCATED"),
        "bootmgr was entered but the file was not fully in memory -- the VBR lost the \
         cluster chain partway; serial was {output:?}"
    );
    assert!(
        output.contains("FULL_LOAD_OK"),
        "the whole-file check did not pass; serial was {output:?}"
    );
}

/// Negative control: with no `bootmgr` in the root directory, the VBR must
/// say so rather than jumping into whatever happens to sit at 0x20000.
#[test]
#[ignore = "needs qemu-system-x86_64 and nasm; see module docs"]
fn the_vbr_reports_when_bootmgr_is_missing() {
    if !tools_available() {
        eprintln!("skipping: qemu-system-x86_64 and/or nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let layout = WindowsMbrPlan::new(600_000_000);
    let path = dir.path().join("empty.img");
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    disk.set_len(layout.total_bytes_required()).unwrap();
    argos_privileged::windows_fat32::write_mbr_partition_table_for_test(&mut disk, &layout)
        .unwrap();
    disk.seek(SeekFrom::Start(0)).unwrap();
    disk.write_all(MBR_CODE).unwrap();
    {
        let mut window = PartitionWindow::new(&mut disk, layout.windows_partition);
        fatfs::format_volume(
            &mut window,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"ARGOS-WIN  "),
        )
        .unwrap();
        argos_privileged::windows_fat32::install_fat32_vbr_for_test(
            &mut window,
            layout.partition_sectors().unwrap().0,
        )
        .unwrap();
    }
    disk.flush().unwrap();

    let output = boot_with_timeout(dir.path(), &path, 15);
    eprintln!("serial: {output:?}");
    assert!(
        !output.contains("BOOTMGR_LOADED"),
        "the VBR jumped to 0x20000 with no bootmgr on the volume"
    );
}

/// The committed `asm/vbr_fat32.bin` must match its source, same as the MBR's.
#[test]
#[ignore = "needs nasm; see module docs"]
fn the_committed_vbr_binary_matches_its_source() {
    if !have("nasm") {
        eprintln!("skipping: nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fresh = assemble("vbr_fat32", dir.path());
    let committed: &[u8] = include_bytes!("../asm/vbr_fat32.bin");
    assert_eq!(
        fresh.as_slice(),
        committed,
        "asm/vbr_fat32.bin is stale -- re-run: nasm -f bin \
         crates/argos-privileged/asm/vbr_fat32.asm -o crates/argos-privileged/asm/vbr_fat32.bin"
    );
}

/// Diagnostic run: installs the `-DSERIAL_DIAG` build of the VBR, which
/// reports progress and failures over the serial port. Not an assertion --
/// it exists so a failing boot says *where* it stopped instead of leaving an
/// empty log, which is otherwise indistinguishable from never having run.
///
/// Markers: `G` geometry computed, `F` bootmgr's directory entry found,
/// `L` whole file loaded; `N` bootmgr not found, `R` disk read error.
#[test]
#[ignore = "diagnostic aid, not an assertion; needs qemu and nasm"]
fn diagnose_the_vbr_over_serial() {
    if !tools_available() {
        eprintln!("skipping: qemu-system-x86_64 and/or nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("asm/vbr_fat32.asm");
    let out = dir.path().join("vbr_diag.bin");
    assert!(Command::new("nasm")
        .args(["-f", "bin", "-DSERIAL_DIAG"])
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("nasm should run")
        .success());
    let mut diag = std::fs::read(&out).unwrap();
    assert!(
        diag.len() <= 510,
        "the diagnostic build outgrew a boot sector"
    );
    diag.resize(510, 0);
    diag.extend_from_slice(&[0x55, 0xAA]);

    let disk = build_bios_media_with_vbr(dir.path(), &stub_bootmgr(dir.path()), Some(&diag));
    let output = boot_with_timeout(dir.path(), &disk, 20);
    eprintln!("VBR diagnostic serial output: {output:?}");
}

/// The one that matters most: media produced by the **real write path**
/// (`TargetLayout::MbrBios`, the same code `argos write --layout fat32-bios`
/// runs) must boot. Everything above tests the boot records against media the
/// test itself assembled; this tests them against media the product made.
///
/// The fixture ISO's `bootmgr` is a placeholder, so after the write its
/// *contents* are replaced with the reporting stub -- the partition table,
/// the MBR boot code, the VBR and the directory layout all remain exactly
/// what the write path produced, which is what is under test.
#[test]
#[ignore = "needs qemu-system-x86_64 and nasm; see module docs"]
fn media_from_the_real_write_path_boots_under_bios() {
    if !tools_available() {
        eprintln!("skipping: qemu-system-x86_64 and/or nasm not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    let iso_path = dir.path().join("fixture.iso");
    std::fs::write(
        &iso_path,
        argos_core::image::windows::fixtures::udf_windows_installer_iso(true, true),
    )
    .unwrap();

    let iso = argos_core::image::windows::WindowsIso::open(&iso_path).unwrap();
    let files = iso.list_files().unwrap();
    let actions = argos_privileged::windows_fat32::plan_copy_actions(&iso, &files).unwrap();
    let layout = argos_privileged::windows_fat32::TargetLayout::for_layout(
        argos_privileged::protocol::WindowsLayout::Fat32Bios,
        argos_privileged::windows_fat32::total_bytes_on_target(&actions),
    );

    let disk_path = dir.path().join("real.img");
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&disk_path)
        .unwrap();
    disk.set_len(layout.total_bytes_required()).unwrap();

    argos_privileged::windows_fat32::write_fat32_media_for_test(
        &mut disk,
        &layout,
        &iso,
        &actions,
        &argos_core::progress::NoopProgress,
    )
    .expect("the real write path should produce BIOS media");

    // Swap in a bootmgr that can report over serial.
    {
        let mut window = PartitionWindow::new(&mut disk, layout.region());
        window.seek(SeekFrom::Start(0)).unwrap();
        let fs = fatfs::FileSystem::new(&mut window, fatfs::FsOptions::new()).unwrap();
        {
            let mut f = fs.root_dir().open_file("bootmgr").unwrap();
            f.truncate().unwrap();
            f.write_all(&stub_bootmgr(dir.path())).unwrap();
            f.flush().unwrap();
        }
        fs.unmount().unwrap();
    }
    disk.flush().unwrap();
    drop(disk);

    let output = boot_with_timeout(dir.path(), &disk_path, 20);
    eprintln!("serial: {output:?}");
    assert!(
        output.contains("BOOTMGR_LOADED"),
        "media built by the real write path did not reach bootmgr; serial was {output:?}"
    );
    assert!(
        output.contains("FULL_LOAD_OK"),
        "bootmgr was entered but not fully loaded; serial was {output:?}"
    );
}
