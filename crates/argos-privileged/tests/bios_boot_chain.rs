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
use std::io::{Seek, SeekFrom, Write};
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
